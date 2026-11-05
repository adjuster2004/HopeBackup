//! OpenStack: Keystone v3 (пароль или application credential «appcred:<id>»), Nova, Glance, Cinder.
//!
//! Копия: ВМ с диском из образа — снимок Nova (createImage) и его загрузка из Glance; ВМ с дисками-томами —
//! каждый том выгружается в образ Cinder (os-volume_upload_image, force — без остановки) и загружается из
//! Glance. Временные образы удаляются. Описание ВМ (flavor, сети, группы безопасности) — JSON.
//! Восстановление: образы загружаются в Glance, ВМ создаётся с тем же flavor (или заданным) и сетью;
//! тома — новые тома Cinder из образов, загрузка с тома.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{poll, repo_body, vm_stream, Driver, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct OpenStack { c: reqwest::Client, token: String, nova: String, glance: String, cinder: Option<String> }

fn endpoint(cat: &Value, kind: &[&str], region: &str) -> Option<String> {
    cat.as_array()?.iter().filter(|s| kind.contains(&s["type"].as_str().unwrap_or(""))).flat_map(|s| s["endpoints"].as_array().cloned().unwrap_or_default())
        .find(|e| e["interface"] == "public" && (region.is_empty() || e["region"] == region || e["region_id"] == region))
        .and_then(|e| e["url"].as_str().map(|u| u.trim_end_matches('/').to_string()))
}

impl OpenStack {
    pub async fn login(cfg: &Value, log: &LogFn) -> Result<OpenStack> {
        let c = super::http(cfg)?;
        let url = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес Keystone"))?.trim_end_matches('/').to_string();
        let ks = if url.ends_with("/v3") { url } else { format!("{url}/v3") };
        let user = cfg_str(cfg, "user").unwrap_or_default();
        let pass = cfg_str(cfg, "password").unwrap_or_default();
        let domain = cfg_str(cfg, "domain").unwrap_or_else(|| "Default".into());
        let body = if let Some(id) = user.strip_prefix("appcred:") {
            json!({ "auth": { "identity": { "methods": ["application_credential"], "application_credential": { "id": id, "secret": pass } } } })
        } else {
            json!({ "auth": { "identity": { "methods": ["password"], "password": { "user": { "name": user, "domain": { "name": domain }, "password": pass } } },
                "scope": { "project": { "name": cfg_str(cfg, "project").unwrap_or_else(|| "admin".into()), "domain": { "name": domain } } } } })
        };
        let r = c.post(format!("{ks}/auth/tokens")).json(&body).send().await.context("Keystone недоступен")?;
        if !r.status().is_success() { bail!("Keystone не принял учётку: {}", r.status()); }
        let token = r.headers().get("x-subject-token").and_then(|v| v.to_str().ok()).ok_or_else(|| anyhow!("Keystone не вернул токен"))?.to_string();
        let v: Value = r.json().await?;
        let region = cfg_str(cfg, "region").unwrap_or_default();
        let cat = &v["token"]["catalog"];
        let nova = endpoint(cat, &["compute"], &region).ok_or_else(|| anyhow!("в каталоге нет Nova"))?;
        let glance = endpoint(cat, &["image"], &region).ok_or_else(|| anyhow!("в каталоге нет Glance"))?;
        let cinder = endpoint(cat, &["volumev3", "block-storage"], &region);
        let _ = log;
        Ok(OpenStack { c, token, nova, glance, cinder })
    }
    async fn call(&self, m: reqwest::Method, url: &str, body: Option<Value>) -> Result<(Value, reqwest::header::HeaderMap)> {
        let mut r = self.c.request(m.clone(), url).header("X-Auth-Token", &self.token).header("OpenStack-API-Version", "compute 2.47").header("X-OpenStack-Nova-API-Version", "2.47");
        if let Some(b) = body { r = r.json(&b); }
        let resp = r.send().await.with_context(|| format!("{url}"))?;
        let st = resp.status();
        let h = resp.headers().clone();
        let t = resp.text().await.unwrap_or_default();
        if !st.is_success() { bail!("OpenStack {m} {url}: {} {}", st.as_u16(), t.chars().take(300).collect::<String>()); }
        Ok((serde_json::from_str(&t).unwrap_or(Value::Null), h))
    }
    async fn get(&self, url: &str) -> Result<Value> { Ok(self.call(reqwest::Method::GET, url, None).await?.0) }
    async fn post(&self, url: &str, b: Value) -> Result<Value> { Ok(self.call(reqwest::Method::POST, url, Some(b)).await?.0) }
    async fn wait_image(&self, id: &str) -> Result<Value> {
        poll(7200, 5, "образ Glance", || async { let x = self.get(&format!("{}/v2/images/{id}", self.glance)).await?; match x["status"].as_str().unwrap_or("") { "active" => Ok(Some(x)), "killed" | "deleted" => bail!("образ {id}: {}", x["status"]), _ => Ok(None) } }).await
    }
    async fn download(&self, w: &mut SnapshotWriter, name: &str, image: &str, log: &LogFn) -> Result<u64> {
        let e = super::http_to_stream(w, name, self.c.get(format!("{}/v2/images/{image}/file", self.glance)).header("X-Auth-Token", &self.token), log).await?;
        Ok(e.size)
    }
    async fn upload(&self, repo: &Arc<Repo>, s: &crate::store::types::StreamEntry, name: &str, fmt: &str, log: &LogFn) -> Result<String> {
        let img = self.post(&format!("{}/v2/images", self.glance), json!({ "name": name, "disk_format": fmt, "container_format": "bare", "visibility": "private" })).await?;
        let id = img["id"].as_str().unwrap_or("").to_string();
        let (body, fetch) = repo_body(repo, s)?;
        log(&format!("  загрузка образа {name} ({})", crate::util::human_bytes(s.size)));
        let r = self.c.put(format!("{}/v2/images/{id}/file", self.glance)).header("X-Auth-Token", &self.token).header("Content-Type", "application/octet-stream").header("Content-Length", s.size.to_string()).body(body).send().await;
        fetch.await??;
        let r = r?;
        if !r.status().is_success() { bail!("Glance: {} {}", r.status(), r.text().await.unwrap_or_default()); }
        self.wait_image(&id).await?;
        Ok(id)
    }

    /// Загрузить готовый файл образа диска в Glance и вернуть его идентификатор (для «Образ машины по SSH»).
    pub async fn upload_image_file(&self, name: &str, disk_format: &str, path: &std::path::Path, len: u64, log: &LogFn) -> Result<String> {
        let img = self.post(&format!("{}/v2/images", self.glance), json!({ "name": name, "disk_format": disk_format, "container_format": "bare", "visibility": "private" })).await?;
        let id = img["id"].as_str().unwrap_or("").to_string();
        log(&format!("  загрузка образа {name} в Glance ({})", crate::util::human_bytes(len)));
        let f = tokio::fs::File::open(path).await.with_context(|| format!("файл {}", path.display()))?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let r = self.c.put(format!("{}/v2/images/{id}/file", self.glance)).header("X-Auth-Token", &self.token).header("Content-Type", "application/octet-stream").header("Content-Length", len.to_string()).body(body).send().await?;
        if !r.status().is_success() { bail!("Glance: {} {}", r.status(), r.text().await.unwrap_or_default()); }
        self.wait_image(&id).await?;
        Ok(id)
    }

    /// Запустить ВМ из загруженного образа с заданным flavor и сетями (минимально, для развёртывания образа).
    pub async fn boot_from_image(&self, name: &str, image_id: &str, flavor: &str, networks: &[String], log: &LogFn) -> Result<String> {
        let fl = self.get(&format!("{}/flavors/detail?is_public=None", self.nova)).await?;
        let flavor_id = fl["flavors"].as_array().into_iter().flatten().find(|x| x["name"] == flavor || x["id"] == flavor).and_then(|x| x["id"].as_str().map(|s| s.to_string())).ok_or_else(|| anyhow!("нет flavor «{flavor}»"))?;
        let nets: Vec<Value> = networks.iter().filter(|n| !n.is_empty()).map(|n| json!({ "uuid": n })).collect();
        let body = json!({ "server": { "name": name, "imageRef": image_id, "flavorRef": flavor_id, "networks": if nets.is_empty() { json!("auto") } else { json!(nets) } } });
        let r = self.post(&format!("{}/servers", self.nova), body).await.context("создание ВМ Nova")?;
        let id = r["server"]["id"].as_str().unwrap_or("").to_string();
        log(&format!("  запущена ВМ «{name}» ({id})"));
        Ok(id)
    }
}

#[async_trait]
impl Driver for OpenStack {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        let v = self.get(&format!("{}/servers/detail?all_tenants=1&limit=1000", self.nova)).await?;
        Ok(v["servers"].as_array().into_iter().flatten().map(|s| VmRef { id: s["id"].as_str().unwrap_or("").into(), name: s["name"].as_str().unwrap_or("").into(), state: s["status"].as_str().unwrap_or("").into(), extra: Value::Null }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, _cfg: &Value, log: &LogFn) -> Result<Value> {
        let srv = self.get(&format!("{}/servers/{}", self.nova, vm.id)).await?["server"].clone();
        super::super::appkit::add_bytes(w, &format!("{prefix}server.json"), serde_json::to_vec_pretty(&srv)?).await?;
        let vols: Vec<String> = srv["os-extended-volumes:volumes_attached"].as_array().into_iter().flatten().filter_map(|v| v["id"].as_str().map(|s| s.to_string())).collect();
        let image_backed = srv["image"].is_object() && srv["image"]["id"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
        let tag = format!("hopebackup-{}", &crate::util::short_id()[..8]);
        let mut disks = Vec::new();
        if image_backed {
            let (v, h) = self.call(reqwest::Method::POST, &format!("{}/servers/{}/action", self.nova, vm.id), Some(json!({ "createImage": { "name": format!("{tag}-{}", vm.name), "metadata": { "hopebackup": "1" } } }))).await.context("снимок Nova (createImage)")?;
            let id = v["image_id"].as_str().map(|s| s.to_string()).or_else(|| h.get("location").and_then(|l| l.to_str().ok()).and_then(|l| l.rsplit('/').next().map(|s| s.to_string()))).ok_or_else(|| anyhow!("Nova не вернула id образа"))?;
            let r = async {
                let img = self.wait_image(&id).await?;
                let fmt = img["disk_format"].as_str().unwrap_or("qcow2").to_string();
                let sname = format!("root.{fmt}");
                let size = self.download(w, &format!("{prefix}{sname}"), &id, log).await?;
                Ok::<_, anyhow::Error>(json!({ "kind": "root", "stream": sname, "format": fmt, "size": size, "min_disk": img["min_disk"] }))
            }.await;
            let _ = self.call(reqwest::Method::DELETE, &format!("{}/v2/images/{id}", self.glance), None).await;
            disks.push(r?);
        }
        let cinder = self.cinder.clone();
        for vid in &vols {
            let cinder = cinder.as_ref().ok_or_else(|| anyhow!("у ВМ тома, а в каталоге нет Cinder"))?;
            let vol = self.get(&format!("{cinder}/volumes/{vid}")).await?["volume"].clone();
            let up = self.post(&format!("{cinder}/volumes/{vid}/action"), json!({ "os-volume_upload_image": { "image_name": format!("{tag}-{vid}"), "disk_format": "raw", "container_format": "bare", "force": true } })).await.context("выгрузка тома в образ")?;
            let id = up["os-volume_upload_image"]["image_id"].as_str().unwrap_or("").to_string();
            let r = async {
                self.wait_image(&id).await?;
                let sname = format!("volume-{vid}.raw");
                let size = self.download(w, &format!("{prefix}{sname}"), &id, log).await?;
                Ok::<_, anyhow::Error>(json!({ "kind": "volume", "volume": vid, "stream": sname, "format": "raw", "size": size, "size_gb": vol["size"], "bootable": vol["bootable"], "device": vol["attachments"][0]["device"], "volume_type": vol["volume_type"] }))
            }.await;
            let _ = self.call(reqwest::Method::DELETE, &format!("{}/v2/images/{id}", self.glance), None).await;
            disks.push(r?);
        }
        Ok(json!({ "kind": "openstack", "disks": disks, "flavor": srv["flavor"], "image_backed": image_backed }))
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let srv: Value = serde_json::from_slice(&super::super::appkit::read_small(repo, vm_stream(m, vm, "server.json")?).await?)?;
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("").to_string());
        let flavor = match cfg_str(p, "flavor") {
            Some(f) => { let fl = self.get(&format!("{}/flavors/detail?is_public=None", self.nova)).await?; fl["flavors"].as_array().into_iter().flatten().find(|x| x["name"] == f.as_str() || x["id"] == f.as_str()).and_then(|x| x["id"].as_str().map(|s| s.to_string())).ok_or_else(|| anyhow!("нет flavor {f}"))? }
            None => {
                let orig = srv["flavor"]["original_name"].as_str().or(srv["flavor"]["id"].as_str()).unwrap_or("").to_string();
                let fl = self.get(&format!("{}/flavors/detail?is_public=None", self.nova)).await?;
                fl["flavors"].as_array().into_iter().flatten().find(|x| x["name"] == orig.as_str() || x["id"] == orig.as_str()).and_then(|x| x["id"].as_str().map(|s| s.to_string())).ok_or_else(|| anyhow!("flavor «{orig}» не найден — укажите flavor"))?
            }
        };
        let nets: Vec<Value> = match cfg_str(p, "network") {
            Some(n) => vec![json!({ "uuid": n })],
            None => srv["addresses"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default().into_iter().map(|n| json!({ "uuid": n })).collect(),
        };
        let mut body = json!({ "server": { "name": name, "flavorRef": flavor, "networks": if nets.is_empty() { json!("auto") } else { json!(nets) } } });
        if let Some(k) = srv["key_name"].as_str() { body["server"]["key_name"] = json!(k); }
        let sg: Vec<Value> = srv["security_groups"].as_array().cloned().unwrap_or_default().into_iter().map(|g| json!({ "name": g["name"] })).collect();
        if !sg.is_empty() { body["server"]["security_groups"] = json!(sg); }
        let mut bdm = Vec::new();
        let mut images = Vec::new();
        for d in vm["disks"].as_array().cloned().unwrap_or_default() {
            let s = vm_stream(m, vm, d["stream"].as_str().unwrap_or(""))?;
            let img = self.upload(repo, s, &format!("{name}-{}", d["stream"].as_str().unwrap_or("disk")), d["format"].as_str().unwrap_or("raw"), log).await?;
            images.push(img.clone());
            if d["kind"] == "root" { body["server"]["imageRef"] = json!(img); continue; }
            let cinder = self.cinder.clone().ok_or_else(|| anyhow!("нет Cinder"))?;
            let mut vb = json!({ "volume": { "size": d["size_gb"], "imageRef": img, "name": format!("{name}-{}", d["volume"].as_str().unwrap_or("vol")) } });
            if let Some(t) = d["volume_type"].as_str() { vb["volume"]["volume_type"] = json!(t); }
            let v = self.post(&format!("{cinder}/volumes"), vb).await.context("создание тома из образа")?;
            let vid = v["volume"]["id"].as_str().unwrap_or("").to_string();
            poll(7200, 5, "том Cinder", || async { let x = self.get(&format!("{cinder}/volumes/{vid}")).await?; match x["volume"]["status"].as_str().unwrap_or("") { "available" => Ok(Some(x)), "error" => bail!("том {vid}: error"), _ => Ok(None) } }).await?;
            bdm.push(json!({ "uuid": vid, "source_type": "volume", "destination_type": "volume", "boot_index": if d["bootable"] == "true" && bdm.is_empty() && body["server"]["imageRef"].is_null() { 0 } else { -1 }, "delete_on_termination": false }));
        }
        if !bdm.is_empty() { body["server"]["block_device_mapping_v2"] = json!(bdm); }
        let r = self.post(&format!("{}/servers", self.nova), body).await.context("создание ВМ Nova")?;
        let sid = r["server"]["id"].as_str().unwrap_or("").to_string();
        let st = poll(1800, 5, "запуск ВМ", || async { let x = self.get(&format!("{}/servers/{sid}", self.nova)).await?; match x["server"]["status"].as_str().unwrap_or("") { "ACTIVE" | "SHUTOFF" => Ok(Some(x)), "ERROR" => bail!("ВМ в состоянии ERROR: {}", x["server"]["fault"]["message"]), _ => Ok(None) } }).await?;
        if !super::super::appkit::bool_param(p, "start", true) { let _ = self.post(&format!("{}/servers/{sid}/action", self.nova), json!({ "os-stop": null })).await; }
        log(&format!("  ВМ {name} создана ({sid}), образы: {}", images.join(", ")));
        Ok(json!({ "vm": name, "id": sid, "status": st["server"]["status"], "images": images }))
    }
}
