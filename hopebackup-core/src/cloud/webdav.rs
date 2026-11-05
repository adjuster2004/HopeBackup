//! WebDAV: Nextcloud (`https://облако/remote.php/dav/files/<логин>`), ownCloud, Synology, любой
//! сервер WebDAV. Вход — логин и пароль приложения (Basic по TLS).

use anyhow::{bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use reqwest::Method;
use std::collections::HashSet;

use super::objstore::{fail, send_retry, take_range, ObjMeta, ObjStore};

pub struct WebDav {
    http: reqwest::Client,
    /// Адрес корня (с папкой площадки), без `/` в конце.
    base: String,
    /// Путь корня в URL (для разбора href), без `/` в конце.
    base_path: String,
    /// Корень WebDAV (без папки площадки) и сама папка — для создания каталогов по сегментам.
    dav_root: String,
    folder: String,
    user: String,
    pass: String,
    made: parking_lot::Mutex<HashSet<String>>,
}

/// Кодирование сегментов пути для URL.
pub fn enc_path(p: &str) -> String {
    p.split('/').map(|s| {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>()
    }).collect::<Vec<_>>().join("/")
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) { out.push(h * 16 + l); i += 3; continue; }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Блоки элемента XML по локальному имени (без учёта префикса пространства имён: d:, D:, ns0:).
fn blocks<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = xml[i..].find('<') {
        let s = i + off;
        let rest = &xml[s + 1..];
        let end = rest.find(|c: char| c == '>' || c.is_whitespace() || c == '/').unwrap_or(rest.len());
        let name = &rest[..end];
        let lname = name.rsplit(':').next().unwrap_or(name);
        if !name.starts_with('/') && lname == local {
            let open_end = match rest.find('>') { Some(e) => s + 1 + e + 1, None => break };
            if xml[..open_end].ends_with("/>") { out.push(""); i = open_end; continue; }
            let close = format!("</{name}>");
            match xml[open_end..].find(&close) {
                Some(c) => { out.push(&xml[open_end..open_end + c]); i = open_end + c + close.len(); }
                None => break,
            }
        } else {
            i = s + 1;
        }
    }
    out
}

fn tag(xml: &str, local: &str) -> Option<String> { blocks(xml, local).into_iter().next().map(|s| super::xml_unescape(s.trim())) }

impl WebDav {
    pub fn new(url: &str, folder: &str, user: &str, pass: &str, fingerprint: &str) -> Result<WebDav> {
        let url = url.trim().trim_end_matches('/');
        if !url.starts_with("https://") && !url.starts_with("http://") { bail!("адрес WebDAV: https://…"); }
        let folder = folder.trim().trim_matches('/');
        let base = if folder.is_empty() { url.to_string() } else { format!("{url}/{}", enc_path(folder)) };
        let after = base.split_once("://").map(|x| x.1).unwrap_or(&base);
        let base_path = after.find('/').map(|i| pct_decode(&after[i..])).unwrap_or_default().trim_end_matches('/').to_string();
        let fp = crate::tlsutil::normalize_fp(fingerprint);
        Ok(WebDav {
            http: crate::tlsutil::client(if fp.is_empty() { None } else { Some(fingerprint) })?,
            base,
            base_path,
            dav_root: url.to_string(),
            folder: folder.to_string(),
            user: user.trim().into(),
            pass: pass.into(),
            made: parking_lot::Mutex::new(HashSet::new()),
        })
    }

    fn url(&self, key: &str) -> String { if key.is_empty() { format!("{}/", self.base) } else { format!("{}/{}", self.base, enc_path(key)) } }

    fn req(&self, m: Method, key: &str) -> reqwest::RequestBuilder {
        self.http.request(m, self.url(key)).basic_auth(&self.user, Some(&self.pass)).timeout(std::time::Duration::from_secs(600))
    }

    async fn propfind(&self, key: &str, depth: &str) -> Result<Option<String>> {
        let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;
        let dir = if key.is_empty() || key.ends_with('/') { key.to_string() } else { format!("{key}/") };
        let r = send_retry(|| self.req(Method::from_bytes(b"PROPFIND").unwrap(), &dir).header("Depth", depth).header("Content-Type", "application/xml").body(body), "WebDAV PROPFIND").await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        if r.status().as_u16() != 207 && !r.status().is_success() { return Err(fail(r, "WebDAV PROPFIND").await); }
        Ok(Some(r.text().await?))
    }

    /// Создать каталоги (папка площадки и `dir` внутри неё) по сегментам от корня WebDAV
    /// (MKCOL; 405 — уже есть).
    async fn mkdirs(&self, dir: &str) -> Result<()> {
        let full = format!("{}/{}", self.folder, dir.trim_matches('/'));
        let mut acc = String::new();
        let mut parts: Vec<String> = Vec::new();
        for seg in full.split('/').filter(|s| !s.is_empty()) {
            if !acc.is_empty() { acc.push('/'); }
            acc.push_str(seg);
            parts.push(acc.clone());
        }
        for p in parts {
            if self.made.lock().contains(&p) { continue; }
            let url = format!("{}/{}/", self.dav_root, enc_path(&p));
            let r = send_retry(|| self.http.request(Method::from_bytes(b"MKCOL").unwrap(), &url).basic_auth(&self.user, Some(&self.pass)), "WebDAV MKCOL").await?;
            let st = r.status().as_u16();
            if !(r.status().is_success() || st == 405 || st == 301) { return Err(fail(r, &format!("WebDAV: создание каталога {p}")).await); }
            self.made.lock().insert(p);
        }
        Ok(())
    }
}

#[async_trait]
impl ObjStore for WebDav {
    fn id(&self) -> String { format!("webdav|{}|{}", self.base, self.user) }

    async fn list(&self, prefix: &str, shallow: bool) -> Result<Vec<ObjMeta>> {
        let mut out = Vec::new();
        let mut todo = vec![prefix.trim_start_matches('/').to_string()];
        while let Some(dir) = todo.pop() {
            let Some(xml) = self.propfind(&dir, "1").await? else { continue };
            for resp in blocks(&xml, "response") {
                let Some(href) = tag(resp, "href") else { continue };
                let path = if href.starts_with("http") { href.split_once("://").and_then(|x| x.1.find('/').map(|i| x.1[i..].to_string())).unwrap_or_default() } else { href.clone() };
                let path = pct_decode(&path);
                let Some(rel) = path.strip_prefix(&self.base_path) else { continue };
                let rel = rel.trim_start_matches('/');
                let is_dir = blocks(resp, "collection").len() > 0 || rel.ends_with('/');
                let rel = rel.trim_end_matches('/').to_string();
                if rel.is_empty() || format!("{rel}/") == dir || rel == dir.trim_end_matches('/') { continue; }
                if is_dir {
                    if !shallow { todo.push(format!("{rel}/")); }
                    continue;
                }
                let size = tag(resp, "getcontentlength").and_then(|v| v.parse().ok()).unwrap_or(0);
                let lm = tag(resp, "getlastmodified").and_then(|v| DateTime::parse_from_rfc2822(&v).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now);
                out.push(ObjMeta { key: rel, size, last_modified: lm });
            }
        }
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let r = send_retry(|| self.req(Method::GET, key), "WebDAV GET").await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        if !r.status().is_success() { return Err(fail(r, &format!("WebDAV: чтение {key}")).await); }
        Ok(Some(r.bytes().await?))
    }

    async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes> {
        if len == 0 { return Ok(Bytes::new()); }
        let range = format!("bytes={}-{}", off, off + len - 1);
        let r = send_retry(|| self.req(Method::GET, key).header("Range", range.clone()), "WebDAV GET").await?;
        let st = r.status().as_u16();
        if !r.status().is_success() { return Err(fail(r, &format!("WebDAV: чтение {key}")).await); }
        take_range(st, r.bytes().await?, off, len)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let r = send_retry(|| self.req(Method::HEAD, key), "WebDAV HEAD").await?;
        match r.status().as_u16() {
            404 => Ok(false),
            s if (200..300).contains(&s) => Ok(true),
            _ => Err(fail(r, &format!("WebDAV: {key}")).await),
        }
    }

    async fn put(&self, key: &str, body: Bytes, _immutable: bool) -> Result<()> {
        if let Some((dir, _)) = key.rsplit_once('/') { self.mkdirs(dir).await?; } else { self.mkdirs("").await?; }
        let r = send_retry(|| self.req(Method::PUT, key).body(body.clone()), "WebDAV PUT").await?;
        if !r.status().is_success() { return Err(fail(r, &format!("WebDAV: запись {key}")).await); }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let r = send_retry(|| self.req(Method::DELETE, key), "WebDAV DELETE").await?;
        if r.status().as_u16() == 404 || r.status().is_success() { return Ok(()); }
        Err(fail(r, &format!("WebDAV: удаление {key}")).await)
    }

    async fn check(&self) -> Result<serde_json::Value> {
        self.mkdirs("").await?;
        let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:quota-available-bytes/><d:quota-used-bytes/></d:prop></d:propfind>"#;
        let r = send_retry(|| self.req(Method::from_bytes(b"PROPFIND").unwrap(), "").header("Depth", "0").header("Content-Type", "application/xml").body(body), "WebDAV").await?;
        if r.status().as_u16() != 207 && !r.status().is_success() { return Err(fail(r, "WebDAV").await); }
        let x = r.text().await?;
        let free = tag(&x, "quota-available-bytes").and_then(|v| v.parse::<i64>().ok()).filter(|v| *v >= 0);
        let used = tag(&x, "quota-used-bytes").and_then(|v| v.parse::<i64>().ok());
        Ok(serde_json::json!({ "account": self.user, "free": free, "used": used }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml() {
        let x = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/u/HB/main/a%20b.json</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>12</d:getcontentlength></d:prop></d:propstat></d:response><D:response><D:href>/x/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response></d:multistatus>"#;
        let r = blocks(x, "response");
        assert_eq!(r.len(), 2);
        assert_eq!(tag(r[0], "getcontentlength").as_deref(), Some("12"));
        assert_eq!(blocks(r[1], "collection").len(), 1);
        assert_eq!(pct_decode("/a%20b%D0%AF"), "/a bЯ");
        assert_eq!(enc_path("main/a b.json"), "main/a%20b.json");
    }
}
