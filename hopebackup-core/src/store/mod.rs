//! Движок хранения: дедупликация, сжатие и шифрование на стороне источника.

pub mod backend;
pub mod cindex;
pub mod crypto;
pub mod http;
pub mod local;
pub mod prune;
pub mod repo;
pub mod s3;
pub mod snapfile;
pub mod types;

use anyhow::Result;
use std::sync::Arc;

pub use backend::Backend;
pub use http::Remote;
pub use repo::Repo;

/// Открыть хранилище по адресу: `https://узел:порт` (с репозиторием и токеном) или локальный каталог.
pub fn backend_for(r: &Remote) -> Result<Arc<dyn Backend>> {
    if let Some(c) = &r.cloud {
        return Ok(Arc::new(s3::S3Backend::open_store(c.store()?, "", &r.repo)));
    }
    if r.is_s3() {
        let a = r.s3.as_ref().ok_or_else(|| anyhow::anyhow!("у площадки S3 нет параметров доступа"))?;
        return Ok(Arc::new(s3::S3Backend::open(a, &r.repo)?));
    }
    if r.url.starts_with("http://") || r.url.starts_with("https://") {
        Ok(Arc::new(http::HttpBackend::new(r)?))
    } else {
        let path = r.url.strip_prefix("file://").unwrap_or(&r.url);
        Ok(Arc::new(local::LocalBackend::open(std::path::Path::new(path).join(&r.repo))?))
    }
}

/// Проверка площадки: узел — TLS с закреплённым отпечатком и ответ /v1/health; S3 — доступ к бакету.
/// Место на диске узла площадки HopeBackup: (всего, занято). `None` — не узел или узел старше 0.8.20.
/// Готовые образы (qcow2) на узле площадки: `{root, sets: [...]}`. `None` — не узел или узел старше 0.9.1.
pub async fn node_images(r: &Remote) -> Option<serde_json::Value> {
    if r.is_s3() || !(r.url.starts_with("http://") || r.url.starts_with("https://")) || r.token.is_empty() { return None; }
    let fp = if r.url.starts_with("https://") { Some(r.fingerprint.as_str()) } else { None };
    let c = crate::tlsutil::client(fp).ok()?;
    let resp = c.get(format!("{}/v1/r/{}/images", r.url.trim_end_matches('/'), r.repo)).bearer_auth(&r.token).timeout(std::time::Duration::from_secs(15)).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    resp.json().await.ok()
}

pub async fn node_disk(r: &Remote) -> Option<(u64, u64)> {
    if r.is_s3() || !(r.url.starts_with("http://") || r.url.starts_with("https://")) || r.token.is_empty() { return None; }
    let fp = if r.url.starts_with("https://") { Some(r.fingerprint.as_str()) } else { None };
    let c = crate::tlsutil::client(fp).ok()?;
    let resp = c.get(format!("{}/v1/disk", r.url.trim_end_matches('/'))).bearer_auth(&r.token).timeout(std::time::Duration::from_secs(15)).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    let v: serde_json::Value = resp.json().await.ok()?;
    let t = v["total"].as_u64()?;
    (t > 0).then(|| (t, v["used"].as_u64().unwrap_or(0)))
}

pub async fn health(r: &Remote) -> Result<()> {
    if let Some(c) = &r.cloud { c.store()?.check().await?; return Ok(()); }
    if r.is_s3() { return s3::health(r).await; }
    if !r.url.starts_with("http://") && !r.url.starts_with("https://") { return Ok(()); }
    let fp = if r.url.starts_with("https://") { Some(r.fingerprint.as_str()) } else { None };
    let c = crate::tlsutil::client(fp)?;
    let resp = c.get(format!("{}/v1/health", r.url.trim_end_matches('/'))).timeout(std::time::Duration::from_secs(20)).send().await?;
    if !resp.status().is_success() { anyhow::bail!("ответ {}", resp.status()); }
    Ok(())
}
