//! XCP-ng, Citrix Hypervisor / XenServer (XAPI на мастере пула).
//!
//! Копия: снимок ВМ (VM.snapshot; с заморозкой — VM.snapshot_with_quiesce, если включено и поддерживается)
//! → экспорт снимка в XVA потоком (HTTP GET /export, без сжатия — для дедупликации) → удаление снимка и
//! его дисков. Восстановление: HTTP PUT /import XVA в выбранный SR (новая ВМ), переименование, запуск.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::xml;
use super::{repo_body, vm_stream, Driver, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Xapi { c: reqwest::Client, base: String, session: String }

impl Xapi {
    pub async fn login(cfg: &Value) -> Result<Xapi> {
        let base = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес мастера пула"))?.trim_end_matches('/').to_string();
        let mut x = Xapi { c: super::http(cfg)?, base, session: String::new() };
        let r = x.call("session.login_with_password", &[json!(cfg_str(cfg, "user").unwrap_or_else(|| "root".into())), json!(cfg_str(cfg, "password").unwrap_or_default()), json!("2.0"), json!("HopeBackup")]).await;
        match r {
            Ok(s) => x.session = s.as_str().unwrap_or("").to_string(),
            Err(e) if e.to_string().contains("HOST_IS_SLAVE") => bail!("это не мастер пула: {e:#} — укажите адрес мастера"),
            Err(e) => return Err(e.context("вход в XAPI")),
        }
        Ok(x)
    }
    /// Вызов XAPI: {Status, Value | ErrorDescription}.
    async fn call(&self, method: &str, args: &[Value]) -> Result<Value> {
        let mut params: Vec<Value> = Vec::new();
        if !self.session.is_empty() { params.push(json!(self.session)); }
        params.extend(args.iter().cloned());
        let body = xml::rpc_request(method, &params);
        let r = self.c.post(&self.base).header("Content-Type", "text/xml").body(body).send().await.with_context(|| format!("XAPI {}", self.base))?;
        let t = r.text().await?;
        let v = xml::rpc_response(&t)?;
        if v["Status"] != "Success" { bail!("{method}: {}", v["ErrorDescription"].as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" ")).unwrap_or_default()); }
        Ok(v["Value"].clone())
    }
    async fn s(&self, method: &str, args: &[Value]) -> Result<String> { Ok(self.call(method, args).await?.as_str().unwrap_or("").to_string()) }

    /// Удалить снимок вместе с его дисками.
    async fn destroy_snapshot(&self, snap: &str) -> Result<()> {
        let vbds = self.call("VM.get_VBDs", &[json!(snap)]).await.unwrap_or_default();
        let mut vdis = Vec::new();
        for b in vbds.as_array().into_iter().flatten() {
            if self.s("VBD.get_type", &[b.clone()]).await.unwrap_or_default() == "Disk" { if let Ok(v) = self.s("VBD.get_VDI", &[b.clone()]).await { vdis.push(v); } }
        }
        self.call("VM.destroy", &[json!(snap)]).await?;
        for v in vdis { let _ = self.call("VDI.destroy", &[json!(v)]).await; }
        Ok(())
    }
}

#[async_trait]
impl Driver for Xapi {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let all = self.call("VM.get_all_records", &[]).await?;
        Ok(all.as_object().into_iter().flatten().filter(|(_, v)| v["is_a_template"] != true && v["is_control_domain"] != true && v["is_a_snapshot"] != true)
            .map(|(r, v)| VmRef { id: v["uuid"].as_str().unwrap_or("").into(), name: v["name_label"].as_str().unwrap_or("").into(), state: v["power_state"].as_str().unwrap_or("").into(), extra: json!({ "ref": r }) }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> {
        let r = vm.extra["ref"].as_str().unwrap_or("").to_string();
        let rec = self.call("VM.get_record", &[json!(r)]).await?;
        super::super::appkit::add_bytes(w, &format!("{prefix}vm.json"), serde_json::to_vec_pretty(&rec)?).await?;
        let name = format!("hopebackup-{}", &crate::util::short_id()[..8]);
        let quiesce = super::super::appkit::bool_param(cfg, "quiesce", true) && vm.state == "Running";
        let snap = if quiesce {
            match self.s("VM.snapshot_with_quiesce", &[json!(r), json!(name)]).await { Ok(s) => s, Err(e) => { log(&format!("  снимок с заморозкой недоступен ({e:#}) — без неё")); self.s("VM.snapshot", &[json!(r), json!(name)]).await? } }
        } else { self.s("VM.snapshot", &[json!(r), json!(name)]).await? };
        let res = async {
            let uuid = self.s("VM.get_uuid", &[json!(snap)]).await?;
            let url = format!("{}/export?session_id={}&uuid={uuid}&use_compression=false", self.base, self.session);
            let e = super::http_to_stream(w, &format!("{prefix}vm.xva"), self.c.get(&url), log).await?;
            Ok::<_, anyhow::Error>(e.size)
        }.await;
        if let Err(e) = self.destroy_snapshot(&snap).await { log(&format!("! снимок {name} не удалён: {e:#}")); }
        let size = res?;
        Ok(json!({ "kind": "xen", "xva": "vm.xva", "size": size, "uuid": vm.id }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let sr = match cfg_str(p, "storage") {
            Some(n) => { let srs = self.call("SR.get_by_name_label", &[json!(n)]).await?; srs.as_array().and_then(|a| a.first()).and_then(|x| x.as_str()).map(|s| s.to_string()).ok_or_else(|| anyhow!("нет SR «{n}»"))? }
            None => { let pools = self.call("pool.get_all", &[]).await?; let pool = pools[0].as_str().unwrap_or("").to_string(); self.s("pool.get_default_SR", &[json!(pool)]).await? }
        };
        let task = self.s("task.create", &[json!("hopebackup-import"), json!("HopeBackup")]).await?;
        let s = vm_stream(m, vm, vm["xva"].as_str().unwrap_or("vm.xva"))?;
        let (body, fetch) = repo_body(repo, s)?;
        log(&format!("  импорт XVA ({})", crate::util::human_bytes(s.size)));
        let url = format!("{}/import?session_id={}&sr_id={sr}&task_id={task}", self.base, self.session);
        let r = self.c.put(&url).header("Content-Length", s.size.to_string()).body(body).send().await;
        fetch.await??;
        let r = r?;
        if !r.status().is_success() { bail!("XAPI /import: {}", r.status()); }
        let res = super::poll(3600, 2, "импорт", || async {
            match self.s("task.get_status", &[json!(task)]).await?.as_str() { "success" => Ok(Some(json!(self.s("task.get_result", &[json!(task)]).await?))), "failure" => bail!("импорт: {}", self.call("task.get_error_info", &[json!(task)]).await.unwrap_or_default()), _ => Ok(None) }
        }).await?;
        let _ = self.call("task.destroy", &[json!(task)]).await;
        let parsed = xml::parse(res.as_str().unwrap_or("")).ok();
        // Результат задачи импорта: <value><array><data><value>OpaqueRef:…</value></data></array></value>.
        let new_vm = parsed.as_ref().and_then(|d| { let mut vs = Vec::new(); d.find_all("value", &mut vs); vs.into_iter().map(|v| v.text.trim().to_string()).find(|t| t.starts_with("OpaqueRef:")) })
            .or_else(|| res.as_str().map(|s| s.trim().to_string()).filter(|s| s.starts_with("OpaqueRef:"))).ok_or_else(|| anyhow!("XAPI не вернул ссылку на импортированную ВМ: {res}"))?;
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("").to_string());
        self.call("VM.set_name_label", &[json!(new_vm), json!(name)]).await?;
        if super::super::appkit::bool_param(p, "start", false) { self.call("VM.start", &[json!(new_vm), json!(false), json!(false)]).await?; }
        log(&format!("  ВМ {name} импортирована в SR"));
        Ok(json!({ "vm": name, "ref": new_vm }))
    }
}
