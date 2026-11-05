//! Яндекс Диск через REST API (`cloud-api.yandex.net/v1/disk`), токен OAuth.
//! Корень — `app:/HopeBackup` (папка приложения «Приложения/<имя>», права `cloud_api:disk.app_folder`)
//! или `disk:/…` (весь диск, права `cloud_api:disk.read` и `cloud_api:disk.write`).

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashSet;

use super::objstore::{fail, send_retry, take_range, ObjMeta, ObjStore};

pub struct YaDisk {
    http: reqwest::Client,
    api: String,
    token: String,
    /// `app:/HopeBackup` — без `/` в конце.
    root: String,
    made: parking_lot::Mutex<HashSet<String>>,
}

impl YaDisk {
    pub fn new(api: &str, token: &str, root: &str) -> Result<YaDisk> {
        if token.trim().is_empty() { bail!("Яндекс Диск: нет токена (кнопка «Подключить» или токен OAuth)"); }
        let mut root = root.trim().trim_end_matches('/').to_string();
        if root.is_empty() { root = "app:/HopeBackup".into(); }
        if !root.starts_with("app:/") && !root.starts_with("disk:/") { root = format!("disk:/{}", root.trim_start_matches('/')); }
        let api = if api.trim().is_empty() { "https://cloud-api.yandex.net".to_string() } else { api.trim().trim_end_matches('/').to_string() };
        Ok(YaDisk { http: crate::tlsutil::client(None)?, api, token: token.trim().into(), root, made: parking_lot::Mutex::new(HashSet::new()) })
    }

    fn path(&self, key: &str) -> String { if key.is_empty() { self.root.clone() } else { format!("{}/{}", self.root, key.trim_matches('/')) } }

    fn req(&self, m: Method, ep: &str, q: &[(&str, String)]) -> reqwest::RequestBuilder {
        self.http.request(m, format!("{}/v1/disk{ep}", self.api)).header("Authorization", format!("OAuth {}", self.token)).query(q).timeout(std::time::Duration::from_secs(120))
    }

    async fn href(&self, ep: &str, q: &[(&str, String)], what: &str) -> Result<Option<String>> {
        let r = send_retry(|| self.req(Method::GET, ep, q), what).await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        if !r.status().is_success() { return Err(fail(r, what).await); }
        let v: Value = r.json().await?;
        Ok(v.get("href").and_then(|h| h.as_str()).map(|s| s.to_string()))
    }

    async fn mkdirs(&self, dir: &str) -> Result<()> {
        let mut acc = String::new();
        let mut parts = vec![String::new()];
        for seg in dir.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
            if !acc.is_empty() { acc.push('/'); }
            acc.push_str(seg);
            parts.push(acc.clone());
        }
        // `app:/` и `disk:/` сами по себе существуют; создаём корневую папку и вложенные.
        for p in parts {
            if self.made.lock().contains(&p) { continue; }
            let path = self.path(&p);
            if path == "app:" || path == "disk:" || path.ends_with(":/") { self.made.lock().insert(p); continue; }
            let r = send_retry(|| self.req(Method::PUT, "/resources", &[("path", path.clone())]), "Яндекс Диск: папка").await?;
            let st = r.status().as_u16();
            if !(r.status().is_success() || st == 409) { return Err(fail(r, &format!("Яндекс Диск: создание папки {path}")).await); }
            self.made.lock().insert(p);
        }
        Ok(())
    }
}

#[async_trait]
impl ObjStore for YaDisk {
    fn id(&self) -> String { format!("yadisk|{}|{}", self.root, crate::util::sha256_hex(&self.token)[..12].to_string()) }

    async fn list(&self, prefix: &str, shallow: bool) -> Result<Vec<ObjMeta>> {
        let mut out = Vec::new();
        let mut todo = vec![prefix.trim_matches('/').to_string()];
        while let Some(dir) = todo.pop() {
            let mut offset = 0u64;
            loop {
                let q = [("path", self.path(&dir)), ("limit", "1000".to_string()), ("offset", offset.to_string()), ("fields", "_embedded.items.name,_embedded.items.type,_embedded.items.size,_embedded.items.modified,_embedded.total".to_string())];
                let r = send_retry(|| self.req(Method::GET, "/resources", &q), "Яндекс Диск: список").await?;
                if r.status().as_u16() == 404 { break; }
                if !r.status().is_success() { return Err(fail(r, "Яндекс Диск: список").await); }
                let v: Value = r.json().await?;
                let items = v.pointer("/_embedded/items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                let n = items.len() as u64;
                for it in items {
                    let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    let key = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                    if it.get("type").and_then(|x| x.as_str()) == Some("dir") {
                        if !shallow { todo.push(key); }
                        continue;
                    }
                    let lm = it.get("modified").and_then(|x| x.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now);
                    out.push(ObjMeta { key, size: it.get("size").and_then(|x| x.as_u64()).unwrap_or(0), last_modified: lm });
                }
                offset += n;
                let total = v.pointer("/_embedded/total").and_then(|x| x.as_u64()).unwrap_or(0);
                if n == 0 || offset >= total { break; }
            }
        }
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let Some(href) = self.href("/resources/download", &[("path", self.path(key))], "Яндекс Диск: ссылка на файл").await? else { return Ok(None) };
        let r = send_retry(|| self.http.get(&href).header("Authorization", format!("OAuth {}", self.token)).timeout(std::time::Duration::from_secs(600)), "Яндекс Диск: чтение").await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        if !r.status().is_success() { return Err(fail(r, &format!("Яндекс Диск: чтение {key}")).await); }
        Ok(Some(r.bytes().await?))
    }

    async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes> {
        if len == 0 { return Ok(Bytes::new()); }
        let href = self.href("/resources/download", &[("path", self.path(key))], "Яндекс Диск: ссылка на файл").await?.ok_or_else(|| anyhow!("Яндекс Диск: нет файла {key}"))?;
        let range = format!("bytes={}-{}", off, off + len - 1);
        let r = send_retry(|| self.http.get(&href).header("Authorization", format!("OAuth {}", self.token)).header("Range", range.clone()).timeout(std::time::Duration::from_secs(600)), "Яндекс Диск: чтение").await?;
        let st = r.status().as_u16();
        if !r.status().is_success() { return Err(fail(r, &format!("Яндекс Диск: чтение {key}")).await); }
        take_range(st, r.bytes().await?, off, len)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let r = send_retry(|| self.req(Method::GET, "/resources", &[("path", self.path(key)), ("fields", "name".into())]), "Яндекс Диск").await?;
        match r.status().as_u16() {
            404 => Ok(false),
            s if (200..300).contains(&s) => Ok(true),
            _ => Err(fail(r, "Яндекс Диск").await),
        }
    }

    async fn put(&self, key: &str, body: Bytes, _immutable: bool) -> Result<()> {
        if let Some((dir, _)) = key.rsplit_once('/') { self.mkdirs(dir).await?; } else { self.mkdirs("").await?; }
        let href = self.href("/resources/upload", &[("path", self.path(key)), ("overwrite", "true".into())], "Яндекс Диск: ссылка для записи").await?.ok_or_else(|| anyhow!("Яндекс Диск: нет ссылки для записи {key}"))?;
        let r = send_retry(|| self.http.put(&href).body(body.clone()).timeout(std::time::Duration::from_secs(1800)), "Яндекс Диск: запись").await?;
        if !r.status().is_success() { return Err(fail(r, &format!("Яндекс Диск: запись {key}")).await); }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let r = send_retry(|| self.req(Method::DELETE, "/resources", &[("path", self.path(key)), ("permanently", "true".into())]), "Яндекс Диск: удаление").await?;
        if r.status().as_u16() == 404 || r.status().is_success() { return Ok(()); }
        Err(fail(r, &format!("Яндекс Диск: удаление {key}")).await)
    }

    async fn check(&self) -> Result<serde_json::Value> {
        let r = send_retry(|| self.req(Method::GET, "", &[]), "Яндекс Диск").await?;
        if !r.status().is_success() { return Err(fail(r, "Яндекс Диск").await); }
        let v: Value = r.json().await?;
        self.mkdirs("").await?;
        let total = v.get("total_space").and_then(|x| x.as_u64());
        let used = v.get("used_space").and_then(|x| x.as_u64());
        Ok(serde_json::json!({ "account": v.pointer("/user/login").and_then(|x| x.as_str()).unwrap_or(""), "total": total, "used": used, "free": total.zip(used).map(|(t, u)| t.saturating_sub(u)) }))
    }
}
