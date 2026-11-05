//! Proxmox VE: клон шаблона с cloud-init через REST API (/api2/json), вход по API-токену.
//!
//! Порядок: nextid → clone (задача) → config (vCPU, память, ciuser, sshkeys, ipconfig0, мост) →
//! resize диска → start (задача) → адрес (статический или через qemu-guest-agent).

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{api_error, p_bool, p_str, p_u64, pct, static_ip, Provider, VmInfo, VmRequest};
use crate::plugins::LogFn;

pub struct Proxmox {
    c: reqwest::Client,
    base: String,
    auth: String,
    node: String,
    p: Value,
}

impl Proxmox {
    pub fn new(p: Value) -> Result<Proxmox> {
        let url = p_str(&p, "url");
        if url.is_empty() { bail!("не задан адрес Proxmox"); }
        let tid = p_str(&p, "token_id");
        let sec = p_str(&p, "token_secret");
        if tid.is_empty() || sec.is_empty() { bail!("не задан API-токен Proxmox (token_id и token_secret)"); }
        if !tid.contains('!') { bail!("token_id — в виде пользователь@область!имя, например root@pam!hopebackup"); }
        let node = { let n = p_str(&p, "node"); if n.is_empty() { "pve".to_string() } else { n } };
        Ok(Proxmox {
            c: super::http(&p_str(&p, "fingerprint"))?,
            base: format!("{}/api2/json", url.trim_end_matches('/').trim_end_matches("/api2/json")),
            auth: format!("PVEAPIToken={tid}={sec}"),
            node,
            p,
        })
    }

    async fn call(&self, m: reqwest::Method, path: &str, form: Option<Vec<(String, String)>>) -> Result<Value> {
        let mut r = self.c.request(m.clone(), format!("{}{path}", self.base)).header("Authorization", &self.auth).timeout(Duration::from_secs(60));
        if let Some(f) = form { r = r.form(&f); }
        let resp = r.send().await.map_err(|e| anyhow!("Proxmox {path}: {e}"))?;
        if !resp.status().is_success() { return Err(api_error(resp, &format!("Proxmox {m} {path}")).await); }
        let v: Value = resp.json().await?;
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> { self.call(reqwest::Method::GET, path, None).await }
    async fn post(&self, path: &str, form: Vec<(&str, String)>) -> Result<Value> { self.call(reqwest::Method::POST, path, Some(form.into_iter().map(|(k, v)| (k.to_string(), v)).collect())).await }

    /// Дождаться завершения задачи Proxmox (UPID).
    async fn wait_task(&self, upid: &Value, what: &str, timeout: Duration) -> Result<()> {
        let Some(upid) = upid.as_str() else { return Ok(()) };
        let started = std::time::Instant::now();
        loop {
            let st = self.get(&format!("/nodes/{}/tasks/{}/status", self.node, pct(upid))).await?;
            if st["status"] == "stopped" {
                let ex = st["exitstatus"].as_str().unwrap_or("");
                if ex == "OK" || ex.starts_with("WARNINGS") { return Ok(()); }
                let log = self.get(&format!("/nodes/{}/tasks/{}/log?limit=20", self.node, pct(upid))).await.ok().and_then(|v| v.as_array().map(|a| a.iter().filter_map(|l| l["t"].as_str()).collect::<Vec<_>>().join(" | "))).unwrap_or_default();
                bail!("Proxmox: {what} — {ex} {log}");
            }
            if started.elapsed() > timeout { bail!("Proxmox: {what} — задача не завершилась за {} с", timeout.as_secs()); }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn vm_path(&self, id: &str) -> String { format!("/nodes/{}/qemu/{id}", self.node) }

    async fn guest_ip(&self, id: &str, timeout: Duration, log: &LogFn) -> Result<String> {
        let started = std::time::Instant::now();
        let mut said = false;
        loop {
            if let Ok(v) = self.get(&format!("{}/agent/network-get-interfaces", self.vm_path(id))).await {
                let ifs = v.get("result").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                for i in ifs {
                    if i["name"] == "lo" { continue; }
                    for a in i["ip-addresses"].as_array().cloned().unwrap_or_default() {
                        let ip = a["ip-address"].as_str().unwrap_or("");
                        if a["ip-address-type"] == "ipv4" && !ip.starts_with("127.") && !ip.starts_with("169.254.") { return Ok(ip.to_string()); }
                    }
                }
            }
            if !said { log("ожидание адреса от qemu-guest-agent…"); said = true; }
            if started.elapsed() > timeout { bail!("Proxmox: адрес ВМ не получен за {} с — в шаблоне нужен qemu-guest-agent (или задайте статический адрес)", timeout.as_secs()); }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Заменить мост в строке сетевого устройства Proxmox (`virtio=AA:..,bridge=vmbr0,firewall=1`).
pub fn with_bridge(net0: &str, bridge: &str) -> String {
    let mut parts: Vec<String> = net0.split(',').map(|s| s.to_string()).collect();
    match parts.iter().position(|p| p.starts_with("bridge=")) {
        Some(i) => parts[i] = format!("bridge={bridge}"),
        None => parts.push(format!("bridge={bridge}")),
    }
    parts.join(",")
}

#[async_trait]
impl Provider for Proxmox {
    fn title(&self) -> &'static str { "Proxmox VE" }

    async fn test(&self) -> Result<String> {
        let v = self.get("/version").await?;
        let tpl = p_u64(&self.p, "template", 0);
        if tpl == 0 { bail!("не задан VMID шаблона"); }
        let cfg = self.get(&format!("{}/config", self.vm_path(&tpl.to_string()))).await.map_err(|e| anyhow!("шаблон {tpl}: {e:#}"))?;
        let ci = cfg.as_object().map(|o| o.keys().any(|k| o[k].as_str().map(|s| s.contains("cloudinit")).unwrap_or(false))).unwrap_or(false);
        Ok(format!(
            "Proxmox VE {} · узел {} · шаблон {tpl} «{}»{}{}",
            v["version"].as_str().unwrap_or("?"),
            self.node,
            cfg["name"].as_str().unwrap_or(""),
            if cfg["template"] == 1 || cfg["template"] == "1" { "" } else { " (не помечен как шаблон)" },
            if ci { "" } else { " · ⚠ нет диска cloud-init" }
        ))
    }

    async fn create(&self, req: &VmRequest, log: &LogFn) -> Result<VmInfo> {
        let p = &req.p;
        let tpl = p_u64(p, "template", 0);
        if tpl == 0 { bail!("не задан VMID шаблона"); }
        let ip = static_ip(p)?;
        let newid = match p_u64(p, "vmid", 0) {
            0 => { let v = self.get("/cluster/nextid").await?; v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())).unwrap_or_default() }
            n => n.to_string(),
        };
        if newid.is_empty() { bail!("Proxmox не выдал VMID"); }
        let full = p_bool(p, "full_clone", true);
        let mut form = vec![("newid", newid.clone()), ("name", req.name.clone()), ("full", if full { "1".into() } else { "0".into() }), ("description", "HopeBackup: восстановление".to_string())];
        let storage = p_str(p, "storage");
        if full && !storage.is_empty() { form.push(("storage", storage)); }
        let pool = p_str(p, "pool");
        if !pool.is_empty() { form.push(("pool", pool)); }
        log(&format!("клонирование шаблона {tpl} → ВМ {newid}"));
        let upid = self.post(&format!("{}/clone", self.vm_path(&tpl.to_string())), form).await?;
        self.wait_task(&upid, "клонирование", Duration::from_secs(3600)).await?;
        // Дальше ВМ уже есть: при ошибке её удалит общий код (destroy_on_failure).
        let r: Result<VmInfo> = async {
            let cur = self.get(&format!("{}/config", self.vm_path(&newid))).await?;
            let mut cfg = vec![
                ("cores", p_u64(p, "cores", 2).to_string()),
                ("memory", p_u64(p, "memory_mb", 4096).to_string()),
                ("agent", "1".to_string()),
                ("ciuser", req.user.clone()),
                // Proxmox ждёт ключи в процентной кодировке (кодируется и «/», и пробел — %20).
                ("sshkeys", pct(&req.ssh_pubkey)),
                ("ipconfig0", match &ip {
                    None => "ip=dhcp".to_string(),
                    Some((a, m)) => { let gw = p_str(p, "gateway"); if gw.is_empty() { format!("ip={a}/{m}") } else { format!("ip={a}/{m},gw={gw}") } }
                }),
            ];
            let dns = p_str(p, "dns");
            if !dns.is_empty() { cfg.push(("nameserver", dns)); }
            let bridge = p_str(p, "bridge");
            if !bridge.is_empty() {
                let net0 = cur["net0"].as_str().ok_or_else(|| anyhow!("у шаблона нет сетевого устройства net0"))?;
                cfg.push(("net0", with_bridge(net0, &bridge)));
            }
            self.post(&format!("{}/config", self.vm_path(&newid)), cfg).await?;
            let disk_gb = p_u64(p, "disk_gb", 0);
            if disk_gb > 0 {
                let disk = { let d = p_str(p, "disk"); if d.is_empty() { "scsi0".to_string() } else { d } };
                let r = self.call(reqwest::Method::PUT, &format!("{}/resize", self.vm_path(&newid)), Some(vec![("disk".into(), disk.clone()), ("size".into(), format!("{disk_gb}G"))])).await;
                match r {
                    Ok(u) => self.wait_task(&u, "увеличение диска", Duration::from_secs(600)).await?,
                    Err(e) if format!("{e:#}").contains("shrinking") => log(&format!("диск {disk} шаблона больше {disk_gb} ГиБ — оставлен как есть")),
                    Err(e) => return Err(e),
                }
            }
            log(&format!("запуск ВМ {newid}"));
            let upid = self.post(&format!("{}/status/start", self.vm_path(&newid)), vec![]).await?;
            self.wait_task(&upid, "запуск", Duration::from_secs(300)).await?;
            let addr = match &ip { Some((a, _)) => a.clone(), None => self.guest_ip(&newid, Duration::from_secs(p_u64(p, "boot_timeout", 900)), log).await? };
            Ok(VmInfo { id: newid.clone(), ip: addr, details: json!({ "node": self.node, "vmid": newid, "template": tpl }) })
        }
        .await;
        match r {
            Ok(v) => Ok(v),
            Err(e) => {
                // ВМ склонирована, но не настроена или не запустилась.
                if p_bool(p, "destroy_on_failure", true) { let _ = self.destroy(&newid, log).await; } else { log(&format!("ВМ {newid} оставлена для разбора")); }
                Err(e)
            }
        }
    }

    async fn destroy(&self, id: &str, log: &LogFn) -> Result<()> {
        if let Ok(st) = self.get(&format!("{}/status/current", self.vm_path(id))).await {
            if st["status"] == "running" {
                let u = self.post(&format!("{}/status/stop", self.vm_path(id)), vec![]).await?;
                self.wait_task(&u, "остановка", Duration::from_secs(300)).await?;
            }
        } else {
            return Ok(());
        }
        let u = self.call(reqwest::Method::DELETE, &format!("{}?purge=1&destroy-unreferenced-disks=1", self.vm_path(id)), None).await?;
        self.wait_task(&u, "удаление", Duration::from_secs(600)).await?;
        log(&format!("ВМ {id} удалена"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bridge() {
        assert_eq!(super::with_bridge("virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr0,firewall=1", "vmbr2"), "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr2,firewall=1");
        assert_eq!(super::with_bridge("virtio=AA:BB:CC:DD:EE:FF", "vmbr2"), "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr2");
    }
}
