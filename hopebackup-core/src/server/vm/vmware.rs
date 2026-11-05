//! VMware vSphere (vCenter 7.0+): REST API `/api`.
//!
//! Источник — выключенная ВМ-эталон (`clone`) или шаблон библиотеки содержимого (`deploy`).
//! Настройка гостя: cloud-init через спецификацию настройки (vSphere 7.0 U3+: пользователь, ключ,
//! сеть), готовая спецификация vCenter по имени или никакой (ключ уже в эталоне).

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{api_error, cloud_config, netplan, p_raw, p_str, p_u64, static_ip, Provider, VmInfo, VmRequest};
use crate::plugins::LogFn;

pub struct Vmware {
    c: reqwest::Client,
    base: String,
    p: Value,
}

impl Vmware {
    pub fn new(p: Value) -> Result<Vmware> {
        let url = p_str(&p, "url");
        if url.is_empty() { bail!("не задан адрес vCenter"); }
        if p_str(&p, "username").is_empty() || p_raw(&p, "password").is_empty() { bail!("не заданы пользователь и пароль vCenter"); }
        Ok(Vmware { c: super::http(&p_str(&p, "fingerprint"))?, base: format!("{}/api", url.trim_end_matches('/').trim_end_matches("/api")), p })
    }

    async fn session(&self) -> Result<String> {
        let r = self.c.post(format!("{}/session", self.base)).basic_auth(p_str(&self.p, "username"), Some(p_raw(&self.p, "password"))).timeout(Duration::from_secs(30)).send().await.map_err(|e| anyhow!("vCenter: {e}"))?;
        if !r.status().is_success() { return Err(api_error(r, "vCenter: вход").await); }
        let v: Value = r.json().await?;
        v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow!("vCenter: неожиданный ответ на вход"))
    }

    async fn call(&self, sid: &str, m: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut r = self.c.request(m.clone(), format!("{}{path}", self.base)).header("vmware-api-session-id", sid).timeout(Duration::from_secs(120));
        if let Some(b) = body { r = r.json(&b); }
        let resp = r.send().await.map_err(|e| anyhow!("vCenter {path}: {e}"))?;
        if !resp.status().is_success() { return Err(api_error(resp, &format!("vCenter {m} {path}")).await); }
        let t = resp.text().await?;
        Ok(if t.trim().is_empty() { Value::Null } else { serde_json::from_str(&t).unwrap_or(Value::String(t)) })
    }

    /// Идентификатор объекта инвентаря по имени (folder, resource-pool, datastore, cluster, host, vm).
    async fn find(&self, sid: &str, kind: &str, name: &str, extra: &str) -> Result<String> {
        let key = match kind { "resource-pool" => "resource_pool", k => k };
        let v = self.call(sid, reqwest::Method::GET, &format!("/vcenter/{kind}?names={}{extra}", super::pct(name)), None).await?;
        let a = v.as_array().cloned().unwrap_or_default();
        match a.len() {
            0 => bail!("vCenter: не найден объект {kind} «{name}»"),
            1 => a[0][key].as_str().map(|s| s.to_string()).ok_or_else(|| anyhow!("vCenter: в ответе нет поля {key}")),
            _ => bail!("vCenter: объектов {kind} с именем «{name}» несколько — уточните"),
        }
    }

    async fn placement(&self, sid: &str) -> Result<serde_json::Map<String, Value>> {
        let mut m = serde_json::Map::new();
        for (param, kind, key) in [("folder", "folder", "folder"), ("resource_pool", "resource-pool", "resource_pool"), ("cluster", "cluster", "cluster"), ("host", "host", "host"), ("datastore", "datastore", "datastore")] {
            let name = p_str(&self.p, param);
            if name.is_empty() { continue; }
            let extra = if kind == "folder" { "&type=VIRTUAL_MACHINE" } else { "" };
            m.insert(key.into(), json!(self.find(sid, kind, &name, extra).await?));
        }
        Ok(m)
    }

    async fn library_item(&self, sid: &str, name: &str) -> Result<String> {
        let v = self.call(sid, reqwest::Method::POST, "/content/library/item?action=find", Some(json!({ "name": name }))).await?;
        let a = v.as_array().cloned().unwrap_or_default();
        a.first().and_then(|x| x.as_str()).map(|s| s.to_string()).ok_or_else(|| anyhow!("vCenter: в библиотеках нет шаблона «{name}»"))
    }

    async fn guest_ip(&self, sid: &str, vm: &str, timeout: Duration, log: &LogFn) -> Result<String> {
        let started = std::time::Instant::now();
        let mut said = false;
        loop {
            if let Ok(v) = self.call(sid, reqwest::Method::GET, &format!("/vcenter/vm/{vm}/guest/identity"), None).await {
                if let Some(ip) = v["ip_address"].as_str() { if !ip.is_empty() && !ip.contains(':') && !ip.starts_with("169.254.") { return Ok(ip.to_string()); } }
            }
            if !said { log("ожидание адреса от VMware Tools…"); said = true; }
            if started.elapsed() > timeout { bail!("vCenter: адрес ВМ не получен за {} с — в эталоне нужны VMware Tools (open-vm-tools) или задайте статический адрес", timeout.as_secs()); }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Спецификация настройки гостя через cloud-init (vSphere 7.0 U3+).
pub fn cloudinit_spec(req: &VmRequest) -> Result<Value> {
    let meta = format!("instance-id: {name}\nlocal-hostname: {name}\n{net}", name = req.name, net = netplan(&req.p)?);
    Ok(json!({ "spec": {
        "configuration_spec": { "cloud_config": { "type": "CLOUDINIT", "cloudinit": { "metadata": meta, "userdata": cloud_config(&req.user, &req.ssh_pubkey, &req.name) } } },
        "interfaces": [],
        "global_DNS_settings": {}
    } }))
}

#[async_trait]
impl Provider for Vmware {
    fn title(&self) -> &'static str { "VMware vSphere" }

    async fn test(&self) -> Result<String> {
        let sid = self.session().await?;
        let src = p_str(&self.p, "source");
        let what = if p_str(&self.p, "source_type") == "library" {
            format!("шаблон библиотеки «{src}» ({})", self.library_item(&sid, &src).await?)
        } else {
            format!("эталон «{src}» ({})", self.find(&sid, "vm", &src, "").await?)
        };
        self.placement(&sid).await?;
        let _ = self.call(&sid, reqwest::Method::DELETE, "/session", None).await;
        Ok(format!("vCenter: вход выполнен, {what}, размещение найдено"))
    }

    async fn create(&self, req: &VmRequest, log: &LogFn) -> Result<VmInfo> {
        let p = &req.p;
        let sid = self.session().await?;
        let src = p_str(p, "source");
        let placement = self.placement(&sid).await?;
        let lib = p_str(p, "source_type") == "library";
        let vm = if lib {
            let item = self.library_item(&sid, &src).await?;
            let mut pl = placement.clone();
            let ds = pl.remove("datastore");
            let mut body = json!({ "name": req.name, "placement": pl, "powered_on": false, "description": "HopeBackup: восстановление" });
            if let Some(d) = ds { body["disk_storage"] = json!({ "datastore": d }); }
            log(&format!("развёртывание шаблона библиотеки «{src}»"));
            self.call(&sid, reqwest::Method::POST, &format!("/vcenter/vm-template/library-items/{item}?action=deploy"), Some(body)).await?
        } else {
            let source = self.find(&sid, "vm", &src, "").await?;
            log(&format!("клонирование эталона «{src}» ({source})"));
            self.call(&sid, reqwest::Method::POST, "/vcenter/vm?action=clone", Some(json!({ "name": req.name, "source": source, "placement": placement, "power_on": false }))).await?
        };
        let vm = vm.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow!("vCenter: не вернул идентификатор новой ВМ"))?;
        let r: Result<VmInfo> = async {
            self.call(&sid, reqwest::Method::PATCH, &format!("/vcenter/vm/{vm}/hardware/cpu"), Some(json!({ "count": p_u64(p, "cores", 2) }))).await?;
            self.call(&sid, reqwest::Method::PATCH, &format!("/vcenter/vm/{vm}/hardware/memory"), Some(json!({ "size_MiB": p_u64(p, "memory_mb", 4096) }))).await?;
            let gb = p_u64(p, "disk_gb", 0);
            if gb > 0 {
                let disks = self.call(&sid, reqwest::Method::GET, &format!("/vcenter/vm/{vm}/hardware/disk"), None).await?;
                if let Some(d) = disks.as_array().and_then(|a| a.first()).and_then(|d| d["disk"].as_str()) {
                    let info = self.call(&sid, reqwest::Method::GET, &format!("/vcenter/vm/{vm}/hardware/disk/{d}"), None).await?;
                    let cur = info["capacity"].as_u64().unwrap_or(0);
                    if cur < gb << 30 { log("vCenter REST не меняет размер диска — увеличьте диск эталона или расширьте после восстановления"); }
                }
            }
            match p_str(p, "customization").as_str() {
                "spec" => {
                    let n = p_str(p, "spec_name");
                    if n.is_empty() { bail!("укажите имя спецификации настройки"); }
                    self.call(&sid, reqwest::Method::PUT, &format!("/vcenter/vm/{vm}/guest/customization"), Some(json!({ "name": n }))).await?;
                }
                "none" => {}
                _ => { self.call(&sid, reqwest::Method::PUT, &format!("/vcenter/vm/{vm}/guest/customization"), Some(cloudinit_spec(req)?)).await?; }
            }
            log(&format!("включение ВМ {vm}"));
            self.call(&sid, reqwest::Method::POST, &format!("/vcenter/vm/{vm}/power?action=start"), None).await?;
            let ip = match static_ip(p)? { Some((a, _)) => a, None => self.guest_ip(&sid, &vm, Duration::from_secs(p_u64(p, "boot_timeout", 900)), log).await? };
            Ok(VmInfo { id: vm.clone(), ip, details: json!({ "vm": vm, "source": src }) })
        }
        .await;
        let _ = self.call(&sid, reqwest::Method::DELETE, "/session", None).await;
        match r {
            Ok(v) => Ok(v),
            Err(e) => {
                if super::p_bool(p, "destroy_on_failure", true) { let _ = self.destroy(&vm, log).await; } else { log(&format!("ВМ {vm} оставлена для разбора")); }
                Err(e)
            }
        }
    }

    async fn destroy(&self, id: &str, log: &LogFn) -> Result<()> {
        let sid = self.session().await?;
        let _ = self.call(&sid, reqwest::Method::POST, &format!("/vcenter/vm/{id}/power?action=stop"), None).await;
        let r = self.call(&sid, reqwest::Method::DELETE, &format!("/vcenter/vm/{id}"), None).await;
        let _ = self.call(&sid, reqwest::Method::DELETE, "/session", None).await;
        r?;
        log(&format!("ВМ {id} удалена"));
        Ok(())
    }
}
