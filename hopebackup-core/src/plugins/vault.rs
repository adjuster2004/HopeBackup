//! HashiCorp Vault / OpenBao со встроенным хранилищем raft: снапшот через HTTP API
//! (`GET /v1/sys/storage/raft/snapshot`) потоком в репозиторий, плюс конфигурация (`/etc/vault.d`).
//! Восстановление — `POST /v1/sys/storage/raft/snapshot-force` (тоже потоком) в работающий кластер.
//!
//! Снапшот зашифрован ключами исходного кластера: после восстановления Vault разблокируется ключами
//! (или механизмом auto-unseal) ИСХОДНОГО кластера. HopeBackup ключи разблокировки не хранит.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Vault;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "addr": { "type": "string", "title": "Адрес Vault (VAULT_ADDR)", "default": "https://127.0.0.1:8200" },
        "token": { "type": "string", "format": "secret", "title": "Токен", "description": "Политика: path \"sys/storage/raft/snapshot\" { capabilities = [\"read\"] }" },
        "namespace": { "type": "string", "title": "Пространство имён (Enterprise)" },
        "fingerprint": { "type": "string", "title": "Отпечаток сертификата Vault", "description": "SHA-256; пусто — проверка по системным CA" },
        "config_paths": { "type": "array", "items": { "type": "string" }, "title": "Конфигурация", "description": "По умолчанию /etc/vault.d (с TLS-ключами)" }
    } })
}

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "addr": { "type": "string", "title": "Целевой Vault", "description": "Пусто — адрес из экземпляра. Кластер инициализирован и разблокирован" },
        "token": { "type": "string", "format": "secret", "title": "Токен целевого Vault", "description": "Пусто — токен экземпляра" },
        "fingerprint": { "type": "string", "title": "Отпечаток сертификата целевого Vault" },
        "force": { "type": "boolean", "title": "snapshot-force (другой кластер)", "default": true, "description": "Нужно, если ключи целевого кластера отличаются (новый кластер)" },
        "restore_config": { "type": "boolean", "title": "Восстановить конфигурацию /etc/vault.d", "default": false }
    } })
}

pub fn defaults() -> Value { json!({ "addr": "https://127.0.0.1:8200", "config_paths": ["/etc/vault.d"] }) }

struct V {
    c: reqwest::Client,
    addr: String,
    token: String,
    ns: String,
}

fn client(p: &Value) -> Result<V> {
    let addr = cfg_str(p, "addr").unwrap_or_else(|| "https://127.0.0.1:8200".into()).trim_end_matches('/').to_string();
    let token = cfg_str(p, "token").ok_or_else(|| anyhow!("не задан токен Vault"))?;
    let fp = cfg_str(p, "fingerprint").unwrap_or_default();
    let c = crate::tlsutil::client(if crate::tlsutil::normalize_fp(&fp).is_empty() { None } else { Some(&fp) })?;
    Ok(V { c, addr, token, ns: cfg_str(p, "namespace").unwrap_or_default() })
}

impl V {
    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.c.request(m, format!("{}{path}", self.addr)).header("X-Vault-Token", &self.token);
        if !self.ns.is_empty() { r = r.header("X-Vault-Namespace", &self.ns); }
        r
    }

    async fn json(&self, path: &str) -> Result<Value> {
        let r = self.req(reqwest::Method::GET, path).timeout(Duration::from_secs(30)).send().await.map_err(|e| anyhow!("Vault {path}: {e}"))?;
        let st = r.status();
        let v: Value = r.json().await.unwrap_or_default();
        // /sys/health отвечает 429/472/473/501/503 по состоянию узла — тело всё равно полезно.
        if !st.is_success() && path != "/v1/sys/health" { bail!("Vault {path}: {st} {}", v["errors"]); }
        Ok(v)
    }
}

#[async_trait]
impl Plugin for Vault {
    fn id(&self) -> &'static str { "vault" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let v = client(cfg)?;
        let h = v.json("/v1/sys/health").await?;
        let raft = v.json("/v1/sys/storage/raft/configuration").await;
        let servers = raft.as_ref().ok().and_then(|r| r["data"]["config"]["servers"].as_array().cloned()).unwrap_or_default();
        Ok(json!({
            "version": h["version"], "cluster_name": h["cluster_name"], "initialized": h["initialized"], "sealed": h["sealed"], "standby": h["standby"],
            "storage": if raft.is_ok() { "raft" } else { "не raft или нет прав" },
            "raft_servers": servers.iter().map(|s| json!({ "node_id": s["node_id"], "address": s["address"], "leader": s["leader"], "voter": s["voter"] })).collect::<Vec<_>>(),
            "raft_error": raft.err().map(|e| format!("{e:#}")),
        }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let v = client(&ctx.config)?;
        let log = &ctx.log;
        let h = v.json("/v1/sys/health").await?;
        if h["sealed"] == true { bail!("Vault запечатан (sealed) — снапшот невозможен"); }
        log(&format!("Vault {}: снапшот raft", h["version"].as_str().unwrap_or("?")));
        let r = v.req(reqwest::Method::GET, "/v1/sys/storage/raft/snapshot").timeout(Duration::from_secs(6 * 3600)).send().await.map_err(|e| anyhow!("Vault: {e}"))?;
        if !r.status().is_success() {
            let st = r.status();
            bail!("Vault: снапшот — {st} {}", r.text().await.unwrap_or_default().trim());
        }
        // Тело ответа — потоком в репозиторий, без промежуточного файла.
        let body = r.bytes_stream().map(|x| x.map_err(std::io::Error::other));
        let rd = tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(body));
        let mut w = ctx.repo.writer();
        let st = w.add_stream("raft.snap", rd, None).await.context("снапшот Vault")?;
        if st.size < 64 { bail!("Vault вернул пустой снапшот ({} байт)", st.size); }
        let paths: Vec<String> = { let l = crate::connectors::net::p_list(&ctx.config, "config_paths"); if l.is_empty() { vec!["/etc/vault.d".into()] } else { l } }.into_iter().filter(|p| std::path::Path::new(p).exists()).collect();
        if !paths.is_empty() {
            let (rp, wp) = std::io::pipe()?;
            let pp = paths.clone();
            let prod = std::thread::spawn(move || super::files::write_tar(wp, &pp, &crate::exclude::Excludes::default()));
            w.add_stream("config.tar", rp, None).await?;
            prod.join().map_err(|_| anyhow!("поток tar"))??;
        }
        let meta = json!({ "version": h["version"], "cluster_name": h["cluster_name"], "config_paths": paths, "snapshot_bytes": st.size });
        let (header, stats) = w.finish(&ctx.instance, "vault", "full", json!({ "version": h["version"] }), meta).await?;
        log(&format!("снапшот {}", crate::util::human_bytes(st.size)));
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let p = &ctx.params;
        let v = client(p)?;
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        if p.get("restore_config").and_then(|x| x.as_bool()).unwrap_or(false) {
            if let Some(s) = m.streams.iter().find(|s| s.name == "config.tar") {
                log("восстановление конфигурации Vault");
                let (reader, fetch) = ctx.repo.stream_reader(s)?;
                let u = tokio::task::spawn_blocking(move || -> Result<()> { let mut a = tar::Archive::new(reader); a.set_preserve_permissions(true); a.set_preserve_ownerships(crate::util::is_root()); a.unpack("/")?; Ok(()) });
                let (u, f) = tokio::join!(u, fetch);
                u??; f??;
            }
        }
        let s = m.streams.iter().find(|s| s.name == "raft.snap").ok_or_else(|| anyhow!("в копии нет снапшота raft"))?.clone();
        let h = v.json("/v1/sys/health").await?;
        if h["initialized"] != true || h["sealed"] == true { bail!("целевой Vault должен быть инициализирован и разблокирован (initialized={}, sealed={})", h["initialized"], h["sealed"]); }
        let force = p.get("force").and_then(|x| x.as_bool()).unwrap_or(true);
        let path = if force { "/v1/sys/storage/raft/snapshot-force" } else { "/v1/sys/storage/raft/snapshot" };
        log(&format!("загрузка снапшота {} в {}{path}", crate::util::human_bytes(s.size), v.addr));
        // Поток снапшота → тело запроса.
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(8);
        struct Tx(tokio::sync::mpsc::Sender<std::io::Result<bytes::Bytes>>);
        impl std::io::Write for Tx {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.blocking_send(Ok(bytes::Bytes::copy_from_slice(b))).map_err(|_| std::io::Error::other("получатель закрыт"))?; Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let repo = ctx.repo.clone();
        let feed = tokio::spawn(async move { repo.restore_stream(&s, Tx(tx)).await });
        let body = reqwest::Body::wrap_stream(tokio_stream_from(rx));
        let r = v.req(reqwest::Method::POST, path).timeout(Duration::from_secs(6 * 3600)).body(body).send().await;
        let fed = feed.await?;
        let r = r.map_err(|e| anyhow!("Vault: {e}"))?;
        fed?;
        if !r.status().is_success() { let st = r.status(); bail!("Vault: восстановление — {st} {}", r.text().await.unwrap_or_default().trim()); }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = v.json("/v1/sys/health").await.unwrap_or_default();
        let sealed = after["sealed"] == true;
        log(if sealed { "снапшот применён; Vault запечатан — разблокируйте ключами исходного кластера" } else { "снапшот применён" });
        Ok(json!({ "addr": v.addr, "sealed": sealed, "note": "токены и ключи разблокировки — исходного кластера" }))
    }
}

/// Поток из получателя mpsc (без зависимости tokio-stream).
fn tokio_stream_from(mut rx: tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
