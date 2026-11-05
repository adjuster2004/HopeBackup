//! VMware vSphere (vCenter или отдельный ESXi, 6.5–8.x) через vSphere Web Services (SOAP /sdk).
//!
//! Копия: снимок ВМ (с заморозкой ФС через VMware Tools, если включено) → экспорт снимка (ExportSnapshot,
//! аренда NFC) — диски в формате streamOptimized VMDK потоком в репозиторий, описание OVF (OvfManager) →
//! удаление снимка. CBT: включается на ВМ, при каждой копии считается объём изменённых блоков с прошлой
//! копии (QueryChangedDiskAreas) — для отчёта; передаётся весь диск (чтение только изменённых блоков
//! требует VDDK), но в репозиторий попадают только новые блоки (дедупликация).
//! Восстановление: OVF + диски → CreateImportSpec → ImportVApp (аренда NFC, загрузка VMDK) в выбранный
//! хост/пул и datastore, по желанию — включение.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::xml::{self, esc, Node};
use super::{repo_body, vm_stream, Driver, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Vsphere { pub(crate) c: reqwest::Client, url: String, host: String, cookie: parking_lot::Mutex<String>, ver: String, pc: String, vmgr: String, root: String, ovf: String }

pub(crate) fn mo(kind: &str, v: &str) -> String { format!("<_this type=\"{kind}\">{}</_this>", esc(v)) }

impl Vsphere {
    pub async fn login(cfg: &Value, log: &LogFn) -> Result<Vsphere> {
        let base = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес vCenter/ESXi"))?.trim_end_matches('/').trim_end_matches("/sdk").to_string();
        let host = reqwest::Url::parse(&base).ok().and_then(|u| u.host_str().map(|s| s.to_string())).unwrap_or_default();
        let mut s = Vsphere { c: super::http(cfg)?, url: format!("{base}/sdk"), host, cookie: parking_lot::Mutex::new(String::new()), ver: "6.5".into(), pc: String::new(), vmgr: String::new(), root: String::new(), ovf: String::new() };
        let sc = s.call(&format!("<RetrieveServiceContent xmlns=\"urn:vim25\">{}</RetrieveServiceContent>", mo("ServiceInstance", "ServiceInstance"))).await?;
        let rv = sc.find("returnval").ok_or_else(|| anyhow!("vSphere: нет ServiceContent"))?.clone();
        s.pc = rv.t("propertyCollector"); s.vmgr = rv.t("viewManager"); s.root = rv.t("rootFolder"); s.ovf = rv.t("ovfManager");
        let about = rv.child("about").cloned().unwrap_or_default();
        s.ver = about.t("apiVersion");
        if s.ovf.is_empty() { bail!("vSphere {}: нет OvfManager", about.t("version")); }
        s.call(&format!("<Login xmlns=\"urn:vim25\">{}<userName>{}</userName><password>{}</password></Login>", mo("SessionManager", &rv.t("sessionManager")), esc(&cfg_str(cfg, "user").unwrap_or_default()), esc(&cfg_str(cfg, "password").unwrap_or_default()))).await.context("вход в vSphere")?;
        log(&format!("{} (API {})", about.t("fullName"), s.ver));
        Ok(s)
    }

    async fn call_raw(&self, body: &str) -> Result<String> {
        let env = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><soapenv:Body>{body}</soapenv:Body></soapenv:Envelope>");
        let mut r = self.c.post(&self.url).header("Content-Type", "text/xml; charset=utf-8").header("SOAPAction", format!("urn:vim25/{}", self.ver)).body(env);
        let ck = self.cookie.lock().clone();
        if !ck.is_empty() { r = r.header("Cookie", ck); }
        let resp = r.send().await.with_context(|| format!("vSphere {} недоступен", self.url))?;
        if let Some(c) = resp.headers().get("set-cookie").and_then(|v| v.to_str().ok()) { *self.cookie.lock() = c.split(';').next().unwrap_or("").to_string(); }
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        if !st.is_success() {
            let f = xml::parse(&t).ok().and_then(|d| d.find("faultstring").map(|x| x.text.clone())).unwrap_or_else(|| t.chars().take(300).collect());
            bail!("vSphere: {f}");
        }
        Ok(t)
    }
    pub(crate) async fn call(&self, body: &str) -> Result<Node> { xml::parse(&self.call_raw(body).await?) }

    /// Свойства объекта: имя → узел значения.
    pub(crate) async fn props(&self, kind: &str, id: &str, paths: &[&str]) -> Result<Vec<(String, Node)>> {
        let ps: String = paths.iter().map(|p| format!("<pathSet>{p}</pathSet>")).collect();
        let x = self.call(&format!("<RetrievePropertiesEx xmlns=\"urn:vim25\">{}<specSet><propSet><type>{kind}</type>{ps}</propSet><objectSet><obj type=\"{kind}\">{}</obj></objectSet></specSet><options/></RetrievePropertiesEx>", mo("PropertyCollector", &self.pc), esc(id))).await?;
        let mut out = Vec::new();
        if let Some(o) = x.find("objects") { for p in o.all("propSet") { if let Some(v) = p.child("val") { out.push((p.t("name"), v.clone())); } } }
        Ok(out)
    }
    pub(crate) async fn prop(&self, kind: &str, id: &str, path: &str) -> Result<Node> {
        self.props(kind, id, &[path]).await?.into_iter().find(|(n, _)| n == path).map(|(_, v)| v).ok_or_else(|| anyhow!("vSphere: нет свойства {path} у {kind} {id}"))
    }

    /// Объекты типа во всём инвентаре: (moref, свойства).
    async fn inventory(&self, kind: &str, paths: &[&str]) -> Result<Vec<(String, Vec<(String, Node)>)>> {
        let v = self.call(&format!("<CreateContainerView xmlns=\"urn:vim25\">{}<container type=\"Folder\">{}</container><type>{kind}</type><recursive>true</recursive></CreateContainerView>", mo("ViewManager", &self.vmgr), esc(&self.root))).await?;
        let view = v.find("returnval").map(|x| x.text.trim().to_string()).unwrap_or_default();
        let ps: String = paths.iter().map(|p| format!("<pathSet>{p}</pathSet>")).collect();
        let x = self.call(&format!("<RetrievePropertiesEx xmlns=\"urn:vim25\">{}<specSet><propSet><type>{kind}</type>{ps}</propSet><objectSet><obj type=\"ContainerView\">{}</obj><skip>true</skip><selectSet xsi:type=\"TraversalSpec\"><name>v</name><type>ContainerView</type><path>view</path><skip>false</skip></selectSet></objectSet></specSet><options/></RetrievePropertiesEx>", mo("PropertyCollector", &self.pc), esc(&view))).await;
        let _ = self.call(&format!("<DestroyView xmlns=\"urn:vim25\">{}</DestroyView>", mo("ContainerView", &view))).await;
        let x = x?;
        let mut out = Vec::new();
        let mut objs = Vec::new();
        x.find_all("objects", &mut objs);
        for o in objs { out.push((o.t("obj"), o.all("propSet").iter().filter_map(|p| p.child("val").map(|v| (p.t("name"), v.clone()))).collect())); }
        Ok(out)
    }

    pub(crate) async fn wait_task(&self, task: &str, what: &str) -> Result<Node> {
        let t0 = std::time::Instant::now();
        loop {
            let info = self.prop("Task", task, "info").await?;
            match info.t("state").as_str() {
                "success" => return Ok(info),
                "error" => bail!("{what}: {}", info.child("error").map(|e| e.t("localizedMessage")).unwrap_or_default()),
                _ => {}
            }
            if t0.elapsed().as_secs() > 6 * 3600 { bail!("{what}: задача не завершилась"); }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    pub(crate) async fn wait_lease(&self, lease: &str) -> Result<Node> {
        for _ in 0..900 {
            let st = self.prop("HttpNfcLease", lease, "state").await?;
            match st.text.trim() {
                "ready" => return self.prop("HttpNfcLease", lease, "info").await,
                "error" => { let e = self.prop("HttpNfcLease", lease, "error").await.map(|e| e.t("localizedMessage")).unwrap_or_default(); bail!("аренда NFC: {e}"); }
                _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }
        bail!("аренда NFC не готова за 15 минут")
    }

    /// Держать аренду NFC живой (иначе она истекает через 5 минут без отчёта о ходе).
    pub(crate) fn keepalive(self: &Arc<Self>, lease: String, done: Arc<AtomicU64>, total: u64) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let pct = if total == 0 { 0 } else { (done.load(Ordering::Relaxed) * 100 / total).min(99) };
                if me.call(&format!("<HttpNfcLeaseProgress xmlns=\"urn:vim25\">{}<percent>{pct}</percent></HttpNfcLeaseProgress>", mo("HttpNfcLease", &lease))).await.is_err() { break; }
            }
        })
    }

    pub(crate) fn ck(&self) -> String { self.cookie.lock().clone() }

    pub(crate) fn dev_url(&self, u: &str) -> String { u.replacen("://*", &format!("://{}", self.host), 1) }

    pub(crate) async fn find_vm(&self, name: &str) -> Result<String> {
        let all = self.inventory("VirtualMachine", &["name"]).await?;
        all.into_iter().find(|(_, p)| p.iter().any(|(_, v)| v.text == name)).map(|(m, _)| m).ok_or_else(|| anyhow!("ВМ «{name}» не найдена"))
    }
}

/// Диски ВМ из config.hardware.device: (ключ устройства, ёмкость, changeId).
pub(crate) fn vdisks(dev: &Node) -> Vec<(String, u64, String)> {
    dev.all("VirtualDevice").into_iter().filter(|d| d.attr("type") == Some("VirtualDisk"))
        .map(|d| (d.t("key"), d.t("capacityInBytes").parse().unwrap_or(0), d.child("backing").map(|b| b.t("changeId")).unwrap_or_default())).collect()
}

/// Вырезать исходный XML элемента `<tag …>…</tag>` из ответа (для передачи обратно без разбора).
fn raw_elem<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let a = s.find(&format!("<{tag}"))?;
    let close = format!("</{tag}>");
    let b = s[a..].find(&close)? + a + close.len();
    Some(&s[a..b])
}

#[async_trait]
impl Driver for Arc<Vsphere> {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let all = self.inventory("VirtualMachine", &["name", "runtime.powerState", "config.template"]).await?;
        Ok(all.into_iter().filter(|(_, p)| !p.iter().any(|(n, v)| n == "config.template" && v.text == "true")).map(|(m, p)| {
            let g = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.text.trim().to_string()).unwrap_or_default();
            VmRef { id: m, name: g("name"), state: g("runtime.powerState"), extra: Value::Null }
        }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> {
        let cbt = super::super::appkit::bool_param(cfg, "cbt", true);
        if cbt && self.prop("VirtualMachine", &vm.id, "config.changeTrackingEnabled").await.map(|v| v.text.trim() != "true").unwrap_or(false) {
            let t = self.call(&format!("<ReconfigVM_Task xmlns=\"urn:vim25\">{}<spec><changeTrackingEnabled>true</changeTrackingEnabled></spec></ReconfigVM_Task>", mo("VirtualMachine", &vm.id))).await?;
            self.wait_task(&t.find("returnval").map(|x| x.text.clone()).unwrap_or_default(), "включение CBT").await?;
            log("  CBT включён (учёт изменений — со следующей копии)");
        }
        let quiesce = super::super::appkit::bool_param(cfg, "quiesce", true) && vm.state == "poweredOn";
        let snap_name = format!("hopebackup-{}", &crate::util::short_id()[..8]);
        let mk = |q: bool| format!("<CreateSnapshot_Task xmlns=\"urn:vim25\">{}<name>{snap_name}</name><description>HopeBackup</description><memory>false</memory><quiesce>{q}</quiesce></CreateSnapshot_Task>", mo("VirtualMachine", &vm.id));
        let t = self.call(&mk(quiesce)).await?;
        let task = t.find("returnval").map(|x| x.text.clone()).unwrap_or_default();
        let info = match self.wait_task(&task, "снимок ВМ").await {
            Ok(i) => i,
            Err(e) if quiesce => {
                log(&format!("  снимок с заморозкой ФС не удался ({e:#}) — без заморозки"));
                let t = self.call(&mk(false)).await?;
                self.wait_task(&t.find("returnval").map(|x| x.text.clone()).unwrap_or_default(), "снимок ВМ").await?
            }
            Err(e) => return Err(e),
        };
        let snap = info.t("result");
        let res = async {
            // CBT: сколько изменилось с прошлой копии.
            let devs = self.prop("VirtualMachineSnapshot", &snap, "config.hardware.device").await.ok();
            let disks = devs.as_ref().map(vdisks).unwrap_or_default();
            let prev: Value = cfg["_prev"].clone();
            let mut changed = 0u64;
            let mut change_ids = Vec::new();
            for (key, cap, cid) in &disks {
                let old = prev["change_ids"].as_array().and_then(|a| a.iter().find(|x| x["key"] == key.as_str())).and_then(|x| x["change_id"].as_str()).unwrap_or("*").to_string();
                if cbt && !cid.is_empty() {
                    let mut off = 0u64;
                    while off < *cap {
                        let r = self.call(&format!("<QueryChangedDiskAreas xmlns=\"urn:vim25\">{}<snapshot type=\"VirtualMachineSnapshot\">{}</snapshot><deviceKey>{key}</deviceKey><startOffset>{off}</startOffset><changeId>{}</changeId></QueryChangedDiskAreas>", mo("VirtualMachine", &vm.id), esc(&snap), esc(&old))).await;
                        let Ok(r) = r else { break };
                        let rv = r.find("returnval").cloned().unwrap_or_default();
                        for a in rv.all("changedArea") { changed += a.t("length").parse::<u64>().unwrap_or(0); }
                        let len: u64 = rv.t("length").parse().unwrap_or(0);
                        if len == 0 { break; }
                        off = rv.t("startOffset").parse::<u64>().unwrap_or(off) + len;
                    }
                }
                change_ids.push(json!({ "key": key, "change_id": cid, "capacity": cap }));
            }
            if cbt && !disks.is_empty() { log(&format!("  CBT: изменено {} с {}", crate::util::human_bytes(changed), if prev["change_ids"].is_null() { "создания дисков".to_string() } else { "прошлой копии".into() })); }
            let l = self.call(&format!("<ExportSnapshot xmlns=\"urn:vim25\">{}</ExportSnapshot>", mo("VirtualMachineSnapshot", &snap))).await.context("экспорт снимка (ExportSnapshot)")?;
            let lease = l.find("returnval").map(|x| x.text.clone()).unwrap_or_default();
            let li = self.wait_lease(&lease).await?;
            let total = li.t("totalDiskCapacityInKB").parse::<u64>().unwrap_or(0) * 1024;
            let done = Arc::new(AtomicU64::new(0));
            let ka = self.keepalive(lease.clone(), done.clone(), total);
            let r = async {
                let mut out = Vec::new();
                for (i, du) in li.all("deviceUrl").into_iter().filter(|d| d.t("disk") == "true").enumerate() {
                    let fname = format!("disk-{i}.vmdk");
                    let url = self.dev_url(&du.t("url"));
                    let e = super::http_to_stream(w, &format!("{prefix}{fname}"), self.c.get(&url).header("Cookie", self.ck()), log).await?;
                    done.fetch_add(e.size, Ordering::Relaxed);
                    out.push(json!({ "stream": fname, "target_id": du.t("targetId"), "key": du.t("key"), "size": e.size }));
                }
                // Описание OVF с теми же именами файлов дисков.
                let files: String = out.iter().map(|d| format!("<ovfFiles><deviceId>{}</deviceId><path>{}</path><size>{}</size></ovfFiles>", esc(d["target_id"].as_str().unwrap_or("")), d["stream"].as_str().unwrap_or(""), d["size"])).collect();
                let od = self.call(&format!("<CreateDescriptor xmlns=\"urn:vim25\">{}<obj type=\"VirtualMachine\">{}</obj><cdp>{files}<includeImageFiles>false</includeImageFiles></cdp></CreateDescriptor>", mo("OvfManager", &self.ovf), esc(&vm.id))).await.context("описание OVF")?;
                let ovf = od.find("ovfDescriptor").map(|x| x.text.clone()).ok_or_else(|| anyhow!("vSphere не вернул OVF"))?;
                super::super::appkit::add_bytes(w, &format!("{prefix}vm.ovf"), ovf.into_bytes()).await?;
                Ok::<_, anyhow::Error>(out)
            }.await;
            ka.abort();
            match &r {
                Ok(_) => { let _ = self.call(&format!("<HttpNfcLeaseComplete xmlns=\"urn:vim25\">{}</HttpNfcLeaseComplete>", mo("HttpNfcLease", &lease))).await; }
                Err(e) => { let _ = self.call(&format!("<HttpNfcLeaseAbort xmlns=\"urn:vim25\">{}<fault><faultMessage><key>hb</key><message>{}</message></faultMessage></fault></HttpNfcLeaseAbort>", mo("HttpNfcLease", &lease), esc(&format!("{e:#}")))).await; }
            }
            Ok::<_, anyhow::Error>((r?, change_ids, changed))
        }.await;
        // Снимок удаляется в любом случае.
        if let Ok(t) = self.call(&format!("<RemoveSnapshot_Task xmlns=\"urn:vim25\">{}<removeChildren>false</removeChildren><consolidate>true</consolidate></RemoveSnapshot_Task>", mo("VirtualMachineSnapshot", &snap))).await {
            if let Err(e) = self.wait_task(&t.find("returnval").map(|x| x.text.clone()).unwrap_or_default(), "удаление снимка").await { log(&format!("! снимок {snap_name} не удалён: {e:#} — удалите его в vSphere")); }
        }
        let (disks, change_ids, changed) = res?;
        Ok(json!({ "kind": "vmware", "disks": disks, "change_ids": change_ids, "cbt_changed_bytes": changed }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let ovf = String::from_utf8(super::super::appkit::read_small(repo, vm_stream(m, vm, "vm.ovf")?).await?)?;
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("").to_string());
        if self.find_vm(&name).await.is_ok() && !super::super::appkit::bool_param(p, "replace", false) { bail!("ВМ «{name}» уже есть: задайте новое имя"); }
        // Куда: хост или ресурсный пул (параметр «Узел»), datastore (параметр «Хранилище»), папка ВМ ЦОД.
        let hosts = self.inventory("HostSystem", &["name", "parent"]).await?;
        let pools = self.inventory("ResourcePool", &["name"]).await?;
        let node = cfg_str(p, "node");
        let (pool, host) = match node.as_deref().and_then(|n| hosts.iter().find(|(_, pr)| pr.iter().any(|(k, v)| k == "name" && v.text == n))) {
            Some((h, pr)) => {
                let parent = pr.iter().find(|(k, _)| k == "parent").map(|(_, v)| v.text.clone()).unwrap_or_default();
                let ptype = pr.iter().find(|(k, _)| k == "parent").and_then(|(_, v)| v.attr("type").map(|s| s.to_string())).unwrap_or_else(|| "ComputeResource".into());
                (self.prop(&ptype, &parent, "resourcePool").await?.text.trim().to_string(), Some(h.clone()))
            }
            None => {
                let rp = match node.as_deref() { Some(n) => pools.iter().find(|(_, pr)| pr.iter().any(|(_, v)| v.text == n)).map(|x| x.0.clone()).ok_or_else(|| anyhow!("нет хоста или пула «{n}»"))?, None => pools.first().map(|x| x.0.clone()).ok_or_else(|| anyhow!("нет ресурсных пулов"))? };
                (rp, None)
            }
        };
        let dss = self.inventory("Datastore", &["name"]).await?;
        let ds = match cfg_str(p, "storage") { Some(n) => dss.iter().find(|(_, pr)| pr.iter().any(|(_, v)| v.text == n)).map(|x| x.0.clone()).ok_or_else(|| anyhow!("нет datastore «{n}»"))?, None => dss.first().map(|x| x.0.clone()).ok_or_else(|| anyhow!("нет datastore"))? };
        let dcs = self.inventory("Datacenter", &["vmFolder"]).await?;
        let folder = dcs.first().and_then(|(_, pr)| pr.first().map(|(_, v)| v.text.trim().to_string())).ok_or_else(|| anyhow!("нет ЦОД"))?;
        let mut netmap = String::new();
        if let Some(n) = cfg_str(p, "network") {
            let nets = self.inventory("Network", &["name"]).await?;
            let nm = nets.iter().find(|(_, pr)| pr.iter().any(|(_, v)| v.text == n)).map(|x| x.0.clone()).ok_or_else(|| anyhow!("нет сети «{n}»"))?;
            let d = xml::parse(&ovf)?;
            let mut ns = Vec::new();
            d.find_all("Network", &mut ns);
            for x in ns { if let Some(on) = x.attr("name") { netmap.push_str(&format!("<networkMapping><name>{}</name><network type=\"Network\">{}</network></networkMapping>", esc(on), esc(&nm))); } }
        }
        let cis = self.call_raw(&format!("<CreateImportSpec xmlns=\"urn:vim25\">{}<ovfDescriptor>{}</ovfDescriptor><resourcePool type=\"ResourcePool\">{}</resourcePool><datastore type=\"Datastore\">{}</datastore><cisp><locale></locale><deploymentOption></deploymentOption><entityName>{}</entityName>{netmap}<diskProvisioning>thin</diskProvisioning></cisp></CreateImportSpec>",
            mo("OvfManager", &self.ovf), esc(&ovf), esc(&pool), esc(&ds), esc(&name))).await.context("CreateImportSpec")?;
        let cd = xml::parse(&cis)?;
        if let Some(e) = cd.find("error") { bail!("OVF не подходит: {}", e.t("localizedMessage")); }
        let spec_raw = raw_elem(&cis, "importSpec").ok_or_else(|| anyhow!("vSphere не вернул importSpec"))?;
        let spec = format!("<spec{}", &spec_raw["<importSpec".len()..spec_raw.len() - "</importSpec>".len()]) + "</spec>";
        let items: Vec<(String, String)> = cd.find("returnval").map(|r| r.all("fileItem").iter().map(|f| (f.t("deviceId"), f.t("path"))).collect()).unwrap_or_default();
        let host_x = host.map(|h| format!("<host type=\"HostSystem\">{}</host>", esc(&h))).unwrap_or_default();
        let l = self.call(&format!("<ImportVApp xmlns=\"urn:vim25\">{}{spec}<folder type=\"Folder\">{}</folder>{host_x}</ImportVApp>", mo("ResourcePool", &pool), esc(&folder))).await.context("ImportVApp")?;
        let lease = l.find("returnval").map(|x| x.text.clone()).unwrap_or_default();
        let li = self.wait_lease(&lease).await?;
        let total: u64 = vm["disks"].as_array().into_iter().flatten().map(|d| d["size"].as_u64().unwrap_or(0)).sum();
        let done = Arc::new(AtomicU64::new(0));
        let ka = self.keepalive(lease.clone(), done.clone(), total);
        let r = async {
            for du in li.all("deviceUrl") {
                let key = du.t("importKey");
                let Some((_, path)) = items.iter().find(|(d, _)| *d == key) else { continue };
                let s = vm_stream(m, vm, path)?;
                let (body, fetch) = repo_body(repo, s)?;
                log(&format!("  загрузка {path} ({})", crate::util::human_bytes(s.size)));
                let resp = self.c.post(self.dev_url(&du.t("url"))).header("Content-Type", "application/x-vnd.vmware-streamVmdk").header("Content-Length", s.size.to_string()).header("Cookie", self.ck()).body(body).send().await;
                fetch.await??;
                let resp = resp?;
                if !resp.status().is_success() { bail!("NFC: {} {}", resp.status(), resp.text().await.unwrap_or_default()); }
                done.fetch_add(s.size, Ordering::Relaxed);
            }
            Ok::<(), anyhow::Error>(())
        }.await;
        ka.abort();
        if let Err(e) = r {
            let _ = self.call(&format!("<HttpNfcLeaseAbort xmlns=\"urn:vim25\">{}</HttpNfcLeaseAbort>", mo("HttpNfcLease", &lease))).await;
            return Err(e);
        }
        self.call(&format!("<HttpNfcLeaseComplete xmlns=\"urn:vim25\">{}</HttpNfcLeaseComplete>", mo("HttpNfcLease", &lease))).await?;
        let new_vm = li.t("entity");
        if super::super::appkit::bool_param(p, "start", false) {
            let t = self.call(&format!("<PowerOnVM_Task xmlns=\"urn:vim25\">{}</PowerOnVM_Task>", mo("VirtualMachine", &new_vm))).await?;
            self.wait_task(&t.find("returnval").map(|x| x.text.clone()).unwrap_or_default(), "включение ВМ").await?;
        }
        log(&format!("  ВМ {name} импортирована ({new_vm})"));
        Ok(json!({ "vm": name, "id": new_vm }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_and_disks() {
        let s = r#"<returnval><importSpec xsi:type="VirtualMachineImportSpec"><configSpec><name>x</name></configSpec></importSpec><fileItem/></returnval>"#;
        assert_eq!(raw_elem(s, "importSpec").unwrap(), r#"<importSpec xsi:type="VirtualMachineImportSpec"><configSpec><name>x</name></configSpec></importSpec>"#);
        let d = xml::parse(r#"<val><VirtualDevice xsi:type="VirtualDisk"><key>2000</key><capacityInBytes>1024</capacityInBytes><backing><changeId>52 a/7</changeId></backing></VirtualDevice><VirtualDevice xsi:type="VirtualCdrom"><key>3000</key></VirtualDevice></val>"#).unwrap();
        let v = vdisks(d.child("val").unwrap());
        assert_eq!(v, vec![("2000".to_string(), 1024, "52 a/7".to_string())]);
    }
}
