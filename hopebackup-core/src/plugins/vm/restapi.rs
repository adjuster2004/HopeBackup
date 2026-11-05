//! SpaceVM и VMmanager 6: платформы на KVM — список ВМ, их узлы и конфигурация из API платформы, диски —
//! через libvirt узла по SSH (внешний снимок без остановки ВМ). Восстановление — файлами на узел
//! (диски и описание libvirt в каталог «Хранилище») для импорта средствами платформы: у их API нет
//! загрузки образа диска ВМ потоком.
//!
//! SpaceVM: ключ интеграции (учётка пустая) или имя и пароль → «Authorization: jwt …», /api/domains/,
//! /api/nodes/{id}/. VMmanager: POST /auth/v4/public/token → x-xsrf-token, /vm/v3/host, /vm/v3/node/{id}.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::libvirt::Libvirt;
use super::{q, vm_stream, Driver, Host, VmRef};
use crate::plugins::{cfg_str, LogFn};
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::Manifest;

pub struct NodeApi { kind: String, c: reqwest::Client, base: String, token: String, cfg: Value }

impl NodeApi {
    pub async fn login(kind: &str, cfg: &Value, _log: &LogFn) -> Result<NodeApi> {
        let c = super::http(cfg)?;
        let url = cfg_str(cfg, "url").ok_or_else(|| anyhow!("не задан адрес {kind}"))?.trim_end_matches('/').to_string();
        let user = cfg_str(cfg, "user").unwrap_or_default();
        let pass = cfg_str(cfg, "password").unwrap_or_default();
        let (base, token) = if kind == "spacevm" {
            let base = url.trim_end_matches("/api").to_string();
            let token = if user.is_empty() { pass } else {
                let (u, ldap) = match user.strip_prefix("ldap:") { Some(x) => (x.to_string(), true), None => (user.clone(), false) };
                let r: Value = c.post(format!("{base}/auth/")).json(&json!({ "username": u, "password": pass, "ldap": ldap })).send().await.context("SpaceVM недоступен")?.json().await.context("SpaceVM: неверное имя или пароль")?;
                r["token"].as_str().or(r["jwt"].as_str()).unwrap_or("").to_string()
            };
            (format!("{base}/api"), token)
        } else {
            let base = url.trim_end_matches("/vm/v3").trim_end_matches("/vm").to_string();
            let r: Value = c.post(format!("{base}/auth/v4/public/token")).json(&json!({ "email": user, "password": pass })).send().await.context("VMmanager недоступен")?.json().await.context("VMmanager: неверный логин или пароль")?;
            (base, r["token"].as_str().unwrap_or("").to_string())
        };
        if token.is_empty() { bail!("{kind}: не получен токен — проверьте учётку"); }
        Ok(NodeApi { kind: kind.into(), c, base, token, cfg: cfg.clone() })
    }
    async fn get(&self, path: &str) -> Result<Value> {
        let rb = self.c.get(format!("{}{path}", self.base));
        let rb = if self.kind == "spacevm" { rb.header("Authorization", format!("jwt {}", self.token)) } else { rb.header("x-xsrf-token", &self.token).header("Cookie", format!("ses6={}", self.token)) };
        let r = rb.send().await.with_context(|| format!("{} {path}", self.kind))?;
        if !r.status().is_success() { bail!("{} {path}: {}", self.kind, r.status()); }
        Ok(r.json().await?)
    }
    /// Узел ВМ и имя домена libvirt на нём.
    async fn locate(&self, vm: &VmRef, log: &LogFn) -> Result<(Libvirt, String, String)> {
        let node = vm.extra["node"].as_str().unwrap_or("").to_string();
        let addr = if self.kind == "spacevm" { let n = self.get(&format!("/nodes/{node}/")).await?; n["management_ip"].as_str().or(n["ip"].as_str()).unwrap_or("").to_string() }
            else { let n = self.get(&format!("/vm/v3/node/{node}")).await?; if n["ip"].is_object() { n["ip"]["ip"].as_str().unwrap_or("").to_string() } else { n["ip_addr"].as_str().or(n["ip"].as_str()).unwrap_or("").to_string() } };
        if addr.is_empty() { bail!("у узла {node} нет адреса"); }
        let host = if cfg_str(&self.cfg, "ssh_host").as_deref() == Some("local") || addr == "127.0.0.1" { Host::Local } else { Host::connect(&self.cfg, Some(&addr), log).await? };
        let lv = Libvirt::new(host, &self.cfg);
        // Домен — по UUID ВМ (так его называет платформа), иначе по имени.
        let uuid = vm.extra["uuid"].as_str().unwrap_or(&vm.id).to_string();
        let dom = match lv.virsh(&format!("domname {}", q(&uuid)), log).await { Ok(n) => n.trim().to_string(), Err(_) => vm.name.clone() };
        Ok((lv, dom, addr))
    }
}

#[async_trait]
impl Driver for NodeApi {
    async fn list(&self, _log: &LogFn) -> Result<Vec<VmRef>> {
        if self.kind == "spacevm" {
            let mut out = Vec::new();
            let mut url = "/domains/?limit=100&offset=0".to_string();
            for _ in 0..1000 {
                let v = self.get(&url).await?;
                for d in v["results"].as_array().cloned().unwrap_or_default() {
                    let node = if d["node"].is_object() { d["node"]["id"].as_str().unwrap_or("").to_string() } else { d["node"].as_str().unwrap_or("").to_string() };
                    out.push(VmRef { id: d["id"].as_str().unwrap_or("").into(), name: d["verbose_name"].as_str().unwrap_or("").into(), state: match d["user_power_state"].as_u64() { Some(3) => "running".into(), Some(2) => "paused".into(), _ => "off".into() }, extra: json!({ "node": node, "uuid": d["id"] }) });
                }
                match v["next"].as_str() { Some(n) if !n.is_empty() => url = n.split("/api").nth(1).unwrap_or("").to_string(), _ => break }
            }
            return Ok(out);
        }
        let v = self.get("/vm/v3/host").await?;
        Ok(v["list"].as_array().into_iter().flatten().map(|h| VmRef { id: h["id"].to_string(), name: h["name"].as_str().unwrap_or("").into(), state: h["state"].as_str().unwrap_or("").into(),
            extra: json!({ "node": if h["node"].is_object() { h["node"]["id"].to_string() } else { h["node"].to_string() }, "uuid": h["uuid"].as_str().or(h["smbios_uuid"].as_str()) }) }).collect())
    }

    async fn backup_vm(&self, vm: &VmRef, w: &mut SnapshotWriter, prefix: &str, cfg: &Value, log: &LogFn) -> Result<Value> {
        let conf = if self.kind == "spacevm" { self.get(&format!("/domains/{}/", vm.id)).await? } else { self.get(&format!("/vm/v3/host/{}", vm.id)).await? };
        super::super::appkit::add_bytes(w, &format!("{prefix}platform.json"), serde_json::to_vec_pretty(&conf)?).await?;
        let (lv, dom, addr) = self.locate(vm, log).await?;
        log(&format!("  узел {addr}, домен libvirt {dom}"));
        let mut m = lv.backup_domain(&dom, w, prefix, cfg, log).await?;
        m["kind"] = json!(self.kind);
        m["node_addr"] = json!(addr);
        Ok(m)
    }

    async fn restore_vm(&self, repo: &Arc<Repo>, m: &Manifest, vm: &Value, p: &Value, log: &LogFn) -> Result<Value> {
        let addr = cfg_str(p, "node").or_else(|| vm["node_addr"].as_str().map(|s| s.to_string())).unwrap_or_default();
        let host = if addr.is_empty() || addr == "127.0.0.1" || cfg_str(p, "ssh_host").as_deref() == Some("local") { Host::Local } else { Host::connect(p, Some(&addr), log).await? };
        let name = cfg_str(p, "new_name").unwrap_or_else(|| vm["name"].as_str().unwrap_or("vm").to_string());
        let dir = cfg_str(p, "storage").unwrap_or_else(|| format!("/var/tmp/hopebackup-restore/{}", super::safe(&name)));
        host.sh(&format!("mkdir -p {}", q(&dir)), log).await?;
        let prefix = vm["prefix"].as_str().unwrap_or("");
        for s in m.streams.iter().filter(|s| s.name.starts_with(prefix)) {
            let f = format!("{dir}/{}", s.name.trim_start_matches(prefix));
            host.stream_in(repo, s, &format!("cat > {}", q(&f)), log).await?;
        }
        let _ = vm_stream(m, vm, "domain.xml")?;
        let how = if self.kind == "spacevm" { "SpaceVM: «Хранилища → Диски → Загрузить» (или импорт ВМ) и подключите диски к новой ВМ" } else { "VMmanager: «Образы → Загрузить» и создайте ВМ из образа" };
        log(&format!("  файлы ВМ на узле {} в {dir}; импорт — {how}", host.label()));
        Ok(json!({ "vm": name, "node": host.label(), "dir": dir, "import": how }))
    }
}
