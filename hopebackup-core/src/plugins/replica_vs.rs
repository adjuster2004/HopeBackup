//! Источник реплики без агента в гостевой системе: ВМ VMware vSphere (vCenter или ESXi; в том числе ВМ
//! vCloud Director — через их vCenter).
//!
//! Синхронизация: снимок ВМ (с заморозкой ФС через VMware Tools) → экспорт снимка (аренда NFC, VMDK
//! streamOptimized) → разбор потока в сплошной образ → сравнение блоков с картой прошлой синхронизации →
//! в репозиторий только изменённые блоки → удаление снимка. По сети каждый раз идёт сжатый диск без
//! пустых областей; на площадку — только изменения.
//! Переключение: штатное выключение гостевой ОС (VMware Tools), затем последняя синхронизация выключенной
//! ВМ — данные согласованы без остановки служб по одной.
//! Подготовка к KVM — сценарием в гостевой ОС через гостевые операции vSphere (нужна учётная запись ОС).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::appkit as kit;
use super::replica::{prev_maps, scan, ScanStats};
use super::vm::vmware::{mo, vdisks, Vsphere};
use super::vm::xml::{esc, Node};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn};

/// Сведения о ВМ для создания на цели.
pub(super) async fn machine(vs: &Vsphere, vm: &str) -> Result<(Value, String)> {
    let p = vs.props("VirtualMachine", vm, &["runtime.powerState", "config.hardware.numCPU", "config.hardware.memoryMB", "config.firmware", "guest.hostName", "config.guestFullName", "config.hardware.device", "name"]).await?;
    let g = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.text.trim().to_string()).unwrap_or_default();
    let mut nics = Vec::new();
    if let Some((_, dev)) = p.iter().find(|(n, _)| n == "config.hardware.device") {
        for d in dev.all("VirtualDevice") {
            let mac = d.t("macAddress");
            if mac.is_empty() { continue; }
            nics.push(json!({ "name": format!("nic{}", nics.len() + 1), "mac": mac.to_lowercase(), "label": d.child("deviceInfo").map(|x| x.t("label")).unwrap_or_default() }));
        }
    }
    let host = if g("guest.hostName").is_empty() { g("name") } else { g("guest.hostName") };
    let m = json!({ "hostname": host, "os": g("config.guestFullName"), "cpus": g("config.hardware.numCPU").parse::<u64>().unwrap_or(1),
        "memory": g("config.hardware.memoryMB").parse::<u64>().unwrap_or(1024) << 20, "nics": nics, "firmware": if g("config.firmware") == "efi" { "uefi" } else { "bios" }, "source": "vsphere" });
    Ok((m, g("runtime.powerState")))
}

async fn task(vs: &Vsphere, body: String, what: &str) -> Result<Node> {
    let t = vs.call(&body).await.with_context(|| what.to_string())?;
    vs.wait_task(&t.find("returnval").map(|x| x.text.clone()).unwrap_or_default(), what).await
}

/// Выключить гостевую ОС штатно (VMware Tools); `hard` — если не вышло, выключить ВМ.
async fn shutdown(vs: &Vsphere, vm: &str, timeout: u64, hard: bool, log: &LogFn) -> Result<()> {
    log("выключение гостевой ОС (VMware Tools)");
    let r = vs.call(&format!("<ShutdownGuest xmlns=\"urn:vim25\">{}</ShutdownGuest>", mo("VirtualMachine", vm))).await;
    if let Err(e) = &r { log(&format!("  штатное выключение не начато: {e:#}")); }
    let t0 = std::time::Instant::now();
    while r.is_ok() && t0.elapsed().as_secs() < timeout {
        if vs.prop("VirtualMachine", vm, "runtime.powerState").await.map(|v| v.text.trim() == "poweredOff").unwrap_or(false) { log(&format!("  ВМ выключена за {} с", t0.elapsed().as_secs())); return Ok(()); }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    if !hard { bail!("гостевая ОС не выключилась за {timeout} с — выключите ВМ вручную или разрешите жёсткое выключение"); }
    log("  жёсткое выключение ВМ");
    task(vs, format!("<PowerOffVM_Task xmlns=\"urn:vim25\">{}</PowerOffVM_Task>", mo("VirtualMachine", vm)), "выключение ВМ").await?;
    Ok(())
}

pub async fn backup(ctx: &BackupCtx) -> Result<BackupOutcome> {
    let log = &ctx.log;
    let cfg = &ctx.config;
    let name = cfg_str(cfg, "vm").ok_or_else(|| anyhow!("не задана ВМ vSphere (vm)"))?;
    let vs = Arc::new(Vsphere::login(cfg, log).await?);
    let vm = vs.find_vm(&name).await?;
    let cutover = ctx.kind == "cutover";
    let (prev_id, prev) = prev_maps(&ctx.repo, &ctx.instance).await?;
    let (mut mach, power) = machine(&vs, &vm).await?;
    log(&format!("реплика ВМ «{name}» без агента: {}{}", if prev_id.is_some() { "изменения" } else { "первая синхронизация" }, if cutover { " · переключение" } else { "" }));
    if cutover && power == "poweredOn" {
        shutdown(&vs, &vm, cfg.get("shutdown_timeout").and_then(|x| x.as_u64()).unwrap_or(600), kit::bool_param(cfg, "hard_poweroff", false), log).await?;
    }
    let on = !cutover && power == "poweredOn";
    let quiesce = on && kit::bool_param(cfg, "quiesce", true);
    let sname = format!("hopebackup-replica-{}", &crate::util::short_id()[..8]);
    let mk = |q: bool| format!("<CreateSnapshot_Task xmlns=\"urn:vim25\">{}<name>{sname}</name><description>HopeBackup: реплика для переезда</description><memory>false</memory><quiesce>{q}</quiesce></CreateSnapshot_Task>", mo("VirtualMachine", &vm));
    let info = match task(&vs, mk(quiesce), "снимок ВМ").await {
        Ok(i) => i,
        Err(e) if quiesce => { log(&format!("  снимок с заморозкой ФС не удался ({e:#}) — без заморозки")); task(&vs, mk(false), "снимок ВМ").await? }
        Err(e) => return Err(e),
    };
    let snap = info.t("result");
    let t0 = std::time::Instant::now();
    let mut w = ctx.repo.writer();
    let res = async {
        let devs = vs.prop("VirtualMachineSnapshot", &snap, "config.hardware.device").await.context("диски снимка")?;
        let disks = vdisks(&devs);
        let l = vs.call(&format!("<ExportSnapshot xmlns=\"urn:vim25\">{}</ExportSnapshot>", mo("VirtualMachineSnapshot", &snap))).await.context("экспорт снимка")?;
        let lease = l.find("returnval").map(|x| x.text.clone()).unwrap_or_default();
        let li = vs.wait_lease(&lease).await?;
        let total: u64 = disks.iter().map(|d| d.1).sum();
        let done = Arc::new(AtomicU64::new(0));
        let ka = vs.keepalive(lease.clone(), done.clone(), total);
        let r = async {
            let mut out = Vec::new();
            for du in li.all("deviceUrl").into_iter().filter(|d| d.t("disk") == "true") {
                let key = du.t("key").rsplit('/').next().unwrap_or("").to_string();
                let cap = disks.iter().find(|d| d.0 == key).map(|d| d.1).unwrap_or(0);
                let id = format!("vmdk{key}");
                let pm = match kit::stream(&prev, &format!("{id}.map")) { Ok(s) => kit::read_small(&ctx.repo, s).await.unwrap_or_default(), Err(_) => Vec::new() };
                let prev_size = prev.meta["disks"].as_array().and_then(|a| a.iter().find(|x| x["id"] == id.as_str())).and_then(|x| x["size"].as_u64());
                let resp = vs.c.get(vs.dev_url(&du.t("url"))).header("Cookie", vs.ck()).send().await.context("загрузка диска (NFC)")?;
                if !resp.status().is_success() { bail!("NFC {id}: {}", resp.status()); }
                let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
                let dn = done.clone();
                let pump = tokio::spawn(async move {
                    use futures::StreamExt;
                    let mut s = resp.bytes_stream();
                    while let Some(b) = s.next().await { let b = b?; dn.fetch_add(b.len() as u64, Ordering::Relaxed); if tx.send(b.to_vec()).await.is_err() { break; } }
                    Ok::<(), anyhow::Error>(())
                });
                let (pr, pw) = std::io::pipe()?;
                let st = Arc::new(parking_lot::Mutex::new(ScanStats::default()));
                let st2 = st.clone();
                let prod = std::thread::spawn(move || -> Result<(Vec<u8>, u64)> {
                    let mut src = super::vmdk::StreamOptReader::new(std::io::BufReader::with_capacity(1 << 20, super::vm::ChanRead::new(rx)))?;
                    let size = src.capacity;
                    let pm = if prev_size == Some(size) { pm } else { Vec::new() };
                    let mut s = ScanStats::default();
                    let map = scan(&mut src, size, &pm, std::io::BufWriter::with_capacity(1 << 20, pw), &mut s)?;
                    // Дочитать поток до конца (метаданные после последнего зерна).
                    let _ = std::io::copy(&mut src.into_inner(), &mut std::io::sink());
                    *st2.lock() = s;
                    Ok((map, size))
                });
                let a = w.add_stream(&format!("{id}.delta"), pr, None).await;
                let (map, size) = prod.join().map_err(|_| anyhow!("разбор VMDK аварийно завершился"))??;
                a?;
                pump.await??;
                kit::add_bytes(&mut w, &format!("{id}.map"), map).await?;
                let s = st.lock().clone();
                if cap != 0 && cap != size { log(&format!("  ! {id}: ёмкость в VMDK {size} ≠ {cap}")); }
                log(&format!("  {id}: {} · изменено блоков {} из {} ({})", crate::util::human_bytes(size), s.changed, s.blocks, crate::util::human_bytes(s.changed_bytes)));
                out.push(json!({ "id": id, "path": format!("vsphere:{name}/{key}"), "size": size, "changed": s.changed, "changed_bytes": s.changed_bytes, "zero_blocks": s.zero }));
            }
            Ok::<_, anyhow::Error>(out)
        }.await;
        ka.abort();
        match &r {
            Ok(_) => { let _ = vs.call(&format!("<HttpNfcLeaseComplete xmlns=\"urn:vim25\">{}</HttpNfcLeaseComplete>", mo("HttpNfcLease", &lease))).await; }
            Err(e) => { let _ = vs.call(&format!("<HttpNfcLeaseAbort xmlns=\"urn:vim25\">{}<fault><faultMessage><key>hb</key><message>{}</message></faultMessage></fault></HttpNfcLeaseAbort>", mo("HttpNfcLease", &lease), esc(&format!("{e:#}")))).await; }
        }
        r
    }.await;
    if let Err(e) = task(&vs, format!("<RemoveSnapshot_Task xmlns=\"urn:vim25\">{}<removeChildren>false</removeChildren><consolidate>true</consolidate></RemoveSnapshot_Task>", mo("VirtualMachineSnapshot", &snap)), "удаление снимка").await {
        log(&format!("! снимок {sname} не удалён: {e:#} — удалите его в vSphere"));
    }
    let disks = res?;
    if disks.is_empty() { bail!("у ВМ нет дисков для реплики"); }
    mach["vm"] = json!(name);
    let meta = json!({ "disks": disks, "machine": mach, "prev": prev_id, "cutover": cutover, "read_seconds": t0.elapsed().as_secs_f64(), "source": "vsphere" });
    let kind = if cutover { "cutover" } else { "sync" };
    let (header, stats) = w.finish(&ctx.instance, "replica", kind, json!({ "prev": prev_id, "disks": disks.len(), "source": "vsphere" }), meta).await?;
    Ok(BackupOutcome { header, stats })
}

/// Учётная запись гостевой ОС для гостевых операций.
fn guest_auth(cfg: &Value) -> Result<(String, String)> {
    let u = cfg_str(cfg, "guest_user").ok_or_else(|| anyhow!("для подготовки без агента нужна учётная запись гостевой ОС (guest_user, guest_password)"))?;
    let auth = format!("<auth xsi:type=\"NamePasswordAuthentication\"><interactiveSession>false</interactiveSession><username>{}</username><password>{}</password></auth>", esc(&u), esc(&cfg_str(cfg, "guest_password").unwrap_or_default()));
    Ok((u, auth))
}

/// Подготовка гостевой Linux к KVM через гостевые операции vSphere (VMware Tools): сценарий загружается
/// в гостевую ОС и выполняется от root (или через `sudo -n`), вывод — обратно.
pub async fn prepare(cfg: &Value, script: &str, log: &LogFn) -> Result<String> {
    let name = cfg_str(cfg, "vm").ok_or_else(|| anyhow!("не задана ВМ vSphere (vm)"))?;
    let (user, auth) = guest_auth(cfg)?;
    let vs = Vsphere::login(cfg, log).await?;
    let vm = vs.find_vm(&name).await?;
    let sc = vs.call(&format!("<RetrieveServiceContent xmlns=\"urn:vim25\">{}</RetrieveServiceContent>", mo("ServiceInstance", "ServiceInstance"))).await?;
    let gom = sc.find("returnval").map(|r| r.t("guestOperationsManager")).filter(|x| !x.is_empty()).ok_or_else(|| anyhow!("vSphere без гостевых операций"))?;
    let g = vs.props("GuestOperationsManager", &gom, &["fileManager", "processManager"]).await?;
    let fm = g.iter().find(|(n, _)| n == "fileManager").map(|(_, v)| v.text.trim().to_string()).unwrap_or_default();
    let pm = g.iter().find(|(n, _)| n == "processManager").map(|(_, v)| v.text.trim().to_string()).unwrap_or_default();
    let vmx = format!("<vm type=\"VirtualMachine\">{}</vm>", esc(&vm));
    let tag = &crate::util::short_id()[..8];
    let (sp, op) = (format!("/tmp/hb-prep-{tag}.sh"), format!("/tmp/hb-prep-{tag}.out"));
    let r = vs.call(&format!("<InitiateFileTransferToGuest xmlns=\"urn:vim25\">{}{vmx}{auth}<guestFilePath>{sp}</guestFilePath><fileAttributes xsi:type=\"GuestPosixFileAttributes\"/><fileSize>{}</fileSize><overwrite>true</overwrite></InitiateFileTransferToGuest>", mo("GuestFileManager", &fm), script.len())).await.context("загрузка сценария в гостевую ОС (VMware Tools работают?)")?;
    let url = vs.dev_url(&r.find("returnval").map(|x| x.text.trim().to_string()).unwrap_or_default());
    let pr = vs.c.put(&url).body(script.to_string()).send().await?;
    if !pr.status().is_success() { bail!("загрузка сценария: {}", pr.status()); }
    let cmd = if user == "root" { format!("sh {sp} > {op} 2>&1; rm -f {sp}") } else { format!("sudo -n sh {sp} > {op} 2>&1; rm -f {sp}") };
    let r = vs.call(&format!("<StartProgramInGuest xmlns=\"urn:vim25\">{}{vmx}{auth}<spec xsi:type=\"GuestProgramSpec\"><programPath>/bin/sh</programPath><arguments>-c {}</arguments></spec></StartProgramInGuest>", mo("GuestProcessManager", &pm), esc(&crate::util::shq(&cmd)))).await.context("запуск сценария в гостевой ОС")?;
    let pid = r.find("returnval").map(|x| x.text.trim().to_string()).unwrap_or_default();
    log(&format!("сценарий подготовки запущен в гостевой ОС (pid {pid})"));
    let t0 = std::time::Instant::now();
    loop {
        let l = vs.call(&format!("<ListProcessesInGuest xmlns=\"urn:vim25\">{}{vmx}{auth}<pids>{pid}</pids></ListProcessesInGuest>", mo("GuestProcessManager", &pm))).await?;
        if let Some(p) = l.find("returnval") { if !p.t("endTime").is_empty() { break; } }
        if t0.elapsed().as_secs() > 1800 { bail!("сценарий подготовки не завершился за 30 минут"); }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let r = vs.call(&format!("<InitiateFileTransferFromGuest xmlns=\"urn:vim25\">{}{vmx}{auth}<guestFilePath>{op}</guestFilePath></InitiateFileTransferFromGuest>", mo("GuestFileManager", &fm))).await.context("вывод сценария")?;
    let url = vs.dev_url(&r.find("url").map(|x| x.text.trim().to_string()).unwrap_or_default());
    let out = vs.c.get(&url).send().await?.text().await?;
    let _ = vs.call(&format!("<DeleteFileInGuest xmlns=\"urn:vim25\">{}{vmx}{auth}<filePath>{op}</filePath></DeleteFileInGuest>", mo("GuestFileManager", &fm))).await;
    Ok(out)
}

/// Включить ВМ источника (откат переезда или неудачное переключение).
pub async fn power_on(cfg: &Value, log: &LogFn) -> Result<Value> {
    let name = cfg_str(cfg, "vm").ok_or_else(|| anyhow!("не задана ВМ vSphere (vm)"))?;
    let vs = Vsphere::login(cfg, log).await?;
    let vm = vs.find_vm(&name).await?;
    if vs.prop("VirtualMachine", &vm, "runtime.powerState").await.map(|v| v.text.trim() == "poweredOn").unwrap_or(false) { return Ok(json!({ "vm": name, "already": true })); }
    task(&vs, format!("<PowerOnVM_Task xmlns=\"urn:vim25\">{}</PowerOnVM_Task>", mo("VirtualMachine", &vm)), "включение ВМ").await?;
    log(&format!("ВМ «{name}» включена"));
    Ok(json!({ "vm": name, "powered_on": true }))
}
