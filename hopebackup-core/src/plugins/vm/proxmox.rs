//! Proxmox VE: на узле (агент на нём или по SSH) — vzdump --stdout без сжатия (формат VMA для QEMU, tar
//! для LXC; режим snapshot — без остановки ВМ, с заморозкой ФС через гостевой агент), конфигурация ВМ.
//! ВМ на другом узле кластера копируется через SSH между узлами (доверие root внутри кластера PVE).
//! Восстановление: qmrestore из stdin (QEMU) или pct restore (LXC) на выбранный узел и хранилище.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{q, vm_stream, Driver, Host, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Proxmox { host: Host }

impl Proxmox {
    pub fn new(host: Host) -> Proxmox { Proxmox { host } }

    async fn local_node(&self, log: &LogFn) -> String { self.host.sh("hostname", log).await.unwrap_or_default().trim().to_string() }

    /// Команда на узле `node`: на этом узле — как есть, на другом — через ssh внутри кластера.
    fn on(&self, me: &str, node: &str, cmd: &str) -> String {
        if node.is_empty() || node == me { cmd.to_string() } else { format!("ssh -o BatchMode=yes root@{} {}", q(node), q(cmd)) }
    }
}

#[async_trait]
impl Driver for Proxmox {
    async fn list(&self, log: &LogFn) -> Result<Vec<VmRef>> {
        let o = self.host.sh("pvesh get /cluster/resources --type vm --output-format json", log).await.context("pvesh (это узел Proxmox VE?)")?;
        let v: Value = serde_json::from_str(&o)?;
        Ok(v.as_array().into_iter().flatten().filter(|r| r["template"].as_u64().unwrap_or(0) == 0).map(|r| VmRef {
            id: r["vmid"].to_string().trim_matches('"').to_string(), name: r["name"].as_str().unwrap_or("").to_string(), state: r["status"].as_str().unwrap_or("").to_string(),
            extra: json!({ "node": r["node"], "type": r["type"] }),
        }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, _cfg: &Value, log: &LogFn) -> Result<Value> {
        let me = self.local_node(log).await;
        let node = vm.extra["node"].as_str().unwrap_or("").to_string();
        let typ = vm.extra["type"].as_str().unwrap_or("qemu").to_string();
        let tool = if typ == "lxc" { "pct" } else { "qm" };
        let conf = self.host.sh(&self.on(&me, &node, &format!("{tool} config {}", vm.id)), log).await.unwrap_or_default();
        super::super::appkit::add_bytes(w, &format!("{prefix}config.txt"), conf.into_bytes()).await?;
        if node != me && !node.is_empty() { log(&format!("  ВМ на узле {node} — vzdump через SSH кластера")); }
        // Без сжатия: копия дедуплицируется, vzdump меньше грузит процессор узла.
        let dump = self.on(&me, &node, &format!("vzdump {} --mode snapshot --stdout --compress 0 --quiet 1", vm.id));
        let name = if typ == "lxc" { "dump.tar" } else { "dump.vma" };
        self.host.stream_out(w, &format!("{prefix}{name}"), &dump, log).await.context("vzdump")?;
        Ok(json!({ "kind": "proxmox", "vmid": vm.id, "type": typ, "node": node, "dump": name }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let me = self.local_node(log).await;
        let node = cfg_str(p, "node").unwrap_or_else(|| me.clone());
        let typ = vm["type"].as_str().unwrap_or("qemu");
        let replace = super::super::appkit::bool_param(p, "replace", false);
        let orig = vm["vmid"].as_str().unwrap_or("").to_string();
        // Тот же vmid — только с заменой; иначе — следующий свободный.
        let used = self.host.sh(&format!("pvesh get /cluster/resources --type vm --output-format json"), log).await.ok().and_then(|o| serde_json::from_str::<Value>(&o).ok())
            .map(|v| v.as_array().into_iter().flatten().any(|r| r["vmid"].to_string().trim_matches('"') == orig)).unwrap_or(false);
        let vmid = if cfg_str(p, "new_name").is_none() && (!used || replace) { orig.clone() } else { self.host.sh("pvesh get /cluster/nextid", log).await?.trim().trim_matches('"').to_string() };
        if used && vmid == orig && replace {
            let tool = if typ == "lxc" { "pct" } else { "qm" };
            let _ = self.host.sh(&self.on(&me, &node, &format!("{tool} stop {vmid}")), log).await;
            log(&format!("  ВМ {vmid} будет перезаписана (qmrestore --force)"));
        }
        let mut opts = String::new();
        if let Some(s) = cfg_str(p, "storage") { opts.push_str(&format!(" --storage {}", q(&s))); }
        if vmid != orig { opts.push_str(" --unique 1"); }
        if used && vmid == orig { opts.push_str(" --force 1"); }
        let s = vm_stream(m, vm, vm["dump"].as_str().unwrap_or("dump.vma"))?;
        if typ == "lxc" {
            let tmp = format!("/var/tmp/hopebackup-{}.tar", crate::util::short_id());
            self.host.stream_in(repo, s, &self.on(&me, &node, &format!("cat > {tmp}")), log).await?;
            let r = self.host.sh(&self.on(&me, &node, &format!("pct restore {vmid} {tmp}{opts}")), log).await;
            let _ = self.host.sh(&self.on(&me, &node, &format!("rm -f {tmp}")), log).await;
            r.context("pct restore")?;
        } else {
            self.host.stream_in(repo, s, &self.on(&me, &node, &format!("qmrestore - {vmid}{opts}")), log).await.context("qmrestore")?;
        }
        let tool = if typ == "lxc" { "pct" } else { "qm" };
        if let Some(n) = cfg_str(p, "new_name") { let k = if typ == "lxc" { "--hostname" } else { "--name" }; self.host.sh(&self.on(&me, &node, &format!("{tool} set {vmid} {k} {}", q(&n))), log).await?; }
        if super::super::appkit::bool_param(p, "start", false) { self.host.sh(&self.on(&me, &node, &format!("{tool} start {vmid}")), log).await?; }
        log(&format!("  восстановлена как {vmid} на узле {node}"));
        if vmid.is_empty() { return Err(anyhow!("не удалось получить vmid")); }
        if typ != "lxc" && typ != "qemu" { bail!("неизвестный тип {typ}"); }
        Ok(json!({ "vmid": vmid, "node": node }))
    }
}
