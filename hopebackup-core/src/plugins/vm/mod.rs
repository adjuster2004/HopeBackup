//! Образы ВМ целиком: конфигурация и диски виртуальных машин гипервизора — потоком в репозиторий
//! (дедупликация по содержимому: неизменённые блоки дисков между копиями не передаются повторно).
//!
//! Платформы: libvirt/KVM; Proxmox VE (vzdump --stdout); oVirt / zVirt / РОСА Виртуализация (API резервного
//! копирования и imageio); OpenNebula / ПК СВ «Брест» (XML-RPC + libvirt узла); OpenStack (Nova/Glance/Cinder);
//! VMware vSphere (снимок + экспорт OVF/NFC, учёт CBT); XCP-ng / Citrix Hypervisor (XAPI, XVA); Nutanix AHV
//! (Prism v3, образы дисков); SpaceVM и VMmanager 6 (API платформы + libvirt узла).
//!
//! Восстановление: на ту же или другую платформу того же типа (новое имя, хранилище, сеть) либо файлами в
//! каталог (диски и описание — для импорта вручную или на другую платформу).

pub mod libvirt;
pub mod nutanix;
pub mod opennebula;
pub mod openstack;
pub mod ovirt;
pub mod proxmox;
pub mod restapi;
pub mod vmware;
pub mod xen;
pub mod xml;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use super::appkit::{self as kit, Cmd};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::{Manifest, StreamEntry};

pub struct VmImage;

pub const PLATFORMS: &[(&str, &str)] = &[
    ("libvirt", "libvirt / KVM"),
    ("proxmox", "Proxmox VE"),
    ("ovirt", "oVirt, zVirt, РОСА Виртуализация, HOSTVM"),
    ("opennebula", "OpenNebula, ПК СВ «Брест»"),
    ("openstack", "OpenStack"),
    ("vmware", "VMware vSphere (vCenter, ESXi)"),
    ("xen", "XCP-ng, Citrix Hypervisor / XenServer"),
    ("nutanix", "Nutanix AHV (Prism)"),
    ("spacevm", "SpaceVM"),
    ("vmmanager", "VMmanager 6"),
];

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "platform": { "type": "string", "title": "Платформа", "enum": PLATFORMS.iter().map(|p| p.0).collect::<Vec<_>>(), "default": "libvirt",
            "description": PLATFORMS.iter().map(|(a, b)| format!("{a} — {b}")).collect::<Vec<_>>().join("; ") },
        "vms": { "type": "array", "items": { "type": "string" }, "title": "Виртуальные машины", "description": "Имена или идентификаторы; пусто — все (только если включено «Все ВМ»)" },
        "all_vms": { "type": "boolean", "title": "Все ВМ платформы", "default": false },
        "url": { "type": "string", "title": "Адрес API", "description": "oVirt: https://engine/ovirt-engine/api; OpenStack: Keystone https://…:5000/v3; vSphere: https://vcenter; XAPI: https://мастер-пула; Nutanix: https://prism:9440; SpaceVM: https://контроллер; VMmanager: https://vmmanager; OpenNebula: http://frontend:2633/RPC2" },
        "user": { "type": "string", "title": "Пользователь API" },
        "password": { "type": "string", "format": "secret", "title": "Пароль API (ключ интеграции SpaceVM, секрет токена Proxmox)" },
        "fingerprint": { "type": "string", "title": "Отпечаток сертификата API", "description": "SHA-256; пусто — системные CA" },
        "insecure": { "type": "boolean", "title": "Не проверять сертификат API", "default": false },
        "project": { "type": "string", "title": "OpenStack: проект", "default": "admin" },
        "domain": { "type": "string", "title": "OpenStack: домен", "default": "Default" },
        "region": { "type": "string", "title": "OpenStack: регион" },
        "ssh_host": { "type": "string", "title": "Узел по SSH", "description": "libvirt, Proxmox, узлы OpenNebula/SpaceVM/VMmanager; пусто — эта машина (агент на гипервизоре)" },
        "ssh_port": { "type": "integer", "title": "Порт SSH", "default": 22 },
        "ssh_user": { "type": "string", "title": "Пользователь SSH", "default": "root" },
        "ssh_password": { "type": "string", "format": "secret", "title": "Пароль SSH" },
        "ssh_key": { "type": "string", "format": "secret-textarea", "title": "Закрытый ключ SSH" },
        "ssh_fingerprint": { "type": "string", "title": "Отпечаток ключа узла SSH", "description": "SHA256:…; пусто — запомнить при первом подключении" },
        "libvirt_uri": { "type": "string", "title": "Адрес libvirt", "default": "qemu:///system" },
        "quiesce": { "type": "boolean", "title": "Согласованность через гостевой агент", "default": true, "description": "Заморозка ФС гостя на время снимка (qemu-guest-agent, VMware Tools); без агента — снимок «как при сбое питания»" },
        "cbt": { "type": "boolean", "title": "vSphere: включить CBT и считать изменённые блоки", "default": true }
    } })
}

pub fn defaults() -> Value { json!({ "platform": "libvirt", "libvirt_uri": "qemu:///system", "ssh_port": 22, "ssh_user": "root", "quiesce": true, "cbt": true, "all_vms": false }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "vm": { "type": "string", "title": "ВМ из копии", "description": "Имя или идентификатор; пусто — все ВМ копии" },
        "new_name": { "type": "string", "title": "Новое имя", "description": "Восстановить рядом с рабочей под этим именем (для одной ВМ)" },
        "replace": { "type": "boolean", "title": "Заменить существующую ВМ с тем же именем", "default": false, "description": "Прежняя ВМ останавливается и удаляется из гипервизора; её диски (libvirt) остаются рядом" },
        "storage": { "type": "string", "title": "Хранилище", "description": "libvirt — каталог дисков; Proxmox — storage; oVirt — домен хранения; vSphere — datastore; XAPI — SR; Nutanix — кластер" },
        "node": { "type": "string", "title": "Узел / кластер / хост", "description": "Proxmox — узел; oVirt — кластер; vSphere — хост ESXi или ресурсный пул" },
        "network": { "type": "string", "title": "Сеть", "description": "oVirt — профиль vNIC; OpenStack — сеть; vSphere — портгруппа; Nutanix — подсеть" },
        "flavor": { "type": "string", "title": "OpenStack: flavor" },
        "start": { "type": "boolean", "title": "Включить после восстановления", "default": false },
        "target_dir": { "type": "string", "title": "Только файлы в каталог", "description": "Диски и описание ВМ — для импорта вручную; гипервизор не трогать" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] {
    &["platform", "url", "user", "password", "fingerprint", "insecure", "project", "domain", "region", "ssh_host", "ssh_port", "ssh_user", "ssh_password", "ssh_key", "ssh_fingerprint", "libvirt_uri"]
}

// ---------------------------------------------------------------- выполнение команд на узле

/// Где выполняются команды гипервизора: здесь или на узле по SSH (от root).
#[derive(Clone)]
pub enum Host { Local, Ssh(Arc<crate::sshc::Conn>, String) }

impl Host {
    pub async fn connect(cfg: &Value, host_override: Option<&str>, log: &LogFn) -> Result<Host> {
        let Some(h) = host_override.map(|s| s.to_string()).or_else(|| cfg_str(cfg, "ssh_host")) else { return Ok(Host::Local) };
        let auth = crate::sshc::Auth { user: cfg_str(cfg, "ssh_user").unwrap_or_else(|| "root".into()), password: cfg_str(cfg, "ssh_password").unwrap_or_default(), private_key: cfg_str(cfg, "ssh_key").unwrap_or_default(), ..Default::default() };
        let port = cfg.get("ssh_port").and_then(|x| x.as_u64()).unwrap_or(22) as u16;
        let fp = if host_override.is_some() { String::new() } else { cfg_str(cfg, "ssh_fingerprint").unwrap_or_default() };
        let c = crate::sshc::connect(&h, port, &auth, &fp).await.with_context(|| format!("SSH {h}"))?;
        if fp.is_empty() { log(&format!("узел {h}: ключ {} (закрепите его в «Отпечаток ключа узла SSH»)", c.host_key)); }
        Ok(Host::Ssh(Arc::new(c), h))
    }

    pub fn label(&self) -> String { match self { Host::Local => "эта машина".into(), Host::Ssh(_, h) => h.clone() } }

    /// Команда оболочки от root; stdout — ответ, ошибка — с концом stderr.
    pub async fn sh(&self, cmd: &str, log: &LogFn) -> Result<String> {
        match self {
            Host::Local => kit::output(&Cmd::new("sh").args(["-c", cmd]), log).await,
            Host::Ssh(c, h) => {
                let o = c.exec_root(cmd, None, std::time::Duration::from_secs(3600)).await?;
                if o.code != 0 { bail!("{h}: {cmd}: код {} {}", o.code, o.err().lines().last().unwrap_or("")); }
                Ok(o.text())
            }
        }
    }

    /// Вывод команды — потоком в репозиторий.
    pub async fn stream_out(&self, w: &mut SnapshotWriter, name: &str, cmd: &str, log: &LogFn) -> Result<StreamEntry> {
        match self {
            Host::Local => kit::stream_cmd(w, name, &Cmd::new("sh").args(["-c", cmd]), log).await,
            Host::Ssh(c, h) => {
                log(&format!("$ [{h}] {cmd}"));
                let (wrapped, prefix) = c.root_wrap(cmd)?;
                let (r, jh) = c.stream_out(&wrapped, prefix).await?;
                let res = w.add_stream(name, r, None).await;
                let (code, err) = jh.await??;
                if code != 0 { bail!("{h}: {cmd}: код {code} {}", err.lines().last().unwrap_or("")); }
                let e = res?;
                log(&format!("  {name}: {}", crate::util::human_bytes(e.size)));
                Ok(e)
            }
        }
    }

    /// Поток копии — в stdin команды.
    pub async fn stream_in(&self, repo: &Arc<Repo>, s: &StreamEntry, cmd: &str, log: &LogFn) -> Result<()> {
        match self {
            Host::Local => kit::feed(repo, s, &Cmd::new("sh").args(["-c", cmd]), log).await,
            Host::Ssh(c, h) => {
                log(&format!("$ [{h}] {cmd} < {}", s.name));
                let (wrapped, prefix) = c.root_wrap(cmd)?;
                let (reader, fetch) = repo.stream_reader(s)?;
                let (code, err) = c.stream_in(&wrapped, prefix, reader).await?;
                fetch.await??;
                if code != 0 { bail!("{h}: {cmd}: код {code} {}", err.lines().last().unwrap_or("")); }
                Ok(())
            }
        }
    }

    /// Записать небольшой файл на узле.
    pub async fn write(&self, path: &str, data: &[u8], log: &LogFn) -> Result<()> {
        match self {
            Host::Local => { if let Some(p) = Path::new(path).parent() { std::fs::create_dir_all(p)?; } std::fs::write(path, data).with_context(|| format!("запись {path}")) }
            Host::Ssh(c, h) => {
                let (wrapped, prefix) = c.root_wrap(&format!("umask 077; cat > {}", crate::util::shq(path)))?;
                let (code, err) = c.stream_in(&wrapped, prefix, std::io::Cursor::new(data.to_vec())).await?;
                if code != 0 { bail!("{h}: запись {path}: {err}"); }
                let _ = log;
                Ok(())
            }
        }
    }
}

pub fn q(s: &str) -> String { crate::util::shq(s) }

pub fn safe(n: &str) -> String { n.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect() }

// ---------------------------------------------------------------- платформа

/// ВМ платформы.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VmRef {
    pub id: String,
    pub name: String,
    pub state: String,
    /// Данные платформы, нужные копии (узел, тип, хост…).
    #[serde(default)]
    pub extra: Value,
}

#[async_trait]
pub trait Driver: Send + Sync {
    async fn list(&self, log: &LogFn) -> Result<Vec<VmRef>>;
    /// Копия одной ВМ: потоки с префиксом `prefix`; ответ — описание ВМ для восстановления.
    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value>;
    /// Восстановление одной ВМ на платформу.
    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value>;
}

pub async fn driver(cfg: &Value, log: &LogFn) -> Result<Box<dyn Driver>> {
    let p = cfg_str(cfg, "platform").unwrap_or_else(|| "libvirt".into());
    Ok(match p.as_str() {
        "libvirt" => Box::new(libvirt::Libvirt::new(Host::connect(cfg, None, log).await?, cfg)),
        "proxmox" => Box::new(proxmox::Proxmox::new(Host::connect(cfg, None, log).await?)),
        "ovirt" => Box::new(ovirt::Ovirt::new(cfg)?),
        "opennebula" => Box::new(opennebula::OpenNebula::new(cfg)?),
        "openstack" => Box::new(openstack::OpenStack::login(cfg, log).await?),
        "vmware" => Box::new(Arc::new(vmware::Vsphere::login(cfg, log).await?)),
        "xen" => Box::new(xen::Xapi::login(cfg).await?),
        "nutanix" => Box::new(nutanix::Nutanix::new(cfg)?),
        "spacevm" | "vmmanager" => Box::new(restapi::NodeApi::login(&p, cfg, log).await?),
        x => bail!("неизвестная платформа {x}"),
    })
}

/// Клиент HTTP платформы (длинные операции — без короткого ограничения времени).
pub fn http(cfg: &Value) -> Result<reqwest::Client> { kit::http(&cfg_str(cfg, "fingerprint").unwrap_or_default(), kit::bool_param(cfg, "insecure", false), 24 * 3600) }

/// Ответ HTTP (GET образа диска) — потоком в репозиторий.
pub async fn http_to_stream(w: &mut SnapshotWriter, name: &str, rb: reqwest::RequestBuilder, log: &LogFn) -> Result<StreamEntry> {
    let resp = rb.send().await.context("загрузка образа")?;
    if !resp.status().is_success() { let st = resp.status(); bail!("{name}: {} {}", st.as_u16(), resp.text().await.unwrap_or_default().chars().take(300).collect::<String>()); }
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let pump = tokio::spawn(async move {
        use futures::StreamExt;
        let mut s = resp.bytes_stream();
        while let Some(b) = s.next().await {
            let b = b?;
            if tx.send(b.to_vec()).await.is_err() { break; }
        }
        Ok::<(), anyhow::Error>(())
    });
    let e = w.add_stream(name, ChanRead { rx, buf: Vec::new(), pos: 0 }, None).await;
    pump.await??;
    let e = e?;
    log(&format!("  {name}: {}", crate::util::human_bytes(e.size)));
    Ok(e)
}

pub(crate) struct ChanRead { rx: tokio::sync::mpsc::Receiver<Vec<u8>>, buf: Vec<u8>, pos: usize }
impl ChanRead { pub(crate) fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self { ChanRead { rx, buf: Vec::new(), pos: 0 } } }
impl std::io::Read for ChanRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() { match self.rx.blocking_recv() { Some(b) => { self.buf = b; self.pos = 0; } None => return Ok(0) } }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Тело запроса из потока копии (загрузка образа на платформу).
pub fn repo_body(repo: &Arc<Repo>, s: &StreamEntry) -> Result<(reqwest::Body, tokio::task::JoinHandle<Result<u64>>)> {
    let (reader, fetch) = repo.stream_reader(s)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(16);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut r = reader;
        loop {
            let mut b = vec![0u8; 1 << 20];
            match r.read(&mut b) { Ok(0) => break, Ok(n) => { b.truncate(n); if tx.blocking_send(Ok(bytes::Bytes::from(b))).is_err() { break; } } Err(e) => { let _ = tx.blocking_send(Err(e)); break; } }
        }
    });
    let st = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
    Ok((reqwest::Body::wrap_stream(st), fetch))
}

/// Ждать условия (опрос раз в `step` секунд, не дольше `secs`).
pub async fn poll<F, Fut>(secs: u64, step: u64, what: &str, mut f: F) -> Result<Value>
where F: FnMut() -> Fut, Fut: std::future::Future<Output = Result<Option<Value>>> {
    let t0 = std::time::Instant::now();
    loop {
        if let Some(v) = f().await? { return Ok(v); }
        if t0.elapsed().as_secs() > secs { bail!("{what}: не дождались за {secs} с"); }
        tokio::time::sleep(std::time::Duration::from_secs(step)).await;
    }
}

fn select(all: &[VmRef], want: &[String]) -> Result<Vec<VmRef>> {
    let mut out = Vec::new();
    for w in want {
        let v = all.iter().find(|v| v.id == *w || v.name == *w).ok_or_else(|| anyhow!("ВМ «{w}» не найдена (есть: {})", all.iter().take(20).map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")))?;
        out.push(v.clone());
    }
    Ok(out)
}

#[async_trait]
impl Plugin for VmImage {
    fn id(&self) -> &'static str { "vm" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let d = driver(cfg, log).await?;
        let l = d.list(log).await?;
        Ok(json!({ "platform": cfg_str(cfg, "platform"), "vms": l.iter().map(|v| json!({ "id": v.id, "name": v.name, "state": v.state })).collect::<Vec<_>>() }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let platform = cfg_str(&ctx.config, "platform").unwrap_or_else(|| "libvirt".into());
        let d = driver(&ctx.config, log).await?;
        let all = d.list(log).await?;
        let want = crate::connectors::net::p_list(&ctx.config, "vms");
        let vms = if want.is_empty() {
            if !kit::bool_param(&ctx.config, "all_vms", false) { bail!("не выбраны ВМ: укажите их или включите «Все ВМ платформы»"); }
            all.clone()
        } else { select(&all, &want)? };
        log(&format!("{} · ВМ: {}", PLATFORMS.iter().find(|p| p.0 == platform).map(|p| p.1).unwrap_or(&platform), vms.iter().map(|v| format!("{} ({})", v.name, v.state)).collect::<Vec<_>>().join(", ")));
        // Описание ВМ из прошлой копии (vSphere CBT: изменения с того момента).
        let prev = match kit::snapshots_of(&ctx.repo, &ctx.instance).await?.last() { Some(h) => ctx.repo.manifest(&h.id).await.map(|(_, m)| m.meta).unwrap_or_default(), None => Value::Null };
        let mut w = ctx.repo.writer();
        let mut metas = Vec::new();
        for v in &vms {
            log(&format!("— ВМ {} [{}]", v.name, v.id));
            let prefix = format!("vm/{}/", safe(&v.id));
            let mut c2 = ctx.config.clone();
            c2["_prev"] = prev["vms"].as_array().and_then(|a| a.iter().find(|x| x["id"] == v.id.as_str()).cloned()).unwrap_or(Value::Null);
            let mut m = d.backup_vm(v, &mut w, &prefix, &c2, log).await.with_context(|| format!("ВМ {}", v.name))?;
            m["id"] = json!(v.id); m["name"] = json!(v.name); m["state"] = json!(v.state); m["prefix"] = json!(prefix);
            metas.push(m);
        }
        let meta = json!({ "platform": platform, "vms": metas });
        let (header, stats) = w.finish(&ctx.instance, "vm", "full", json!({ "platform": platform, "vms": vms.iter().map(|v| v.name.clone()).collect::<Vec<_>>() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let want = cfg_str(&ctx.params, "vm");
        let vms: Vec<Value> = m.meta["vms"].as_array().cloned().unwrap_or_default().into_iter().filter(|v| want.as_ref().map(|w| v["id"] == w.as_str() || v["name"] == w.as_str()).unwrap_or(true)).collect();
        if vms.is_empty() { bail!("в копии нет такой ВМ"); }
        if cfg_str(&ctx.params, "new_name").is_some() && vms.len() > 1 { bail!("«Новое имя» — для одной ВМ: укажите её в «ВМ из копии»"); }
        let mut out = Vec::new();
        if let Some(dir) = cfg_str(&ctx.params, "target_dir") {
            for v in &vms {
                let prefix = v["prefix"].as_str().unwrap_or("");
                let d = Path::new(&dir).join(safe(v["name"].as_str().unwrap_or("vm")));
                std::fs::create_dir_all(&d)?;
                for s in m.streams.iter().filter(|s| s.name.starts_with(prefix)) {
                    let f = d.join(s.name.trim_start_matches(prefix));
                    kit::to_file(&ctx.repo, s, &f).await?;
                    log(&format!("  {} → {}", s.name, f.display()));
                }
                std::fs::write(d.join("hopebackup-vm.json"), serde_json::to_vec_pretty(v)?)?;
                out.push(json!({ "vm": v["name"], "dir": d }));
            }
            return Ok(json!({ "target_dir": dir, "vms": out }));
        }
        let mut p = ctx.params.clone();
        if cfg_str(&p, "platform").is_none() { p["platform"] = m.meta["platform"].clone(); }
        if p["platform"] != m.meta["platform"] { bail!("копия ВМ платформы {} — на платформу {} её можно перенести только файлами (каталог)", m.meta["platform"], p["platform"]); }
        let d = driver(&p, log).await?;
        for v in &vms {
            log(&format!("— ВМ {}", v["name"].as_str().unwrap_or("")));
            out.push(d.restore_vm(&ctx.repo, &m, v, &p, log).await.with_context(|| format!("ВМ {}", v["name"]))?);
        }
        Ok(json!({ "vms": out }))
    }
}

/// Потоки ВМ по имени внутри её префикса.
pub fn vm_stream<'a>(m: &'a Manifest, vm: &Value, name: &str) -> Result<&'a StreamEntry> {
    kit::stream(m, &format!("{}{name}", vm["prefix"].as_str().unwrap_or("")))
}
