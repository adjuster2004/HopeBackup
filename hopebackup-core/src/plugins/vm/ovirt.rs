//! oVirt Engine API v4 — zVirt (Орион софт), РОСА Виртуализация, HOSTVM, RHV/OLVM.
//!
//! Копия: API резервного копирования ВМ (POST /vms/{id}/backups — без остановки, Engine 4.4+), каждый диск
//! — передачей образа (imagetransfers, direction=download, format=raw) потоком из imageio в репозиторий;
//! конфигурация ВМ, диски и сетевые интерфейсы — JSON. Восстановление: новые диски (imagetransfers upload),
//! новая ВМ с той же конфигурацией в выбранном кластере, подключение дисков и интерфейсов.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{poll, repo_body, vm_stream, Driver, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Ovirt { c: reqwest::Client, base: String, user: String, pass: String }

impl Ovirt {
    pub fn new(cfg: &Value) -> Result<Ovirt> {
        let base = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес API oVirt (https://engine/ovirt-engine/api)"))?.trim_end_matches('/').to_string();
        let base = if base.ends_with("/api") { base } else { format!("{base}/ovirt-engine/api") };
        Ok(Ovirt { c: super::http(cfg)?, base, user: cfg_str(cfg, "user").unwrap_or_else(|| "admin@internal".into()), pass: cfg_str(cfg, "password").unwrap_or_default() })
    }
    pub(crate) fn rb(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.base) };
        self.c.request(m, url).basic_auth(&self.user, Some(&self.pass)).header("Accept", "application/json").header("Version", "4")
    }
    pub(crate) async fn call(&self, m: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut r = self.rb(m.clone(), path);
        if let Some(b) = body { r = r.json(&b); }
        let resp = r.send().await.with_context(|| format!("oVirt {path}"))?;
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        if !st.is_success() { bail!("oVirt {m} {path}: {} {}", st.as_u16(), v["fault"]["detail"].as_str().or(v["detail"].as_str()).unwrap_or(&t.chars().take(200).collect::<String>())); }
        Ok(v)
    }
    pub(crate) async fn get(&self, p: &str) -> Result<Value> { self.call(reqwest::Method::GET, p, None).await }
    pub(crate) async fn post(&self, p: &str, b: Value) -> Result<Value> { self.call(reqwest::Method::POST, p, Some(b)).await }

    pub(crate) async fn transfer(&self, disk: &str, backup: Option<&str>, dir: &str, log: &LogFn) -> Result<(String, String)> {
        let mut body = json!({ "disk": { "id": disk }, "direction": dir, "format": "raw", "inactivity_timeout": 3600 });
        if let Some(b) = backup { body["backup"] = json!({ "id": b }); }
        let t = self.post("/imagetransfers", body).await.context("передача образа")?;
        let id = t["id"].as_str().ok_or_else(|| anyhow!("oVirt: нет id передачи"))?.to_string();
        let v = poll(600, 2, "передача образа", || async {
            let x = self.get(&format!("/imagetransfers/{id}")).await?;
            match x["phase"].as_str().unwrap_or("") { "transferring" => Ok(Some(x)), "failed" | "finished_failure" | "cancelled_system" | "cancelled_user" => bail!("передача образа: {}", x["phase"]), _ => Ok(None) }
        }).await?;
        let url = v["transfer_url"].as_str().filter(|u| !u.is_empty()).or(v["proxy_url"].as_str()).ok_or_else(|| anyhow!("oVirt: нет адреса imageio"))?.to_string();
        let _ = log;
        Ok((id, url))
    }

    pub(crate) async fn finalize(&self, id: &str) -> Result<()> {
        self.post(&format!("/imagetransfers/{id}/finalize"), json!({})).await?;
        poll(600, 2, "завершение передачи", || async {
            match self.get(&format!("/imagetransfers/{id}")).await {
                Ok(x) => match x["phase"].as_str().unwrap_or("") { "finished_success" => Ok(Some(x)), "finished_failure" | "failed" => bail!("передача образа завершилась ошибкой"), _ => Ok(None) },
                Err(_) => Ok(Some(Value::Null)), // передача удалена — завершена
            }
        }).await?;
        Ok(())
    }
}

#[async_trait]
impl Driver for Ovirt {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let v = self.get("/vms").await?;
        Ok(v["vm"].as_array().into_iter().flatten().map(|x| VmRef { id: x["id"].as_str().unwrap_or("").into(), name: x["name"].as_str().unwrap_or("").into(), state: x["status"].as_str().unwrap_or("").into(), extra: Value::Null }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, _cfg: &Value, log: &LogFn) -> Result<Value> {
        let conf = self.get(&format!("/vms/{}?all_content=true", vm.id)).await?;
        let atts = self.get(&format!("/vms/{}/diskattachments?follow=disk", vm.id)).await?;
        let nics = self.get(&format!("/vms/{}/nics", vm.id)).await.unwrap_or_default();
        super::super::appkit::add_bytes(w, &format!("{prefix}vm.json"), serde_json::to_vec_pretty(&json!({ "vm": conf, "diskattachments": atts, "nics": nics }))?).await?;
        let disks: Vec<Value> = atts["disk_attachment"].as_array().cloned().unwrap_or_default();
        if disks.is_empty() { return Ok(json!({ "kind": "ovirt", "disks": [] })); }
        let ids: Vec<Value> = disks.iter().map(|a| json!({ "id": a["disk"]["id"] })).collect();
        let b = self.post(&format!("/vms/{}/backups", vm.id), json!({ "disks": { "disk": ids } })).await.context("начало резервного копирования (Engine 4.4+)")?;
        let bid = b["id"].as_str().unwrap_or("").to_string();
        poll(900, 3, "подготовка резервного копирования", || async {
            let x = self.get(&format!("/vms/{}/backups/{bid}", vm.id)).await?;
            match x["phase"].as_str().unwrap_or("") { "ready" => Ok(Some(x)), "failed" => bail!("резервное копирование не началось"), _ => Ok(None) }
        }).await?;
        let res = async {
            let mut out = Vec::new();
            for a in &disks {
                let d = &a["disk"];
                let did = d["id"].as_str().unwrap_or("");
                let (tid, url) = self.transfer(did, Some(&bid), "download", log).await?;
                let sname = format!("disk-{did}.raw");
                let r = super::http_to_stream(w, &format!("{prefix}{sname}"), self.c.get(&url), log).await;
                let f = self.finalize(&tid).await;
                r?; f?;
                out.push(json!({ "id": did, "name": d["name"], "stream": sname, "provisioned_size": d["provisioned_size"], "format": d["format"], "sparse": d["sparse"],
                    "storage_domain": d["storage_domains"]["storage_domain"][0]["id"], "interface": a["interface"], "bootable": a["bootable"] }));
            }
            Ok::<_, anyhow::Error>(out)
        }.await;
        let _ = self.post(&format!("/vms/{}/backups/{bid}/finalize", vm.id), json!({})).await;
        Ok(json!({ "kind": "ovirt", "disks": res? }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let cfgj: Value = serde_json::from_slice(&super::super::appkit::read_small(repo, vm_stream(m, vm, "vm.json")?).await?)?;
        let c = &cfgj["vm"];
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("").to_string());
        let exists = self.get(&format!("/vms?search=name%3D{}", urlenc(&name))).await?["vm"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
        if exists {
            if !super::super::appkit::bool_param(p, "replace", false) { bail!("ВМ «{name}» уже есть: задайте новое имя или включите замену"); }
            let old = self.get(&format!("/vms?search=name%3D{}", urlenc(&name))).await?["vm"][0]["id"].as_str().unwrap_or("").to_string();
            let aside = format!("{name}-before-restore-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
            let _ = self.post(&format!("/vms/{old}/stop"), json!({})).await;
            self.call(reqwest::Method::PUT, &format!("/vms/{old}"), Some(json!({ "name": aside }))).await?;
            log(&format!("  прежняя ВМ переименована в {aside}"));
        }
        let sd = cfg_str(p, "storage");
        let mut new_disks = Vec::new();
        for d in vm["disks"].as_array().cloned().unwrap_or_default() {
            let sdom = match &sd { Some(n) => json!({ "name": n }), None => json!({ "id": d["storage_domain"] }) };
            let nd = self.post("/disks", json!({ "name": format!("{name}_{}", d["name"].as_str().unwrap_or("disk")), "provisioned_size": d["provisioned_size"], "format": d["format"].as_str().unwrap_or("cow"),
                "sparse": d["sparse"].as_bool().unwrap_or(true), "storage_domains": { "storage_domain": [sdom] } })).await.context("создание диска")?;
            let did = nd["id"].as_str().unwrap_or("").to_string();
            poll(900, 2, "создание диска", || async { let x = self.get(&format!("/disks/{did}")).await?; Ok((x["status"] == "ok").then_some(x)) }).await?;
            let (tid, url) = self.transfer(&did, None, "upload", log).await?;
            let s = vm_stream(m, vm, d["stream"].as_str().unwrap_or(""))?;
            let (body, fetch) = repo_body(repo, s)?;
            log(&format!("  загрузка диска {} ({})", d["name"].as_str().unwrap_or(""), crate::util::human_bytes(s.size)));
            let r = self.c.put(&url).header("Content-Length", s.size.to_string()).body(body).send().await;
            fetch.await??;
            let r = r?;
            if !r.status().is_success() { bail!("imageio: {} {}", r.status(), r.text().await.unwrap_or_default()); }
            self.finalize(&tid).await?;
            poll(900, 2, "диск после загрузки", || async { let x = self.get(&format!("/disks/{did}")).await?; Ok((x["status"] == "ok").then_some(x)) }).await?;
            new_disks.push((did, d.clone()));
        }
        let cluster = match cfg_str(p, "node") { Some(n) => json!({ "name": n }), None => json!({ "id": c["cluster"]["id"] }) };
        let mut body = json!({ "name": name, "cluster": cluster, "template": { "name": "Blank" } });
        for k in ["memory", "cpu", "os", "bios", "type", "description", "memory_policy", "high_availability"] { if !c[k].is_null() { body[k] = c[k].clone(); } }
        let nv = self.post("/vms", body).await.context("создание ВМ")?;
        let vid = nv["id"].as_str().unwrap_or("").to_string();
        poll(600, 2, "создание ВМ", || async { let x = self.get(&format!("/vms/{vid}")).await?; Ok((x["status"] == "down").then_some(x)) }).await?;
        for (did, d) in &new_disks {
            self.post(&format!("/vms/{vid}/diskattachments"), json!({ "disk": { "id": did }, "interface": d["interface"].as_str().unwrap_or("virtio_scsi"), "bootable": d["bootable"].as_bool().unwrap_or(false), "active": true })).await.context("подключение диска")?;
        }
        let profile = match cfg_str(p, "network") {
            Some(n) => self.get("/vnicprofiles").await?["vnic_profile"].as_array().into_iter().flatten().find(|x| x["name"] == n.as_str()).map(|x| x["id"].clone()),
            None => None,
        };
        for n in cfgj["nics"]["nic"].as_array().cloned().unwrap_or_default() {
            let prof = profile.clone().or_else(|| n["vnic_profile"]["id"].as_str().map(|s| json!(s)));
            let mut b = json!({ "name": n["name"], "interface": n["interface"].as_str().unwrap_or("virtio") });
            if let Some(pr) = prof { b["vnic_profile"] = json!({ "id": pr }); }
            if let Err(e) = self.post(&format!("/vms/{vid}/nics"), b).await { log(&format!("  ! интерфейс {}: {e:#} — добавьте вручную", n["name"])); }
        }
        if super::super::appkit::bool_param(p, "start", false) { self.post(&format!("/vms/{vid}/start"), json!({})).await?; }
        log(&format!("  ВМ {name} создана ({vid}), дисков {}", new_disks.len()));
        Ok(json!({ "vm": name, "id": vid, "disks": new_disks.iter().map(|x| x.0.clone()).collect::<Vec<_>>() }))
    }
}

pub fn urlenc(s: &str) -> String { s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect() }
