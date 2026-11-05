//! OpenNebula и ПК СВ «Брест» (XML-RPC фронтенда + libvirt узла).
//!
//! Копия: описание ВМ (one.vm.info) и диски — через libvirt узла, на котором ВМ размещена (домен one-<id>,
//! внешний снимок дисков без остановки). Восстановление: диски (цепочка qcow2 собирается в один образ)
//! кладутся во временный каталог фронтенда, регистрируются образами в хранилище (one.image.allocate),
//! создаётся шаблон по описанию ВМ с новыми образами (one.template.allocate) и ВМ из него.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::libvirt::Libvirt;
use super::xml::{self, esc};
use super::{q, vm_stream, Driver, Host, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct OpenNebula { c: reqwest::Client, url: String, session: String, cfg: Value }

impl OpenNebula {
    pub fn new(cfg: &Value) -> Result<OpenNebula> {
        let url = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес XML-RPC фронтенда (http://frontend:2633/RPC2)"))?;
        Ok(OpenNebula { c: super::http(cfg)?, url, session: format!("{}:{}", cfg_str(cfg, "user").unwrap_or_else(|| "oneadmin".into()), cfg_str(cfg, "password").unwrap_or_default()), cfg: cfg.clone() })
    }
    /// Вызов: ответ [успех, значение, код].
    async fn call(&self, method: &str, args: &[Value]) -> Result<Value> {
        let mut params = vec![json!(self.session)];
        params.extend(args.iter().cloned());
        let r = self.c.post(&self.url).header("Content-Type", "text/xml").body(xml::rpc_request(method, &params)).send().await.with_context(|| format!("OpenNebula {}", self.url))?;
        let v = xml::rpc_response(&r.text().await?)?;
        if v[0] != true { bail!("{method}: {}", v[1].as_str().unwrap_or("")); }
        Ok(v[1].clone())
    }
    async fn vm_xml(&self, id: &str) -> Result<(String, xml::Node)> {
        let id_n: i64 = id.parse().context("id ВМ OpenNebula — число")?;
        let s = self.call("one.vm.info", &[json!(id_n)]).await?.as_str().unwrap_or("").to_string();
        let d = xml::parse(&s)?;
        Ok((s, d))
    }
    /// Узел ВМ: последняя запись истории размещения.
    fn host_of(d: &xml::Node) -> Option<String> {
        let mut hs = Vec::new();
        d.find_all("HISTORY", &mut hs);
        hs.last().map(|h| h.t("HOSTNAME")).filter(|s| !s.is_empty())
    }
    async fn node(&self, hostname: &str, log: &LogFn) -> Result<Host> {
        let me = std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
        if hostname == "localhost" || hostname == "127.0.0.1" || hostname == me { return Ok(Host::Local); }
        Host::connect(&self.cfg, Some(hostname), log).await
    }
}

#[async_trait]
impl Driver for OpenNebula {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let s = match self.call("one.vmpool.infoextended", &[json!(-2), json!(-1), json!(-1), json!(-1)]).await { Ok(v) => v, Err(_) => self.call("one.vmpool.info", &[json!(-2), json!(-1), json!(-1), json!(-1)]).await? };
        let d = xml::parse(s.as_str().unwrap_or(""))?;
        let mut vms = Vec::new();
        d.find_all("VM", &mut vms);
        Ok(vms.into_iter().map(|v| VmRef { id: v.t("ID"), name: v.t("NAME"), state: match v.t("STATE").as_str() { "3" => "running".into(), "8" => "poweroff".into(), "9" => "undeployed".into(), s => format!("state {s}") }, extra: json!({ "host": Self::host_of(v) }) }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> {
        let (text, d) = self.vm_xml(&vm.id).await?;
        super::super::appkit::add_bytes(w, &format!("{prefix}vm.xml"), text.into_bytes()).await?;
        let host = Self::host_of(&d).ok_or_else(|| anyhow!("ВМ не размещена на узле (UNDEPLOYED?) — разверните её или копируйте образы хранилища"))?;
        if vm.state == "undeployed" { bail!("ВМ выгружена с узла (UNDEPLOYED)"); }
        log(&format!("  узел {host}, домен one-{}", vm.id));
        let lv = Libvirt::new(self.node(&host, log).await?, cfg);
        let mut m = lv.backup_domain(&format!("one-{}", vm.id), w, prefix, cfg, log).await?;
        m["kind"] = json!("opennebula");
        m["host"] = json!(host);
        Ok(m)
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let name = cfg_str(p, "new_name").unwrap_or_else(|| format!("{}-restored", vm["name"].as_str().unwrap_or("vm")));
        let orig = String::from_utf8(super::super::appkit::read_small(repo, vm_stream(m, vm, "vm.xml")?).await?)?;
        let od = xml::parse(&orig)?;
        let tpl = od.find("TEMPLATE").cloned().unwrap_or_default();
        // Файлы — на фронтенде (агент на нём или «Узел по SSH»), откуда хранилище их забирает.
        let front = Host::connect(p, None, log).await?;
        let dir = format!("/var/tmp/hopebackup-one-{}", crate::util::short_id());
        front.sh(&format!("mkdir -p {dir} && chmod 755 {dir}"), log).await?;
        let ds: i64 = cfg_str(p, "storage").and_then(|s| s.parse().ok()).unwrap_or(1);
        let res = async {
            let mut images = Vec::new();
            for (i, dsk) in vm["disks"].as_array().cloned().unwrap_or_default().iter().enumerate() {
                let layers = dsk["layers"].as_array().cloned().unwrap_or_default();
                let mut prev: Option<(String, String)> = None;
                let mut top = String::new();
                for (j, l) in layers.iter().enumerate() {
                    let fmt = l["format"].as_str().unwrap_or("raw").to_string();
                    let f = format!("{dir}/d{i}.{j}.{fmt}");
                    front.stream_in(repo, vm_stream(m, vm, l["stream"].as_str().unwrap_or(""))?, &format!("cat > {}", q(&f)), log).await?;
                    if let Some((pf, pfmt)) = &prev { front.sh(&format!("qemu-img rebase -u -F {pfmt} -b {} {}", q(pf), q(&f)), log).await?; }
                    prev = Some((f.clone(), fmt));
                    top = f;
                }
                // Один самостоятельный образ (без базы) — для регистрации в хранилище.
                let flat = format!("{dir}/disk{i}.qcow2");
                front.sh(&format!("qemu-img convert -O qcow2 {} {}", q(&top), q(&flat)), log).await.context("сборка образа диска")?;
                front.sh(&format!("chmod 644 {}", q(&flat)), log).await?;
                let it = format!("NAME=\"{}-disk{i}\"\nPATH=\"{flat}\"\nTYPE=\"{}\"\nFORMAT=\"qcow2\"\nPERSISTENT=\"YES\"\n", name.replace('"', ""), if i == 0 { "OS" } else { "DATABLOCK" });
                let id = self.call("one.image.allocate", &[json!(it), json!(ds)]).await.context("регистрация образа")?;
                let id = id.as_i64().unwrap_or(-1);
                super::poll(7200, 3, "образ OpenNebula", || async {
                    let s = self.call("one.image.info", &[json!(id)]).await?;
                    let d = xml::parse(s.as_str().unwrap_or(""))?;
                    match d.find("IMAGE").map(|x| x.t("STATE")).unwrap_or_default().as_str() { "1" => Ok(Some(json!(id))), "5" => bail!("образ {id}: ошибка"), _ => Ok(None) }
                }).await?;
                images.push(id);
            }
            Ok::<_, anyhow::Error>(images)
        }.await;
        let _ = front.sh(&format!("rm -rf {dir}"), log).await;
        let images = res?;
        let mut t = format!("NAME=\"{}\"\n", name.replace('"', ""));
        for k in ["CPU", "VCPU", "MEMORY"] { let v = tpl.t(k); if !v.is_empty() { t.push_str(&format!("{k}=\"{v}\"\n")); } }
        for img in &images { t.push_str(&format!("DISK=[IMAGE_ID=\"{img}\"]\n")); }
        for nic in tpl.all("NIC") {
            let net = match cfg_str(p, "network") { Some(n) => format!("NETWORK=\"{}\"", esc(&n)), None => format!("NETWORK_ID=\"{}\"", nic.t("NETWORK_ID")) };
            t.push_str(&format!("NIC=[{net}]\n"));
        }
        if let Some(g) = tpl.child("GRAPHICS") { t.push_str(&format!("GRAPHICS=[TYPE=\"{}\",LISTEN=\"0.0.0.0\"]\n", g.t("TYPE"))); }
        if let Some(o) = tpl.child("OS") { let a = o.t("ARCH"); if !a.is_empty() { t.push_str(&format!("OS=[ARCH=\"{a}\"]\n")); } }
        let tid = self.call("one.template.allocate", &[json!(t)]).await.context("шаблон ВМ")?.as_i64().unwrap_or(-1);
        let hold = !super::super::appkit::bool_param(p, "start", false);
        let vid = self.call("one.template.instantiate", &[json!(tid), json!(name), json!(hold), json!(""), json!(false)]).await.context("создание ВМ из шаблона")?;
        log(&format!("  ВМ {name} создана (id {vid}, шаблон {tid}, образы {images:?}){}", if hold { ", в состоянии HOLD" } else { "" }));
        Ok(json!({ "vm": name, "id": vid, "template": tid, "images": images }))
    }
}
