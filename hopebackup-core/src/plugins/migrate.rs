//! Цели переезда: платформа, на которой реплика машины становится ВМ.
//!
//! При первом применении цель создаёт ВМ с параметрами источника (процессоры, память, прошивка, сетевые
//! интерфейсы с теми же MAC) и тома под диски; дальше — находит их по состоянию, которое возвращает
//! применение, и только открывает тома для записи изменений.
//!
//! * Proxmox VE — агент на узле: `qm create`, тома — `pvesm alloc` в выбранном хранилище, запись — прямо
//!   в том (файл, LVM, LVM-thin, ZFS, Ceph RBD через `rbd map`).
//! * oVirt / zVirt / РОСА / HOSTVM — через API Engine: ВМ, диски, интерфейсы; запись — передачами imageio.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::appkit::{self as kit, Cmd};
use super::vm::ovirt::{urlenc, Ovirt};
use super::vm::poll;
use super::{cfg_str, LogFn};

/// Диск источника, который нужен на цели.
#[derive(Clone, Debug)]
pub struct DiskPlan { pub id: String, pub size: u64 }

/// Том цели для записи: путь, устройство или `imageio:<адрес>`; `prezero` — новый том не обнулён.
#[derive(Clone, Debug)]
pub struct Vol { pub disk: String, pub target: String, pub prezero: bool }

#[async_trait]
pub trait Target: Send {
    /// Создать ВМ и тома (первое применение) или найти их по состоянию; вернуть тома для записи.
    async fn open(&mut self, disks: &[DiskPlan], machine: &Value, log: &LogFn) -> Result<Vec<Vol>>;
    /// После записи: закрыть передачи и устройства; вернуть состояние для следующего применения.
    async fn close(&mut self, ok: bool, log: &LogFn) -> Result<Value>;
    /// Управление ВМ цели (по состоянию): `test-start` — снимок томов и запуск без сети; `test-stop` —
    /// остановка и возврат к снимку (тома снова совпадают с применённой синхронизацией); `start` — запуск
    /// после переключения со снимком для отката; `rollback` — остановка и возврат к нему; `finish` —
    /// убрать снимок отката; `stop`. Если снимки хранилищу недоступны, тома помечаются «изменены на цели»
    /// (`dirty`), и следующее применение перепишет их целиком.
    async fn power(&mut self, op: &str, log: &LogFn) -> Result<Value>;
}

pub const OPS: &[&str] = &["test-start", "test-stop", "start", "stop", "rollback", "finish"];

pub fn target(platform: &str, params: &Value) -> Result<Box<dyn Target>> {
    let state = params.get("state").cloned().unwrap_or(Value::Null);
    if !state.is_null() && state["platform"].as_str().is_some_and(|p| p != platform) { bail!("состояние цели — от платформы {}, а выбрана {platform}", state["platform"]); }
    Ok(match platform {
        "proxmox" => Box::new(Proxmox { p: params.clone(), st: state, maps: vec![] }),
        "ovirt" | "zvirt" => Box::new(OvirtT { c: Ovirt::new(params)?, p: params.clone(), st: state, transfers: vec![] }),
        "libvirt" | "kvm" => Box::new(Libvirt { p: params.clone(), st: state }),
        _ => bail!("платформа цели «{platform}» не поддерживается (proxmox, libvirt, ovirt)"),
    })
}

/// Имя ВМ: заданное или имя машины источника — только буквы, цифры, «-» и «.».
fn vm_name(p: &Value, machine: &Value) -> String {
    let n = cfg_str(p, "name").unwrap_or_else(|| machine["hostname"].as_str().unwrap_or("migrated").to_string());
    let s: String = n.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect();
    let s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    if s.is_empty() { "migrated".into() } else { s }
}

/// Память ВМ, МиБ: заданная или как у источника (MemTotal меньше настоящей — округляем вверх).
fn memory_mb(p: &Value, machine: &Value) -> u64 {
    if let Some(m) = p.get("memory_mb").and_then(|x| x.as_u64()) { return m; }
    let mb = machine["memory"].as_u64().unwrap_or(1 << 30).div_ceil(1 << 20);
    let step = if mb > 2048 { 1024 } else { 256 };
    mb.div_ceil(step).max(1) * step
}

fn cores(p: &Value, machine: &Value) -> u64 { p.get("cores").and_then(|x| x.as_u64()).unwrap_or_else(|| machine["cpus"].as_u64().unwrap_or(1)).max(1) }

/// Сеть цели для интерфейса источника: по имени интерфейса, иначе «*», иначе `def`.
fn net_for(p: &Value, nic: &str, def: &str) -> String {
    let m = &p["networks"];
    m[nic].as_str().or(m["*"].as_str()).map(String::from).unwrap_or_else(|| def.to_string())
}

// ---------------------------------------------------------------- Proxmox VE

struct Proxmox { p: Value, st: Value, maps: Vec<String> }

async fn sh(prog: &str, args: &[String], log: &LogFn) -> Result<String> { kit::output(&Cmd::new(prog).args(args.iter().cloned()), log).await }
fn a<const N: usize>(x: [&str; N]) -> Vec<String> { x.iter().map(|s| s.to_string()).collect() }

impl Proxmox {
    async fn storage_type(&self, storage: &str, log: &LogFn) -> Result<String> {
        let o = sh("pvesm", &a(["status", "--storage", storage]), log).await.with_context(|| format!("хранилище {storage} (это узел Proxmox VE?)"))?;
        o.lines().skip(1).find_map(|l| { let f: Vec<&str> = l.split_whitespace().collect(); (f.first() == Some(&storage)).then(|| f.get(1).unwrap_or(&"").to_string()) })
            .ok_or_else(|| anyhow!("хранилище {storage} не найдено на узле"))
    }

    /// Путь тома на узле; том RBD подключается через `rbd map`, неактивный том LVM — активируется.
    async fn vol_path(&mut self, volid: &str, log: &LogFn) -> Result<String> {
        let p = sh("pvesm", &a(["path", volid]), log).await?.trim().to_string();
        if let Some(rest) = p.strip_prefix("rbd:") {
            let img = rest.split(':').next().unwrap_or("").to_string();
            let dev = sh("rbd", &a(["map", &img]), log).await.with_context(|| format!("rbd map {img}"))?.trim().to_string();
            self.maps.push(dev.clone());
            return Ok(dev);
        }
        if !p.starts_with('/') { bail!("том {volid}: запись в «{p}» не поддерживается — выберите хранилище с локальным путём или RBD"); }
        if !std::path::Path::new(&p).exists() && p.starts_with("/dev/") { let _ = sh("lvchange", &a(["-ay", &p]), log).await; }
        if !std::path::Path::new(&p).exists() { bail!("том {volid}: нет {p}"); }
        Ok(p)
    }
}

#[async_trait]
impl Target for Proxmox {
    async fn open(&mut self, disks: &[DiskPlan], machine: &Value, log: &LogFn) -> Result<Vec<Vol>> {
        let storage = cfg_str(&self.p, "storage").or_else(|| self.st["storage"].as_str().map(String::from)).ok_or_else(|| anyhow!("не задано хранилище Proxmox (storage)"))?;
        let stype = self.storage_type(&storage, log).await?;
        let mut created = false;
        let vmid = match self.st["vmid"].as_str() {
            Some(v) => v.to_string(),
            None => {
                let vmid = match self.p.get("vmid").and_then(|x| x.as_u64()) { Some(v) => v.to_string(), None => sh("pvesh", &a(["get", "/cluster/nextid"]), log).await?.trim().trim_matches('"').to_string() };
                if sh("qm", &a(["status", &vmid]), log).await.is_ok() { bail!("ВМ {vmid} уже есть на узле — задайте другой vmid"); }
                let name = vm_name(&self.p, machine);
                let mut args = a(["create", &vmid, "--name", &name, "--cores", &cores(&self.p, machine).to_string(), "--memory", &memory_mb(&self.p, machine).to_string(),
                    "--ostype", "l26", "--scsihw", "virtio-scsi-single", "--agent", "enabled=1"]);
                if machine["firmware"] == "uefi" { args.extend(a(["--bios", "ovmf", "--machine", "q35", "--efidisk0", &format!("{storage}:1,efitype=4m,pre-enrolled-keys=0")])); }
                for (i, n) in machine["nics"].as_array().into_iter().flatten().enumerate() {
                    let mac = n["mac"].as_str().unwrap_or("");
                    let br = net_for(&self.p, n["name"].as_str().unwrap_or(""), "vmbr0");
                    args.push(format!("--net{i}"));
                    args.push(if mac.is_empty() { format!("virtio,bridge={br}") } else { format!("virtio={mac},bridge={br}") });
                }
                sh("qm", &args, log).await.context("создание ВМ")?;
                log(&format!("  создана ВМ {vmid} «{name}»: {} vCPU, {} МиБ, {}", cores(&self.p, machine), memory_mb(&self.p, machine), machine["firmware"].as_str().unwrap_or("bios")));
                self.st = json!({ "platform": "proxmox", "vmid": vmid, "storage": storage, "volumes": {} });
                created = true;
                vmid
            }
        };
        let mut out = Vec::new();
        let mut first_slot = None;
        for d in disks {
            let have = self.st["volumes"][&d.id].clone();
            let (volid, slot, fresh) = if have.is_object() {
                let volid = have["volid"].as_str().unwrap_or("").to_string();
                let slot = have["slot"].as_str().unwrap_or("").to_string();
                if d.size > have["size"].as_u64().unwrap_or(0) {
                    sh("qm", &a(["resize", &vmid, &slot, &format!("{}K", d.size.div_ceil(1024))]), log).await.context("увеличение тома")?;
                    log(&format!("  {slot}: том увеличен до {}", crate::util::human_bytes(d.size)));
                    self.st["volumes"][&d.id]["size"] = json!(d.size);
                }
                (volid, slot, false)
            } else {
                let used = self.st["volumes"].as_object().map(|m| m.len()).unwrap_or(0);
                let slot = format!("scsi{used}");
                let list = sh("pvesm", &a(["list", &storage, "--vmid", &vmid]), log).await.unwrap_or_default();
                let fname = (0..).map(|n| format!("vm-{vmid}-disk-{n}")).find(|f| !list.contains(&format!("/{f}")) && !list.contains(&format!(":{f}"))).unwrap();
                let o = sh("pvesm", &a(["alloc", &storage, &vmid, &fname, &d.size.div_ceil(1024).to_string(), "--format", "raw"]), log).await.with_context(|| format!("том под диск {}", d.id))?;
                let volid = o.split('\'').nth(1).map(String::from).unwrap_or_else(|| format!("{storage}:{fname}"));
                sh("qm", &a(["set", &vmid, &format!("--{slot}"), &format!("{volid},discard=on,iothread=1")]), log).await.context("подключение тома")?;
                self.st["volumes"][&d.id] = json!({ "volid": volid, "slot": slot, "size": d.size });
                log(&format!("  {slot}: {volid} ({}) ← диск {}", crate::util::human_bytes(d.size), d.id));
                (volid, slot, true)
            };
            if first_slot.is_none() { first_slot = Some(slot.clone()); }
            let path = self.vol_path(&volid, log).await?;
            // Толстый LVM не обнуляет новый том; прочие хранилища выдают нули.
            out.push(Vol { disk: d.id.clone(), target: path, prezero: fresh && stype == "lvm" });
        }
        if created {
            if let Some(s) = first_slot { sh("qm", &a(["set", &vmid, "--boot", &format!("order={s}")]), log).await?; }
        }
        Ok(out)
    }

    async fn close(&mut self, ok: bool, log: &LogFn) -> Result<Value> {
        for d in std::mem::take(&mut self.maps) { let _ = sh("rbd", &a(["unmap", &d]), log).await; }
        if ok && kit::bool_param(&self.p, "start", false) { self.power("start", log).await?; }
        Ok(self.st.clone())
    }

    async fn power(&mut self, op: &str, log: &LogFn) -> Result<Value> {
        let vmid = self.st["vmid"].as_str().ok_or_else(|| anyhow!("ВМ на цели ещё не создана — сначала примените синхронизацию"))?.to_string();
        let snap = |n: &str| a(["snapshot", &vmid, n, "--description", "HopeBackup: возврат томов к применённой синхронизации"]);
        match op {
            "test-start" => {
                if !self.st["testing"].is_null() { bail!("тестовый запуск уже идёт"); }
                let has = sh("qm", &snap("hbtest"), log).await.is_ok();
                if !has { log("  ! хранилище не поддерживает снимки: после теста тома будут переписаны целиком"); }
                // Без сети: интерфейсы с теми же MAC и адресами, что у работающего источника.
                let conf = sh("qm", &a(["config", &vmid]), log).await?;
                for l in conf.lines() {
                    if let Some((k, v)) = l.split_once(": ") {
                        if k.starts_with("net") && k[3..].chars().all(|c| c.is_ascii_digit()) && !v.contains("link_down=1") {
                            sh("qm", &a(["set", &vmid, &format!("--{k}"), &format!("{v},link_down=1")]), log).await?;
                        }
                    }
                }
                sh("qm", &a(["start", &vmid]), log).await.context("запуск ВМ")?;
                self.st["testing"] = json!({ "snapshot": has, "since": chrono::Utc::now().to_rfc3339() });
                log(&format!("  тестовый запуск ВМ {vmid}: сеть отключена{}", if has { ", снимок hbtest" } else { "" }));
            }
            "test-stop" => {
                let t = self.st["testing"].clone();
                if t.is_null() { bail!("тестовый запуск не идёт"); }
                let _ = sh("qm", &a(["stop", &vmid]), log).await;
                if t["snapshot"].as_bool().unwrap_or(false) {
                    // Снимок хранит и конфигурацию: интерфейсы вернутся подключёнными.
                    sh("qm", &a(["rollback", &vmid, "hbtest"]), log).await.context("возврат к снимку")?;
                    sh("qm", &a(["delsnapshot", &vmid, "hbtest"]), log).await?;
                } else {
                    self.relink(&vmid, log).await?;
                    self.st["dirty"] = json!(true);
                }
                self.st["testing"] = Value::Null;
                log(&format!("  тест окончен: ВМ {vmid} остановлена, тома {}", if t["snapshot"].as_bool().unwrap_or(false) { "возвращены к снимку" } else { "будут переписаны при следующем применении" }));
            }
            "start" => {
                if !self.st["testing"].is_null() { bail!("идёт тестовый запуск — сначала завершите его"); }
                let has = sh("qm", &snap("hbcutover"), log).await.is_ok();
                self.relink(&vmid, log).await?;
                sh("qm", &a(["start", &vmid]), log).await.context("запуск ВМ")?;
                self.st["running"] = json!({ "snapshot": has, "since": chrono::Utc::now().to_rfc3339() });
                log(&format!("  ВМ {vmid} запущена{}", if has { " (снимок hbcutover — для отката)" } else { "" }));
            }
            "rollback" => {
                let r = self.st["running"].clone();
                let _ = sh("qm", &a(["stop", &vmid]), log).await;
                if r["snapshot"].as_bool().unwrap_or(false) {
                    sh("qm", &a(["rollback", &vmid, "hbcutover"]), log).await.context("возврат к снимку")?;
                    sh("qm", &a(["delsnapshot", &vmid, "hbcutover"]), log).await?;
                } else { self.st["dirty"] = json!(true); }
                self.st["running"] = Value::Null;
                log(&format!("  откат: ВМ {vmid} остановлена, тома {}", if r["snapshot"].as_bool().unwrap_or(false) { "возвращены к переключению" } else { "будут переписаны при следующем применении" }));
            }
            "finish" => {
                if self.st["running"]["snapshot"].as_bool().unwrap_or(false) { let _ = sh("qm", &a(["delsnapshot", &vmid, "hbcutover"]), log).await; }
                self.st["running"]["snapshot"] = json!(false);
                self.st["finished"] = json!(true);
                log(&format!("  переезд завершён: снимок отката ВМ {vmid} удалён"));
            }
            "stop" => { sh("qm", &a(["stop", &vmid]), log).await?; self.st["running"] = Value::Null; }
            _ => bail!("неизвестная операция {op}"),
        }
        Ok(self.st.clone())
    }
}

impl Proxmox {
    /// Подключить интерфейсы (снять link_down).
    async fn relink(&self, vmid: &str, log: &LogFn) -> Result<()> {
        let conf = sh("qm", &a(["config", vmid]), log).await?;
        for l in conf.lines() {
            if let Some((k, v)) = l.split_once(": ") {
                if k.starts_with("net") && v.contains("link_down=1") {
                    let nv: Vec<&str> = v.split(',').filter(|x| *x != "link_down=1").collect();
                    sh("qm", &a(["set", vmid, &format!("--{k}"), &nv.join(",")]), log).await?;
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- oVirt / zVirt

struct OvirtT { c: Ovirt, p: Value, st: Value, transfers: Vec<(String, String)> }

impl OvirtT {
    async fn wait_disk(&self, did: &str) -> Result<()> {
        poll(1800, 2, "диск", || async { let x = self.c.get(&format!("/disks/{did}")).await?; Ok((x["status"] == "ok").then_some(x)) }).await?;
        Ok(())
    }
}

#[async_trait]
impl Target for OvirtT {
    async fn open(&mut self, disks: &[DiskPlan], machine: &Value, log: &LogFn) -> Result<Vec<Vol>> {
        let sd = cfg_str(&self.p, "storage").or_else(|| self.st["storage"].as_str().map(String::from)).ok_or_else(|| anyhow!("не задан домен хранения (storage)"))?;
        let vid = match self.st["vm_id"].as_str() {
            Some(v) => v.to_string(),
            None => {
                let cluster = cfg_str(&self.p, "cluster").ok_or_else(|| anyhow!("не задан кластер (cluster)"))?;
                let name = vm_name(&self.p, machine);
                let exists = self.c.get(&format!("/vms?search=name%3D{}", urlenc(&name))).await?["vm"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
                if exists { bail!("ВМ «{name}» уже есть — задайте другое имя (name)"); }
                let mem = memory_mb(&self.p, machine) << 20;
                let bios = if machine["firmware"] == "uefi" { "q35_ovmf" } else { "q35_sea_bios" };
                let nv = self.c.post("/vms", json!({ "name": name, "cluster": { "name": cluster }, "template": { "name": "Blank" },
                    "cpu": { "topology": { "sockets": 1, "cores": cores(&self.p, machine), "threads": 1 } },
                    "memory": mem, "memory_policy": { "guaranteed": mem, "max": mem }, "bios": { "type": bios }, "os": { "type": "other_linux" },
                    "description": format!("Перенесена HopeBackup с {}", machine["hostname"].as_str().unwrap_or("")) })).await.context("создание ВМ")?;
                let vid = nv["id"].as_str().ok_or_else(|| anyhow!("oVirt: нет id ВМ"))?.to_string();
                poll(600, 2, "создание ВМ", || async { let x = self.c.get(&format!("/vms/{vid}")).await?; Ok((x["status"] == "down").then_some(x)) }).await?;
                let profiles = self.c.get("/vnicprofiles").await.unwrap_or_default()["vnic_profile"].as_array().cloned().unwrap_or_default();
                for (i, n) in machine["nics"].as_array().into_iter().flatten().enumerate() {
                    let pname = net_for(&self.p, n["name"].as_str().unwrap_or(""), "ovirtmgmt");
                    let prof = profiles.iter().find(|x| x["name"] == pname.as_str()).and_then(|x| x["id"].as_str()).ok_or_else(|| anyhow!("нет профиля сети «{pname}»"))?;
                    let mut b = json!({ "name": format!("nic{}", i + 1), "interface": "virtio", "vnic_profile": { "id": prof } });
                    if let Some(m) = n["mac"].as_str().filter(|m| !m.is_empty()) { b["mac"] = json!({ "address": m }); }
                    self.c.post(&format!("/vms/{vid}/nics"), b).await.with_context(|| format!("интерфейс {}", n["name"]))?;
                }
                log(&format!("  создана ВМ «{name}» ({vid}) в кластере {cluster}: {} vCPU, {} МиБ, {bios}", cores(&self.p, machine), mem >> 20));
                self.st = json!({ "platform": "ovirt", "vm_id": vid, "storage": sd, "disks": {} });
                vid
            }
        };
        // Блочный домен (iSCSI, FC): raw — только с полным выделением, и новый диск не обнулён.
        let sdv = self.c.get(&format!("/storagedomains?search=name%3D{}", urlenc(&sd))).await.unwrap_or_default();
        let block = sdv["storage_domain"][0]["storage"]["type"].as_str().map(|t| t == "iscsi" || t == "fcp").unwrap_or(false);
        let mut out = Vec::new();
        for (i, d) in disks.iter().enumerate() {
            let have = self.st["disks"][&d.id].clone();
            let (did, fresh) = if have.is_object() {
                let did = have["id"].as_str().unwrap_or("").to_string();
                if d.size > have["size"].as_u64().unwrap_or(0) {
                    self.c.call(reqwest::Method::PUT, &format!("/vms/{vid}/diskattachments/{did}"), Some(json!({ "disk": { "provisioned_size": d.size } }))).await.context("увеличение диска")?;
                    self.wait_disk(&did).await?;
                    self.st["disks"][&d.id]["size"] = json!(d.size);
                }
                (did, false)
            } else {
                let name = format!("{}_{}", vm_name(&self.p, machine), d.id);
                let nd = self.c.post("/disks", json!({ "name": name, "provisioned_size": d.size, "format": "raw", "sparse": !block, "storage_domains": { "storage_domain": [{ "name": sd }] } })).await.context("создание диска")?;
                let did = nd["id"].as_str().ok_or_else(|| anyhow!("oVirt: нет id диска"))?.to_string();
                self.wait_disk(&did).await?;
                let boot = self.st["disks"].as_object().map(|m| m.is_empty()).unwrap_or(true) && i == 0;
                self.c.post(&format!("/vms/{vid}/diskattachments"), json!({ "disk": { "id": did }, "interface": "virtio_scsi", "bootable": boot, "active": true })).await.context("подключение диска")?;
                self.st["disks"][&d.id] = json!({ "id": did, "size": d.size });
                log(&format!("  диск {name} ({}) ← {}{}", crate::util::human_bytes(d.size), d.id, if boot { ", загрузочный" } else { "" }));
                (did, true)
            };
            let (tid, url) = self.c.transfer(&did, None, "upload", log).await.with_context(|| format!("передача для диска {}", d.id))?;
            self.transfers.push((tid, did.clone()));
            out.push(Vol { disk: d.id.clone(), target: format!("imageio:{url}"), prezero: fresh && block });
        }
        Ok(out)
    }

    async fn close(&mut self, ok: bool, log: &LogFn) -> Result<Value> {
        let mut err = None;
        for (tid, did) in std::mem::take(&mut self.transfers) {
            let r = if ok { self.c.finalize(&tid).await } else { self.c.post(&format!("/imagetransfers/{tid}/cancel"), json!({})).await.map(|_| ()) };
            if let Err(e) = r { err.get_or_insert(e); continue; }
            if let Err(e) = self.wait_disk(&did).await { err.get_or_insert(e); }
        }
        if let Some(e) = err { if ok { return Err(e.context("завершение передач")); } }
        if ok && kit::bool_param(&self.p, "start", false) { self.power("start", log).await?; }
        Ok(self.st.clone())
    }

    async fn power(&mut self, op: &str, log: &LogFn) -> Result<Value> {
        let vid = self.st["vm_id"].as_str().ok_or_else(|| anyhow!("ВМ на цели ещё не создана — сначала примените синхронизацию"))?.to_string();
        match op {
            "test-start" => {
                if !self.st["testing"].is_null() { bail!("тестовый запуск уже идёт"); }
                let sid = self.snapshot(&vid, "hopebackup-test").await?;
                self.link(&vid, false).await?;
                self.c.post(&format!("/vms/{vid}/start"), json!({})).await.context("запуск ВМ")?;
                self.st["testing"] = json!({ "snapshot": sid, "since": chrono::Utc::now().to_rfc3339() });
                log("  тестовый запуск: снимок ВМ, интерфейсы отключены, ВМ запущена");
            }
            "test-stop" => {
                let sid = self.st["testing"]["snapshot"].as_str().ok_or_else(|| anyhow!("тестовый запуск не идёт"))?.to_string();
                self.stop(&vid).await?;
                self.revert(&vid, &sid).await?;
                self.link(&vid, true).await?;
                self.st["testing"] = Value::Null;
                log("  тест окончен: ВМ остановлена, диски возвращены к снимку");
            }
            "start" => {
                if !self.st["testing"].is_null() { bail!("идёт тестовый запуск — сначала завершите его"); }
                let sid = self.snapshot(&vid, "hopebackup-cutover").await?;
                self.link(&vid, true).await?;
                self.c.post(&format!("/vms/{vid}/start"), json!({})).await.context("запуск ВМ")?;
                self.st["running"] = json!({ "snapshot": sid, "since": chrono::Utc::now().to_rfc3339() });
                log("  ВМ запущена (снимок hopebackup-cutover — для отката)");
            }
            "rollback" => {
                let sid = self.st["running"]["snapshot"].as_str().map(String::from);
                self.stop(&vid).await?;
                match sid { Some(s) => self.revert(&vid, &s).await?, None => self.st["dirty"] = json!(true) }
                self.st["running"] = Value::Null;
                log("  откат: ВМ остановлена, диски возвращены к переключению");
            }
            "finish" => {
                if let Some(s) = self.st["running"]["snapshot"].as_str().map(String::from) { self.drop_snapshot(&vid, &s).await?; }
                self.st["running"]["snapshot"] = Value::Null;
                self.st["finished"] = json!(true);
                log("  переезд завершён: снимок отката удалён");
            }
            "stop" => { self.stop(&vid).await?; self.st["running"] = Value::Null; }
            _ => bail!("неизвестная операция {op}"),
        }
        Ok(self.st.clone())
    }
}

impl OvirtT {
    async fn snapshot(&self, vid: &str, desc: &str) -> Result<String> {
        let s = self.c.post(&format!("/vms/{vid}/snapshots"), json!({ "description": desc, "persist_memorystate": false })).await.context("снимок ВМ")?;
        let sid = s["id"].as_str().ok_or_else(|| anyhow!("oVirt: нет id снимка"))?.to_string();
        poll(1800, 2, "снимок ВМ", || async { let x = self.c.get(&format!("/vms/{vid}/snapshots/{sid}")).await?; Ok((x["snapshot_status"] == "ok").then_some(x)) }).await?;
        Ok(sid)
    }
    async fn stop(&self, vid: &str) -> Result<()> {
        let _ = self.c.post(&format!("/vms/{vid}/stop"), json!({})).await;
        poll(600, 2, "остановка ВМ", || async { let x = self.c.get(&format!("/vms/{vid}")).await?; Ok((x["status"] == "down").then_some(x)) }).await?;
        Ok(())
    }
    /// Вернуть диски к снимку и убрать его.
    async fn revert(&self, vid: &str, sid: &str) -> Result<()> {
        self.c.post(&format!("/vms/{vid}/snapshots/{sid}/restore"), json!({ "restore_memory": false })).await.context("возврат к снимку")?;
        poll(1800, 2, "возврат к снимку", || async { let x = self.c.get(&format!("/vms/{vid}")).await?; Ok((x["status"] == "down").then_some(x)) }).await?;
        self.drop_snapshot(vid, sid).await
    }
    async fn drop_snapshot(&self, vid: &str, sid: &str) -> Result<()> {
        self.c.call(reqwest::Method::DELETE, &format!("/vms/{vid}/snapshots/{sid}"), None).await.context("удаление снимка")?;
        poll(3600, 3, "удаление снимка", || async { Ok(self.c.get(&format!("/vms/{vid}/snapshots/{sid}")).await.is_err().then_some(Value::Null)) }).await?;
        Ok(())
    }
    async fn link(&self, vid: &str, up: bool) -> Result<()> {
        for n in self.c.get(&format!("/vms/{vid}/nics")).await?["nic"].as_array().cloned().unwrap_or_default() {
            let nid = n["id"].as_str().unwrap_or("");
            self.c.call(reqwest::Method::PUT, &format!("/vms/{vid}/nics/{nid}"), Some(json!({ "linked": up }))).await.context("интерфейс ВМ")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- libvirt / KVM

/// Развёртывание на libvirt/KVM через `virsh`: тома — raw-файлы в каталоге хранилища (пул), ВМ —
/// определяется XML с параметрами источника (vCPU, память, UEFI, сетевые карты с теми же MAC).
struct Libvirt { p: Value, st: Value }

impl Libvirt {
    fn conn(&self) -> Vec<String> { cfg_str(&self.p, "connect").map(|u| a(["-c", &u]).to_vec()).unwrap_or_default() }
    async fn virsh(&self, args: &[String], log: &LogFn) -> Result<String> {
        let mut v = self.conn(); v.extend(args.iter().cloned());
        sh("virsh", &v, log).await
    }
}

#[async_trait]
impl Target for Libvirt {
    async fn open(&mut self, disks: &[DiskPlan], machine: &Value, log: &LogFn) -> Result<Vec<Vol>> {
        let pool = cfg_str(&self.p, "storage").or_else(|| self.st["storage"].as_str().map(String::from)).unwrap_or_else(|| "/var/lib/libvirt/images".into());
        std::fs::create_dir_all(&pool).ok();
        let name = vm_name(&self.p, machine);
        if self.st["name"].is_null() {
            if self.virsh(&a(["dominfo", &name]), log).await.is_ok() { bail!("ВМ «{name}» уже определена в libvirt — задайте другое имя (name)"); }
            self.st = json!({ "platform": "libvirt", "name": name, "storage": pool, "volumes": {} });
        }
        let pool = self.st["storage"].as_str().unwrap_or(&pool).to_string();
        let mut out = Vec::new();
        for d in disks {
            let have = self.st["volumes"][&d.id].clone();
            let path = if let Some(p) = have["path"].as_str() { p.to_string() } else {
                let fname = format!("{}-{}.img", vm_name(&self.p, machine), d.id);
                let path = std::path::Path::new(&pool).join(&fname).to_string_lossy().to_string();
                // Разрежённый raw-файл нужного размера (нули — «дырами»).
                sh("qemu-img", &a(["create", "-f", "raw", &path, &d.size.to_string()]), log).await.with_context(|| format!("том под диск {}", d.id))?;
                self.st["volumes"][&d.id] = json!({ "path": path, "size": d.size });
                log(&format!("  том {path} ({}) ← диск {}", crate::util::human_bytes(d.size), d.id));
                path
            };
            out.push(Vol { disk: d.id.clone(), target: path, prezero: false });
        }
        // Запомнить машину-источник для определения ВМ при close.
        self.st["machine"] = machine.clone();
        Ok(out)
    }

    async fn close(&mut self, ok: bool, log: &LogFn) -> Result<Value> {
        if !ok { return Ok(self.st.clone()); }
        if self.st["defined"].as_bool().unwrap_or(false) { return Ok(self.st.clone()); }
        let name = self.st["name"].as_str().unwrap_or("migrated").to_string();
        let machine = self.st["machine"].clone();
        let mem = memory_mb(&self.p, &machine);
        let vcpu = cores(&self.p, &machine);
        let uefi = machine["firmware"] == "uefi";
        let mut disks_xml = String::new();
        let letters = ["vda", "vdb", "vdc", "vdd", "vde", "vdf", "vdg", "vdh"];
        let vols = self.st["volumes"].as_object().cloned().unwrap_or_default();
        for (i, (_id, v)) in vols.iter().enumerate() {
            let path = v["path"].as_str().unwrap_or("");
            let dev = letters.get(i).copied().unwrap_or("vdz");
            disks_xml.push_str(&format!("<disk type='file' device='disk'><driver name='qemu' type='raw'/><source file='{}'/><target dev='{}' bus='virtio'/></disk>", xml_esc(path), dev));
        }
        let mut nics_xml = String::new();
        for n in machine["nics"].as_array().into_iter().flatten() {
            let net = net_for(&self.p, n["name"].as_str().unwrap_or(""), "default");
            let mac = n["mac"].as_str().unwrap_or("");
            let macx = if mac.is_empty() { String::new() } else { format!("<mac address='{}'/>", xml_esc(mac)) };
            // Имя вида brX/vmbrX/virbrX — мост, иначе сеть libvirt.
            let src = if net.contains("br") { format!("<interface type='bridge'>{macx}<source bridge='{}'/><model type='virtio'/></interface>", xml_esc(&net)) }
                      else { format!("<interface type='network'>{macx}<source network='{}'/><model type='virtio'/></interface>", xml_esc(&net)) };
            nics_xml.push_str(&src);
        }
        let os = if uefi { "<os firmware='efi'><type arch='x86_64' machine='q35'>hvm</type><boot dev='hd'/></os>" } else { "<os><type arch='x86_64' machine='q35'>hvm</type><boot dev='hd'/></os>" };
        let xml = format!(
            "<domain type='kvm'><name>{}</name><memory unit='MiB'>{mem}</memory><vcpu>{vcpu}</vcpu>{os}\
             <features><acpi/><apic/></features><cpu mode='host-passthrough'/>\
             <devices>{disks_xml}{nics_xml}<channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel><console type='pty'/></devices></domain>",
            xml_esc(&name));
        let wd = kit::WorkDir::new(None, None)?;
        let f = wd.join("domain.xml");
        std::fs::write(&f, &xml)?;
        self.virsh(&a(["define", &f.to_string_lossy()]), log).await.context("virsh define")?;
        log(&format!("  определена ВМ «{name}»: {vcpu} vCPU, {mem} МиБ, {}", if uefi { "UEFI" } else { "BIOS" }));
        self.st["defined"] = json!(true);
        if kit::bool_param(&self.p, "start", false) { self.virsh(&a(["start", &name]), log).await.context("запуск ВМ")?; log("  ВМ запущена"); }
        Ok(self.st.clone())
    }

    async fn power(&mut self, op: &str, log: &LogFn) -> Result<Value> {
        let name = self.st["name"].as_str().ok_or_else(|| anyhow!("ВМ ещё не создана"))?.to_string();
        match op { "start" => { self.virsh(&a(["start", &name]), log).await?; } "stop" => { let _ = self.virsh(&a(["destroy", &name]), log).await; } _ => bail!("для libvirt поддерживаются start/stop") }
        Ok(self.st.clone())
    }
}

fn xml_esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&apos;").replace('"', "&quot;") }

/// Операция с ВМ цели по состоянию (`params.state`).
pub async fn power(platform: &str, params: &Value, op: &str, log: &LogFn) -> Result<Value> {
    if !OPS.contains(&op) { bail!("неизвестная операция {op} (есть: {})", OPS.join(", ")); }
    let mut t = target(platform, params)?;
    t.power(op, log).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizing() {
        let m = json!({ "hostname": "web_01.corp", "cpus": 4, "memory": 4_022_000_000u64 });
        assert_eq!(memory_mb(&json!({}), &m), 4096);
        assert_eq!(memory_mb(&json!({}), &json!({ "memory": 990_000_000u64 })), 1024);
        assert_eq!(vm_name(&json!({}), &m), "web-01.corp");
        assert_eq!(net_for(&json!({ "networks": { "eth1": "vmbr2", "*": "vmbr1" } }), "eth0", "vmbr0"), "vmbr1");
    }
}
