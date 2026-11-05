//! Nutanix AHV через Prism Central (API v3).
//!
//! Копия: из каждого диска ВМ создаётся образ (images с источником vm_disk — клон диска на момент
//! создания, без остановки ВМ), образ загружается потоком (GET /images/{uuid}/file) и удаляется; описание
//! ВМ (spec) — JSON. Восстановление: образы загружаются обратно (PUT /images/{uuid}/file), ВМ создаётся с
//! дисками-клонами этих образов в выбранном кластере и подсети.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{poll, repo_body, vm_stream, Driver, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct Nutanix { c: reqwest::Client, base: String, user: String, pass: String }

impl Nutanix {
    pub fn new(cfg: &Value) -> Result<Nutanix> {
        let base = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес Prism Central"))?.trim_end_matches('/').trim_end_matches("/api/nutanix/v3").to_string();
        Ok(Nutanix { c: super::http(cfg)?, base: format!("{base}/api/nutanix/v3"), user: cfg_str(cfg, "user").unwrap_or_default(), pass: cfg_str(cfg, "password").unwrap_or_default() })
    }
    fn rb(&self, m: reqwest::Method, p: &str) -> reqwest::RequestBuilder { self.c.request(m, format!("{}{p}", self.base)).basic_auth(&self.user, Some(&self.pass)) }
    async fn call(&self, m: reqwest::Method, p: &str, b: Option<Value>) -> Result<Value> {
        let mut r = self.rb(m.clone(), p);
        if let Some(b) = b { r = r.json(&b); }
        let resp = r.send().await.with_context(|| format!("Prism {p}"))?;
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        if !st.is_success() { bail!("Prism {m} {p}: {} {}", st.as_u16(), t.chars().take(300).collect::<String>()); }
        Ok(serde_json::from_str(&t).unwrap_or(Value::Null))
    }
    async fn task(&self, v: &Value, what: &str) -> Result<Value> {
        let Some(t) = v["status"]["execution_context"]["task_uuid"].as_str().map(|s| s.to_string()) else { return Ok(v.clone()) };
        poll(7200, 3, what, || async { let x = self.call(reqwest::Method::GET, &format!("/tasks/{t}"), None).await?; match x["status"].as_str().unwrap_or("") { "SUCCEEDED" => Ok(Some(x)), "FAILED" | "ABORTED" => bail!("{what}: {}", x["error_detail"]), _ => Ok(None) } }).await
    }
}

#[async_trait]
impl Driver for Nutanix {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let v = self.call(reqwest::Method::POST, "/vms/list", Some(json!({ "kind": "vm", "length": 500 }))).await?;
        Ok(v["entities"].as_array().into_iter().flatten().map(|e| VmRef { id: e["metadata"]["uuid"].as_str().unwrap_or("").into(), name: e["status"]["name"].as_str().unwrap_or("").into(), state: e["status"]["resources"]["power_state"].as_str().unwrap_or("").into(), extra: Value::Null }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, _cfg: &Value, log: &LogFn) -> Result<Value> {
        let v = self.call(reqwest::Method::GET, &format!("/vms/{}", vm.id), None).await?;
        super::super::appkit::add_bytes(w, &format!("{prefix}vm.json"), serde_json::to_vec_pretty(&v)?).await?;
        let mut disks = Vec::new();
        for (i, d) in v["spec"]["resources"]["disk_list"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            if d["device_properties"]["device_type"] == "CDROM" { continue; }
            let du = d["uuid"].as_str().unwrap_or("").to_string();
            let img = self.call(reqwest::Method::POST, "/images", Some(json!({ "spec": { "name": format!("hopebackup-{}-{i}", &crate::util::short_id()[..6]), "resources": { "image_type": "DISK_IMAGE", "data_source_reference": { "kind": "vm_disk", "uuid": du } } }, "metadata": { "kind": "image" } }))).await.context("образ из диска ВМ")?;
            let iu = img["metadata"]["uuid"].as_str().unwrap_or("").to_string();
            let r = async {
                self.task(&img, "образ из диска").await?;
                let sname = format!("disk-{i}.img");
                let e = super::http_to_stream(w, &format!("{prefix}{sname}"), self.rb(reqwest::Method::GET, &format!("/images/{iu}/file")), log).await?;
                Ok::<_, anyhow::Error>(json!({ "index": i, "stream": sname, "size": e.size, "disk_size_mib": d["disk_size_mib"], "adapter": d["device_properties"]["disk_address"]["adapter_type"], "device_index": d["device_properties"]["disk_address"]["device_index"] }))
            }.await;
            let _ = self.call(reqwest::Method::DELETE, &format!("/images/{iu}"), None).await;
            disks.push(r?);
        }
        Ok(json!({ "kind": "nutanix", "disks": disks, "cluster": v["spec"]["cluster_reference"] }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let orig: Value = serde_json::from_slice(&super::super::appkit::read_small(repo, vm_stream(m, vm, "vm.json")?).await?)?;
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("").to_string());
        let mut disk_list = Vec::new();
        for d in vm["disks"].as_array().cloned().unwrap_or_default() {
            let s = vm_stream(m, vm, d["stream"].as_str().unwrap_or(""))?;
            let img = self.call(reqwest::Method::POST, "/images", Some(json!({ "spec": { "name": format!("{name}-disk{}", d["index"]), "resources": { "image_type": "DISK_IMAGE" } }, "metadata": { "kind": "image" } }))).await?;
            self.task(&img, "создание образа").await?;
            let iu = img["metadata"]["uuid"].as_str().unwrap_or("").to_string();
            let (body, fetch) = repo_body(repo, s)?;
            log(&format!("  загрузка диска {} ({})", d["index"], crate::util::human_bytes(s.size)));
            let r = self.rb(reqwest::Method::PUT, &format!("/images/{iu}/file")).header("Content-Type", "application/octet-stream").header("Content-Length", s.size.to_string()).body(body).send().await;
            fetch.await??;
            let r = r?;
            if !r.status().is_success() { bail!("Prism: загрузка образа {}", r.status()); }
            disk_list.push(json!({ "data_source_reference": { "kind": "image", "uuid": iu }, "device_properties": { "device_type": "DISK", "disk_address": { "adapter_type": d["adapter"].as_str().unwrap_or("SCSI"), "device_index": d["device_index"] } } }));
        }
        let r0 = &orig["spec"]["resources"];
        let cluster = match cfg_str(p, "storage").or_else(|| cfg_str(p, "node")) { Some(c) => json!({ "kind": "cluster", "uuid": c }), None => orig["spec"]["cluster_reference"].clone() };
        let nics: Vec<Value> = match cfg_str(p, "network") {
            Some(sn) => vec![json!({ "subnet_reference": { "kind": "subnet", "uuid": sn } })],
            None => r0["nic_list"].as_array().cloned().unwrap_or_default().into_iter().map(|n| json!({ "subnet_reference": n["subnet_reference"] })).collect(),
        };
        let spec = json!({ "spec": { "name": name, "cluster_reference": cluster, "resources": { "num_sockets": r0["num_sockets"], "num_vcpus_per_socket": r0["num_vcpus_per_socket"], "memory_size_mib": r0["memory_size_mib"],
            "boot_config": r0["boot_config"], "disk_list": disk_list, "nic_list": nics, "power_state": if super::super::appkit::bool_param(p, "start", false) { "ON" } else { "OFF" } } }, "metadata": { "kind": "vm" } });
        let v = self.call(reqwest::Method::POST, "/vms", Some(spec)).await.context("создание ВМ")?;
        self.task(&v, "создание ВМ").await?;
        let id = v["metadata"]["uuid"].as_str().unwrap_or("").to_string();
        log(&format!("  ВМ {name} создана ({id})"));
        Ok(json!({ "vm": name, "id": id }))
    }
}
