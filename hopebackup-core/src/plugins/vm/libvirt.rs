//! libvirt / KVM (локально или на узле по SSH; используется и для узлов OpenNebula, SpaceVM, VMmanager).
//!
//! Работающая ВМ: внешний снимок дисков (virsh snapshot-create-as --disk-only, с заморозкой ФС через
//! гостевой агент, если он есть) — исходные файлы дисков на время копии только читаются, запись идёт в
//! оверлеи; затем blockcommit --active --pivot сливает оверлеи обратно. Выключенная — файлы как есть.
//! Цепочки qcow2 (диск на базе шаблона) копируются послойно и при восстановлении собираются заново.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::xml;
use super::{q, safe, vm_stream, Driver, Host, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Libvirt { pub host: Host, pub uri: String }

#[derive(Debug, Clone)]
pub struct Disk { pub target: String, pub source: String, pub kind: String, pub format: String }

/// Диски ВМ из описания (только устройства device="disk" с источником-файлом или блочным устройством).
pub fn disks(dom: &xml::Node) -> Vec<Disk> {
    let mut out = Vec::new();
    let Some(devs) = dom.find("devices") else { return out };
    for d in devs.all("disk") {
        if d.attr("device").unwrap_or("disk") != "disk" || d.child("readonly").is_some() || d.child("shareable").is_some() { continue; }
        let kind = d.attr("type").unwrap_or("file").to_string();
        let Some(src) = d.child("source") else { continue };
        let source = match kind.as_str() { "file" => src.attr("file"), "block" => src.attr("dev"), _ => None }.unwrap_or("").to_string();
        if source.is_empty() { continue; }
        let target = d.child("target").and_then(|t| t.attr("dev")).unwrap_or("").to_string();
        let format = d.child("driver").and_then(|x| x.attr("type")).unwrap_or("raw").to_string();
        out.push(Disk { target, source, kind, format });
    }
    out
}

impl Libvirt {
    pub fn new(host: Host, cfg: &Value) -> Libvirt { Libvirt { host, uri: cfg_str(cfg, "libvirt_uri").unwrap_or_else(|| "qemu:///system".into()) } }

    pub async fn virsh(&self, args: &str, log: &LogFn) -> Result<String> { self.host.sh(&format!("virsh -q -c {} {args}", q(&self.uri)), log).await }

    /// Слои диска: от базы к верхнему (qemu-img info --backing-chain).
    async fn chain(&self, d: &Disk, log: &LogFn) -> Result<Vec<(String, String)>> {
        if d.kind == "block" { return Ok(vec![(d.source.clone(), d.format.clone())]); }
        let o = self.host.sh(&format!("qemu-img info -U --backing-chain --output=json {}", q(&d.source)), log).await.context("qemu-img info")?;
        let v: Value = serde_json::from_str(&o).context("ответ qemu-img info")?;
        let arr = if v.is_array() { v.as_array().cloned().unwrap_or_default() } else { vec![v] };
        let mut layers: Vec<(String, String)> = arr.iter().map(|x| (x["filename"].as_str().unwrap_or("").to_string(), x["format"].as_str().unwrap_or("raw").to_string())).collect();
        layers.reverse();
        Ok(layers)
    }

    pub async fn backup_domain(&self, name: &str, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> {
        let xml_text = self.virsh(&format!("dumpxml --security-info {}", q(name)), log).await?;
        super::super::appkit::add_bytes(w, &format!("{prefix}domain.xml"), xml_text.clone().into_bytes()).await?;
        let dom = xml::parse(&xml_text)?;
        let ds = disks(&dom);
        if ds.is_empty() { log("  у ВМ нет дисков-файлов или блочных устройств"); }
        let state = self.virsh(&format!("domstate {}", q(name)), log).await.unwrap_or_default().trim().to_string();
        let running = state == "running" || state == "paused";
        let tag = format!("hb{}", &crate::util::short_id()[..8]);
        let mut overlays: Vec<(String, String)> = Vec::new();
        if running && !ds.is_empty() {
            let specs: String = ds.iter().map(|d| format!(" --diskspec {},snapshot=external,file={}", q(&d.target), q(&format!("{}.{tag}", d.source)))).collect();
            let base = format!("snapshot-create-as {} {tag} --disk-only --atomic --no-metadata{specs}", q(name));
            let quiesce = super::super::appkit::bool_param(cfg, "quiesce", true);
            let r = if quiesce { self.virsh(&format!("{base} --quiesce"), log).await } else { Err(anyhow!("без заморозки")) };
            if let Err(e) = r {
                if quiesce { log(&format!("  заморозка ФС гостя недоступна ({}) — снимок «как при сбое питания»", e.to_string().lines().last().unwrap_or(""))); }
                self.virsh(&base, log).await.context("внешний снимок дисков")?;
            } else { log("  ФС гостя заморожена на время снимка (qemu-guest-agent)"); }
            overlays = ds.iter().map(|d| (d.target.clone(), format!("{}.{tag}", d.source))).collect();
        }
        // Копия слоёв; оверлеи сливаются обратно в любом случае.
        let res = async {
            let mut out = Vec::new();
            for d in &ds {
                let layers = self.chain(d, log).await?;
                let mut lm = Vec::new();
                for (i, (file, fmt)) in layers.iter().enumerate() {
                    let sname = format!("{prefix}disk-{}.{i}.{fmt}", safe(&d.target));
                    let cmd = if d.kind == "block" { format!("dd if={} bs=4M status=none", q(file)) } else { format!("cat {}", q(file)) };
                    let e = self.host.stream_out(w, &sname, &cmd, log).await?;
                    lm.push(json!({ "stream": sname.trim_start_matches(prefix), "format": fmt, "file": file, "size": e.size }));
                }
                out.push(json!({ "target": d.target, "source": d.source, "type": d.kind, "format": d.format, "layers": lm }));
            }
            Ok::<_, anyhow::Error>(out)
        }.await;
        for (target, ov) in &overlays {
            if let Err(e) = self.virsh(&format!("blockcommit {} {} --shallow --active --pivot --wait", q(name), q(target)), log).await {
                log(&format!("! слияние оверлея {ov} ({target}) не удалось: {e:#} — выполните: virsh blockcommit {name} {target} --shallow --active --pivot"));
                continue;
            }
            let _ = self.host.sh(&format!("rm -f {}", q(ov)), log).await;
        }
        let ds_meta = res?;
        Ok(json!({ "kind": "libvirt", "domain": name, "running": running, "disks": ds_meta }))
    }

    /// Восстановить домен из копии: диски в каталог `storage` (или туда, где были), описание с новым именем.
    pub async fn restore_domain(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let old = vm["domain"].as_str().or(vm["name"].as_str()).unwrap_or("").to_string();
        let new_name = cfg_str(p, "new_name");
        let name = new_name.clone().unwrap_or_else(|| old.clone());
        let replace = super::super::appkit::bool_param(p, "replace", false);
        let exists = self.virsh(&format!("domstate {}", q(&name)), log).await.is_ok();
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        if exists {
            if !replace { bail!("ВМ «{name}» уже есть: задайте новое имя или включите замену"); }
            let _ = self.virsh(&format!("destroy {}", q(&name)), log).await;
            if self.virsh(&format!("undefine {} --nvram", q(&name)), log).await.is_err() { self.virsh(&format!("undefine {}", q(&name)), log).await?; }
            log(&format!("  прежняя ВМ {name} удалена из libvirt (диски остаются рядом)"));
        }
        let xml_s = vm_stream(m, vm, "domain.xml")?;
        let mut xml_text = String::from_utf8(super::super::appkit::read_small(repo, xml_s).await?)?;
        let storage = cfg_str(p, "storage");
        let mut moved = Vec::new();
        for d in vm["disks"].as_array().cloned().unwrap_or_default() {
            let target = d["target"].as_str().unwrap_or("");
            let source = d["source"].as_str().unwrap_or("").to_string();
            let dir = storage.clone().unwrap_or_else(|| std::path::Path::new(&source).parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "/var/lib/libvirt/images".into()));
            let layers = d["layers"].as_array().cloned().unwrap_or_default();
            let mut prev: Option<(String, String)> = None;
            let mut top = String::new();
            for (i, l) in layers.iter().enumerate() {
                let fmt = l["format"].as_str().unwrap_or("raw").to_string();
                let last = i + 1 == layers.len();
                // Верхний слой — имя диска (как было, если имя ВМ не меняется); базы — рядом с суффиксом.
                let file = if d["type"] == "block" && new_name.is_none() && storage.is_none() { source.clone() }
                    else if last && new_name.is_none() { format!("{dir}/{}", std::path::Path::new(&source).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| format!("{target}.{fmt}"))) }
                    else if last { format!("{dir}/{}-{}.{fmt}", safe(&name), safe(target)) }
                    else { format!("{dir}/{}-{}.base{i}.{fmt}", safe(&name), safe(target)) };
                if d["type"] != "block" {
                    if self.host.sh(&format!("test -e {}", q(&file)), log).await.is_ok() {
                        let aside = format!("{file}.before-restore-{ts}");
                        self.host.sh(&format!("mv {} {}", q(&file), q(&aside)), log).await?;
                        moved.push(aside);
                    }
                    self.host.sh(&format!("mkdir -p {}", q(&dir)), log).await?;
                }
                let s = vm_stream(m, vm, l["stream"].as_str().unwrap_or(""))?;
                let cmd = if d["type"] == "block" { format!("dd of={} bs=4M status=none conv=fsync", q(&file)) } else { format!("cat > {}", q(&file)) };
                self.host.stream_in(repo, s, &cmd, log).await?;
                if let Some((pf, pfmt)) = &prev { self.host.sh(&format!("qemu-img rebase -u -F {pfmt} -b {} {}", q(pf), q(&file)), log).await.context("сборка цепочки qcow2")?; }
                prev = Some((file.clone(), fmt));
                top = file;
            }
            if top != source {
                xml_text = xml_text.replace(&format!("'{}'", xml::esc(&source)), &format!("'{}'", xml::esc(&top))).replace(&format!("\"{}\"", xml::esc(&source)), &format!("\"{}\"", xml::esc(&top)));
            }
            let _ = self.host.sh(&format!("chown --reference={} {} 2>/dev/null || true", q(&dir), q(&top)), log).await;
        }
        if new_name.is_some() {
            xml_text = xml_text.replacen(&format!("<name>{}</name>", xml::esc(&old)), &format!("<name>{}</name>", xml::esc(&name)), 1);
            xml_text = strip_tag(&xml_text, "uuid");
            xml_text = xml_text.lines().filter(|l| !l.trim_start().starts_with("<mac address=")).collect::<Vec<_>>().join("\n");
        }
        let tmp = format!("/tmp/hopebackup-{}.xml", crate::util::short_id());
        self.host.write(&tmp, xml_text.as_bytes(), log).await?;
        let r = self.virsh(&format!("define {}", q(&tmp)), log).await;
        let _ = self.host.sh(&format!("rm -f {}", q(&tmp)), log).await;
        r.context("virsh define")?;
        if super::super::appkit::bool_param(p, "start", false) { self.virsh(&format!("start {}", q(&name)), log).await?; }
        log(&format!("  ВМ {name} определена в libvirt ({})", self.host.label()));
        Ok(json!({ "vm": name, "moved_aside": moved }))
    }
}

fn strip_tag(s: &str, tag: &str) -> String {
    let (o, c) = (format!("<{tag}>"), format!("</{tag}>"));
    match (s.find(&o), s.find(&c)) { (Some(a), Some(b)) if b > a => format!("{}{}", &s[..a], &s[b + c.len()..]), _ => s.to_string() }
}

#[async_trait]
impl Driver for Libvirt {
    async fn list(&self, log: &LogFn) -> Result<Vec<VmRef>> {
        let names = self.virsh("list --all --name", log).await?;
        let mut out = Vec::new();
        for n in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
            let id = self.virsh(&format!("domuuid {}", q(n)), log).await.unwrap_or_default().trim().to_string();
            let st = self.virsh(&format!("domstate {}", q(n)), log).await.unwrap_or_default().trim().to_string();
            out.push(VmRef { id, name: n.to_string(), state: st, extra: Value::Null });
        }
        Ok(out)
    }
    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> { self.backup_domain(&vm.name, w, prefix, cfg, log).await }
    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> { self.restore_domain(repo, m, vm, p, log).await }
}
