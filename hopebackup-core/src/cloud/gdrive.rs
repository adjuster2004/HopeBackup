//! Google Диск (Drive API v3), OAuth 2.0 с правами `drive.file` (приложение видит только свои файлы).
//!
//! Раскладка плоская: все объекты репозитория — файлы в одной папке площадки, ключ (`main/packs/ab/….pack`)
//! — имя файла. В Google Диске имена не уникальны и папки создаются гонками; плоская папка избавляет от
//! дублей каталогов при одновременной записи с нескольких агентов. Одинаковые имена (повтор того же
//! содержимого) безвредны: читается первый, удаляются все.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;

use super::objstore::{fail, send_retry, take_range, ObjMeta, ObjStore};

const FOLDER: &str = "application/vnd.google-apps.folder";

pub struct GDrive {
    http: reqwest::Client,
    api: String,
    upload: String,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    folder_name: String,
    folder_id: tokio::sync::Mutex<String>,
    access: tokio::sync::Mutex<(String, i64)>,
    /// Имя → идентификаторы файлов (обновляется списком).
    ids: parking_lot::Mutex<HashMap<String, Vec<String>>>,
    /// Список папки целиком на 30 с (очистка спрашивает его для каждого экземпляра).
    listing: parking_lot::Mutex<Option<(std::time::Instant, Vec<ObjMeta>)>>,
}

fn q_escape(s: &str) -> String { s.replace('\\', "\\\\").replace('\'', "\\'") }

impl GDrive {
    pub fn new(endpoint: &str, client_id: &str, client_secret: &str, refresh_token: &str, folder: &str, folder_id: &str) -> Result<GDrive> {
        if refresh_token.trim().is_empty() { bail!("Google Диск: не подключён (кнопка «Подключить»)"); }
        if client_id.trim().is_empty() { bail!("Google Диск: не задан Client ID приложения OAuth"); }
        let ep = endpoint.trim().trim_end_matches('/');
        let (api, upload, token_url) = if ep.is_empty() {
            ("https://www.googleapis.com/drive/v3".to_string(), "https://www.googleapis.com/upload/drive/v3".to_string(), "https://oauth2.googleapis.com/token".to_string())
        } else {
            (format!("{ep}/drive/v3"), format!("{ep}/upload/drive/v3"), format!("{ep}/token"))
        };
        Ok(GDrive {
            http: crate::tlsutil::client(None)?,
            api,
            upload,
            token_url,
            client_id: client_id.trim().into(),
            client_secret: client_secret.trim().into(),
            refresh_token: refresh_token.trim().into(),
            folder_name: if folder.trim().is_empty() { "HopeBackup".into() } else { folder.trim().trim_matches('/').into() },
            folder_id: tokio::sync::Mutex::new(folder_id.trim().into()),
            access: tokio::sync::Mutex::new((String::new(), 0)),
            ids: parking_lot::Mutex::new(HashMap::new()),
            listing: parking_lot::Mutex::new(None),
        })
    }

    async fn token(&self) -> Result<String> {
        let mut g = self.access.lock().await;
        if !g.0.is_empty() && g.1 > Utc::now().timestamp() + 60 { return Ok(g.0.clone()); }
        let (tok, exp) = super::oauth::refresh("gdrive", &self.token_url, &self.client_id, &self.client_secret, &self.refresh_token).await?;
        *g = (tok.access_token.clone(), exp);
        Ok(tok.access_token)
    }

    async fn call(&self, m: Method, url: &str, q: &[(&str, String)], body: Option<(Bytes, &str)>, extra: &[(&str, String)], what: &str) -> Result<reqwest::Response> {
        let tok = self.token().await?;
        send_retry(|| {
            let mut r = self.http.request(m.clone(), url).bearer_auth(&tok).query(q).timeout(std::time::Duration::from_secs(1800));
            for (k, v) in extra { r = r.header(*k, v.clone()); }
            if let Some((b, ct)) = &body { r = r.header("Content-Type", *ct).body(b.clone()); }
            r
        }, what).await
    }

    /// Папка площадки: заданная или найденная/созданная по имени в корне диска.
    async fn folder(&self) -> Result<String> {
        let mut g = self.folder_id.lock().await;
        if !g.is_empty() { return Ok(g.clone()); }
        let q = format!("name = '{}' and mimeType = '{FOLDER}' and 'root' in parents and trashed = false", q_escape(&self.folder_name));
        let r = self.call(Method::GET, &format!("{}/files", self.api), &[("q", q), ("fields", "files(id)".into()), ("spaces", "drive".into())], None, &[], "Google Диск: папка").await?;
        if !r.status().is_success() { return Err(fail(r, "Google Диск: папка").await); }
        let v: Value = r.json().await?;
        let id = match v.pointer("/files/0/id").and_then(|x| x.as_str()) {
            Some(id) => id.to_string(),
            None => {
                let meta = json!({ "name": self.folder_name, "mimeType": FOLDER, "parents": ["root"] });
                let r = self.call(Method::POST, &format!("{}/files", self.api), &[("fields", "id".into())], Some((Bytes::from(meta.to_string()), "application/json")), &[], "Google Диск: создание папки").await?;
                if !r.status().is_success() { return Err(fail(r, "Google Диск: создание папки").await); }
                let v: Value = r.json().await?;
                v.get("id").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("Google Диск: нет id папки"))?.to_string()
            }
        };
        *g = id.clone();
        Ok(id)
    }

    async fn find(&self, key: &str) -> Result<Vec<String>> {
        if let Some(v) = self.ids.lock().get(key) { if !v.is_empty() { return Ok(v.clone()); } }
        let fid = self.folder().await?;
        let q = format!("name = '{}' and '{fid}' in parents and trashed = false", q_escape(key));
        let r = self.call(Method::GET, &format!("{}/files", self.api), &[("q", q), ("fields", "files(id)".into()), ("spaces", "drive".into())], None, &[], "Google Диск: поиск").await?;
        if !r.status().is_success() { return Err(fail(r, "Google Диск: поиск").await); }
        let v: Value = r.json().await?;
        let ids: Vec<String> = v.get("files").and_then(|f| f.as_array()).map(|a| a.iter().filter_map(|x| x.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).collect()).unwrap_or_default();
        if !ids.is_empty() { self.ids.lock().insert(key.to_string(), ids.clone()); }
        Ok(ids)
    }
}

#[async_trait]
impl ObjStore for GDrive {
    fn id(&self) -> String { format!("gdrive|{}|{}|{}", self.client_id, self.folder_name, crate::util::sha256_hex(&self.refresh_token)[..12].to_string()) }

    async fn list(&self, prefix: &str, shallow: bool) -> Result<Vec<ObjMeta>> {
        let filter = |all: &[ObjMeta]| all.iter().filter(|o| o.key.starts_with(prefix) && !(shallow && o.key[prefix.len()..].contains('/'))).cloned().collect::<Vec<_>>();
        if let Some((t, all)) = self.listing.lock().as_ref() { if t.elapsed() < std::time::Duration::from_secs(30) { return Ok(filter(all)); } }
        let all = self.list_all().await?;
        let out = filter(&all);
        *self.listing.lock() = Some((std::time::Instant::now(), all));
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        self.get_impl(key).await
    }

    async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes> {
        if len == 0 { return Ok(Bytes::new()); }
        self.get_range_impl(key, off, len).await
    }

    async fn exists(&self, key: &str) -> Result<bool> { Ok(!self.find(key).await?.is_empty()) }

    async fn put(&self, key: &str, body: Bytes, immutable: bool) -> Result<()> {
        *self.listing.lock() = None;
        self.put_impl(key, body, immutable).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        *self.listing.lock() = None;
        self.delete_impl(key).await
    }

    async fn check(&self) -> Result<serde_json::Value> { self.check_impl().await }
}

impl GDrive {
    /// Все файлы папки (постранично).
    async fn list_all(&self) -> Result<Vec<ObjMeta>> {
        let (prefix, shallow) = ("", false);
        let fid = self.folder().await?;
        let mut out = Vec::new();
        let mut page = String::new();
        let mut seen: HashMap<String, Vec<String>> = HashMap::new();
        loop {
            let mut q = vec![("q", format!("'{fid}' in parents and trashed = false")), ("fields", "nextPageToken,files(id,name,size,modifiedTime,mimeType)".into()), ("pageSize", "1000".into()), ("spaces", "drive".into())];
            if !page.is_empty() { q.push(("pageToken", page.clone())); }
            let r = self.call(Method::GET, &format!("{}/files", self.api), &q, None, &[], "Google Диск: список").await?;
            if !r.status().is_success() { return Err(fail(r, "Google Диск: список").await); }
            let v: Value = r.json().await?;
            for f in v.get("files").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                if f.get("mimeType").and_then(|x| x.as_str()) == Some(FOLDER) { continue; }
                let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let id = f.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let first = !seen.contains_key(&name);
                seen.entry(name.clone()).or_default().push(id);
                if !first || !name.starts_with(prefix) { continue; }
                if shallow && name[prefix.len()..].contains('/') { continue; }
                let size = f.get("size").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
                let lm = f.get("modifiedTime").and_then(|x| x.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now);
                out.push(ObjMeta { key: name, size, last_modified: lm });
            }
            page = v.get("nextPageToken").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if page.is_empty() { break; }
        }
        self.ids.lock().extend(seen);
        Ok(out)
    }

    async fn get_impl(&self, key: &str) -> Result<Option<Bytes>> {
        let Some(id) = self.find(key).await?.into_iter().next() else { return Ok(None) };
        let r = self.call(Method::GET, &format!("{}/files/{id}", self.api), &[("alt", "media".into())], None, &[], "Google Диск: чтение").await?;
        if r.status().as_u16() == 404 { self.ids.lock().remove(key); return Ok(None); }
        if !r.status().is_success() { return Err(fail(r, &format!("Google Диск: чтение {key}")).await); }
        Ok(Some(r.bytes().await?))
    }

    async fn get_range_impl(&self, key: &str, off: u64, len: u64) -> Result<Bytes> {
        let id = self.find(key).await?.into_iter().next().ok_or_else(|| anyhow!("Google Диск: нет файла {key}"))?;
        let r = self.call(Method::GET, &format!("{}/files/{id}", self.api), &[("alt", "media".into())], None, &[("Range", format!("bytes={}-{}", off, off + len - 1))], "Google Диск: чтение").await?;
        let st = r.status().as_u16();
        if !r.status().is_success() { return Err(fail(r, &format!("Google Диск: чтение {key}")).await); }
        take_range(st, r.bytes().await?, off, len)
    }

    async fn put_impl(&self, key: &str, body: Bytes, immutable: bool) -> Result<()> {
        let fid = self.folder().await?;
        // Служебный объект (блокировка) — обновить содержимое существующего файла.
        if !immutable {
            if let Some(id) = self.find(key).await?.into_iter().next() {
                let r = self.call(Method::PATCH, &format!("{}/files/{id}", self.upload), &[("uploadType", "media".into())], Some((body.clone(), "application/octet-stream")), &[], "Google Диск: запись").await?;
                if r.status().is_success() { return Ok(()); }
                if r.status().as_u16() != 404 { return Err(fail(r, &format!("Google Диск: запись {key}")).await); }
                self.ids.lock().remove(key);
            }
        }
        let meta = json!({ "name": key, "parents": [fid] }).to_string();
        let id = if body.len() <= 4 << 20 {
            let boundary = format!("hb{}", uuid::Uuid::new_v4().simple());
            let mut b = Vec::with_capacity(body.len() + 512);
            b.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
            b.extend_from_slice(&body);
            b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let ct = format!("multipart/related; boundary={boundary}");
            let r = self.call(Method::POST, &format!("{}/files", self.upload), &[("uploadType", "multipart".into()), ("fields", "id".into())], Some((Bytes::from(b), ct.as_str())), &[], "Google Диск: запись").await?;
            if !r.status().is_success() { return Err(fail(r, &format!("Google Диск: запись {key}")).await); }
            r.json::<Value>().await?.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string()
        } else {
            let r = self.call(Method::POST, &format!("{}/files", self.upload), &[("uploadType", "resumable".into()), ("fields", "id".into())], Some((Bytes::from(meta), "application/json; charset=UTF-8")), &[("X-Upload-Content-Length", body.len().to_string())], "Google Диск: начало загрузки").await?;
            if !r.status().is_success() { return Err(fail(r, &format!("Google Диск: запись {key}")).await); }
            let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| anyhow!("Google Диск: нет адреса загрузки"))?.to_string();
            let r = self.call(Method::PUT, &loc, &[], Some((body.clone(), "application/octet-stream")), &[], "Google Диск: загрузка").await?;
            if !r.status().is_success() { return Err(fail(r, &format!("Google Диск: запись {key}")).await); }
            r.json::<Value>().await.ok().and_then(|v| v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string())).unwrap_or_default()
        };
        if !id.is_empty() { self.ids.lock().insert(key.to_string(), vec![id]); }
        Ok(())
    }

    async fn delete_impl(&self, key: &str) -> Result<()> {
        for id in self.find(key).await? {
            let r = self.call(Method::DELETE, &format!("{}/files/{id}", self.api), &[], None, &[], "Google Диск: удаление").await?;
            if !(r.status().is_success() || r.status().as_u16() == 404) { return Err(fail(r, &format!("Google Диск: удаление {key}")).await); }
        }
        self.ids.lock().remove(key);
        Ok(())
    }

    async fn check_impl(&self) -> Result<serde_json::Value> {
        let r = self.call(Method::GET, &format!("{}/about", self.api), &[("fields", "user(emailAddress),storageQuota".into())], None, &[], "Google Диск").await?;
        if !r.status().is_success() { return Err(fail(r, "Google Диск").await); }
        let v: Value = r.json().await?;
        let fid = self.folder().await?;
        let n = |p: &str| v.pointer(p).and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok());
        let (total, used) = (n("/storageQuota/limit"), n("/storageQuota/usage"));
        Ok(json!({ "account": v.pointer("/user/emailAddress").and_then(|x| x.as_str()).unwrap_or(""), "folder_id": fid, "total": total, "used": used, "free": total.zip(used).map(|(t, u)| t.saturating_sub(u)) }))
    }
}
