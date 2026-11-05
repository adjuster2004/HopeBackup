//! Клиент S3 (подмножество API, нужное площадке и плагину S3): объекты, диапазоны, листинг,
//! Object Lock, многочастная загрузка. Подпись — SigV4, повторы при сбоях сети и 5xx/SlowDown.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::time::Duration;

use super::sigv4::{self, Creds};
use super::{xml_blocks, xml_escape, xml_tag};

#[derive(Clone)]
pub struct S3 {
    http: reqwest::Client,
    scheme: String,
    /// Хост конечной точки (с портом, если он не стандартный).
    host: String,
    pub region: String,
    access_key: String,
    secret_key: String,
    pub bucket: String,
    path_style: bool,
}

#[derive(Clone, Debug)]
pub struct Obj {
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
}

#[derive(Clone, Debug, Default)]
pub struct Head {
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub last_modified: Option<DateTime<Utc>>,
    /// Пользовательские метаданные (`x-amz-meta-*`, без префикса).
    pub meta: Vec<(String, String)>,
}

/// Ошибка S3 с кодом (`NoSuchKey`, `AccessDenied`, …).
#[derive(Debug)]
pub struct S3Error {
    pub status: u16,
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S3 {}: {}{}", self.status, self.code, if self.message.is_empty() { String::new() } else { format!(" — {}", self.message) })
    }
}
impl std::error::Error for S3Error {}

pub fn md5_b64(b: &[u8]) -> String {
    use md5::Digest;
    base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(b))
}

impl S3 {
    /// `endpoint` — https://storage.yandexcloud.net, https://s3.eu-central-1.amazonaws.com, http://minio:9000.
    pub fn new(endpoint: &str, region: &str, bucket: &str, access_key: &str, secret_key: &str, path_style: bool, fingerprint: &str) -> Result<S3> {
        let ep = endpoint.trim().trim_end_matches('/');
        let (scheme, rest) = ep.split_once("://").ok_or_else(|| anyhow!("адрес S3 «{endpoint}»: нужен https://…"))?;
        let host = rest.split('/').next().unwrap_or(rest).to_string();
        if host.is_empty() { bail!("адрес S3 «{endpoint}»: нет хоста"); }
        let host = match host.rsplit_once(':') {
            Some((h, "443")) if scheme == "https" => h.to_string(),
            Some((h, "80")) if scheme == "http" => h.to_string(),
            _ => host,
        };
        // Хост-IP, бакет с точками (сертификат *.endpoint) — только адресация по пути.
        let ip_host = host.split(':').next().map(|h| h.parse::<std::net::IpAddr>().is_ok() || h == "localhost").unwrap_or(false);
        let fp = crate::tlsutil::normalize_fp(fingerprint);
        Ok(S3 {
            http: crate::tlsutil::client(if fp.is_empty() { None } else { Some(fingerprint) })?,
            scheme: scheme.to_string(),
            host,
            region: if region.trim().is_empty() { "us-east-1".into() } else { region.trim().into() },
            access_key: access_key.trim().into(),
            secret_key: secret_key.trim().into(),
            bucket: bucket.trim().into(),
            path_style: path_style || ip_host || bucket.contains('.'),
        })
    }

    fn target(&self, key: &str, bucket: bool) -> (String, String) {
        let k = sigv4::uri_encode(key, true);
        if !bucket {
            return (self.host.clone(), "/".into());
        }
        if self.path_style {
            (self.host.clone(), if key.is_empty() { format!("/{}", self.bucket) } else { format!("/{}/{k}", self.bucket) })
        } else {
            (format!("{}.{}", self.bucket, self.host), format!("/{k}"))
        }
    }

    async fn send_once(&self, method: &reqwest::Method, key: &str, bucket: bool, query: &[(String, String)], headers: &[(String, String)], body: &Bytes) -> Result<reqwest::Response> {
        let (host, path) = self.target(key, bucket);
        let payload = if body.is_empty() { sigv4::EMPTY_SHA256.to_string() } else { sigv4::sha256_hex(body) };
        let creds = Creds { access_key: &self.access_key, secret_key: &self.secret_key, region: &self.region, service: "s3" };
        let signed = sigv4::sign(&creds, method.as_str(), &host, &path, query, headers, &payload, Utc::now());
        let qs = query.iter().map(|(k, v)| format!("{}={}", sigv4::uri_encode(k, false), sigv4::uri_encode(v, false))).collect::<Vec<_>>().join("&");
        let url = format!("{}://{host}{path}{}", self.scheme, if qs.is_empty() { String::new() } else { format!("?{qs}") });
        let mut r = self.http.request(method.clone(), url).timeout(Duration::from_secs(if body.len() > 1 << 20 { 3600 } else { 120 }));
        for (k, v) in signed { r = r.header(k, v); }
        if !body.is_empty() { r = r.body(body.clone()); }
        Ok(r.send().await?)
    }

    /// Запрос с повторами при сбоях сети, 5xx и SlowDown.
    pub async fn send(&self, method: reqwest::Method, key: &str, query: Vec<(String, String)>, headers: Vec<(String, String)>, body: Bytes) -> Result<reqwest::Response> {
        self.send_ex(method, key, true, query, headers, body).await
    }

    async fn send_ex(&self, method: reqwest::Method, key: &str, bucket: bool, query: Vec<(String, String)>, headers: Vec<(String, String)>, body: Bytes) -> Result<reqwest::Response> {
        let mut delay = 1u64;
        for attempt in 1..=6 {
            match self.send_once(&method, key, bucket, &query, &headers, &body).await {
                Ok(r) if (r.status().is_server_error() || r.status().as_u16() == 429) && attempt < 6 => {
                    tracing::warn!("S3 {method} {key}: {} — повтор через {delay} с", r.status());
                }
                Ok(r) => return Ok(r),
                Err(e) if attempt < 6 => {
                    let transient = e.downcast_ref::<reqwest::Error>().map(|x| x.is_connect() || x.is_timeout() || x.is_request() || x.is_body()).unwrap_or(false);
                    if !transient { return Err(e); }
                    tracing::warn!("S3 {method} {key}: {e} — повтор через {delay} с");
                }
                Err(e) => return Err(e),
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
        }
        unreachable!()
    }

    async fn err(r: reqwest::Response) -> anyhow::Error {
        let status = r.status().as_u16();
        let body = r.text().await.unwrap_or_default();
        let code = xml_tag(&body, "Code").unwrap_or_else(|| match status { 403 => "AccessDenied".into(), 404 => "NoSuchKey".into(), _ => format!("HTTP{status}") });
        S3Error { status, code, message: xml_tag(&body, "Message").unwrap_or_default() }.into()
    }

    async fn ok(r: reqwest::Response) -> Result<reqwest::Response> { if r.status().is_success() { Ok(r) } else { Err(Self::err(r).await) } }

    pub async fn head_bucket(&self) -> Result<()> {
        let r = self.send(reqwest::Method::HEAD, "", vec![], vec![], Bytes::new()).await?;
        match r.status().as_u16() {
            200 => Ok(()),
            404 => bail!("бакет «{}» не найден", self.bucket),
            403 => bail!("нет доступа к бакету «{}» (проверьте ключи и права)", self.bucket),
            s => bail!("бакет «{}»: HTTP {s}", self.bucket),
        }
    }

    /// Все объекты с префиксом (постранично). С `delimiter` — только верхний уровень + общие префиксы.
    pub async fn list(&self, prefix: &str, delimiter: Option<&str>) -> Result<(Vec<Obj>, Vec<String>)> {
        let mut out = Vec::new();
        let mut prefixes = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type".to_string(), "2".to_string()), ("prefix".to_string(), prefix.to_string()), ("max-keys".to_string(), "1000".to_string())];
            if let Some(d) = delimiter { q.push(("delimiter".into(), d.into())); }
            if let Some(t) = &token { q.push(("continuation-token".into(), t.clone())); }
            let r = Self::ok(self.send(reqwest::Method::GET, "", q, vec![], Bytes::new()).await?).await?;
            let x = r.text().await?;
            for c in xml_blocks(&x, "Contents") {
                out.push(Obj {
                    key: xml_tag(c, "Key").unwrap_or_default(),
                    size: xml_tag(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    last_modified: xml_tag(c, "LastModified").and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now),
                    etag: xml_tag(c, "ETag").unwrap_or_default().trim_matches('"').to_string(),
                });
            }
            for p in xml_blocks(&x, "CommonPrefixes") { if let Some(pp) = xml_tag(p, "Prefix") { prefixes.push(pp); } }
            if xml_tag(&x, "IsTruncated").as_deref() == Some("true") {
                token = xml_tag(&x, "NextContinuationToken");
                if token.is_none() { break; }
            } else {
                break;
            }
        }
        Ok((out, prefixes))
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let r = self.send(reqwest::Method::GET, key, vec![], vec![], Bytes::new()).await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        Ok(Some(Self::ok(r).await?.bytes().await?))
    }

    pub async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes> {
        if len == 0 { return Ok(Bytes::new()); }
        let r = self.send(reqwest::Method::GET, key, vec![], vec![("range".into(), format!("bytes={off}-{}", off + len - 1))], Bytes::new()).await?;
        let b = Self::ok(r).await?.bytes().await?;
        if b.len() as u64 != len { bail!("S3 {key}: прочитано {} байт из {len}", b.len()); }
        Ok(b)
    }

    /// Ответ GET для потокового чтения (плагин S3: большие объекты).
    pub async fn get_resp(&self, key: &str) -> Result<reqwest::Response> {
        Self::ok(self.send(reqwest::Method::GET, key, vec![], vec![], Bytes::new()).await?).await
    }

    pub async fn head(&self, key: &str) -> Result<Option<Head>> {
        let r = self.send(reqwest::Method::HEAD, key, vec![], vec![], Bytes::new()).await?;
        if r.status().as_u16() == 404 { return Ok(None); }
        let r = Self::ok(r).await?;
        Ok(Some(head_of(r.headers())))
    }

    pub async fn put(&self, key: &str, body: Bytes, mut headers: Vec<(String, String)>) -> Result<()> {
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-md5")) { headers.push(("content-md5".into(), md5_b64(&body))); }
        Self::ok(self.send(reqwest::Method::PUT, key, vec![], headers, body).await?).await?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let r = self.send(reqwest::Method::DELETE, key, vec![], vec![], Bytes::new()).await?;
        if r.status().as_u16() == 404 { return Ok(()); }
        Self::ok(r).await?;
        Ok(())
    }

    pub async fn list_buckets(&self) -> Result<Vec<String>> {
        let r = Self::ok(self.send_ex(reqwest::Method::GET, "", false, vec![], vec![], Bytes::new()).await?).await?;
        let x = r.text().await?;
        Ok(xml_blocks(&x, "Bucket").iter().filter_map(|b| xml_tag(b, "Name")).collect())
    }

    pub async fn create_bucket(&self) -> Result<()> {
        let body = if self.region == "us-east-1" { Bytes::new() } else {
            Bytes::from(format!("<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>", xml_escape(&self.region)))
        };
        let r = self.send(reqwest::Method::PUT, "", vec![], vec![], body).await?;
        if r.status().as_u16() == 409 { return Ok(()); }
        Self::ok(r).await?;
        Ok(())
    }

    // ------------------------------------------------------------ многочастная загрузка

    pub async fn multipart_create(&self, key: &str, headers: Vec<(String, String)>) -> Result<String> {
        let r = Self::ok(self.send(reqwest::Method::POST, key, vec![("uploads".into(), String::new())], headers, Bytes::new()).await?).await?;
        let x = r.text().await?;
        xml_tag(&x, "UploadId").ok_or_else(|| anyhow!("S3: нет UploadId в ответе"))
    }

    pub async fn multipart_part(&self, key: &str, upload: &str, n: u32, body: Bytes) -> Result<String> {
        let md5 = md5_b64(&body);
        let r = Self::ok(self.send(reqwest::Method::PUT, key, vec![("partNumber".into(), n.to_string()), ("uploadId".into(), upload.into())], vec![("content-md5".into(), md5)], body).await?).await?;
        Ok(r.headers().get("etag").and_then(|v| v.to_str().ok()).unwrap_or("").to_string())
    }

    pub async fn multipart_complete(&self, key: &str, upload: &str, parts: &[(u32, String)]) -> Result<()> {
        let mut x = String::from("<CompleteMultipartUpload>");
        for (n, e) in parts { x += &format!("<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>", xml_escape(e)); }
        x += "</CompleteMultipartUpload>";
        let r = Self::ok(self.send(reqwest::Method::POST, key, vec![("uploadId".into(), upload.into())], vec![("content-type".into(), "application/xml".into())], Bytes::from(x)).await?).await?;
        // Ошибка может прийти в теле ответа 200.
        let t = r.text().await.unwrap_or_default();
        if t.contains("<Error>") { bail!("S3: многочастная загрузка {key}: {}", xml_tag(&t, "Message").unwrap_or(t)); }
        Ok(())
    }

    pub async fn multipart_abort(&self, key: &str, upload: &str) {
        let _ = self.send(reqwest::Method::DELETE, key, vec![("uploadId".into(), upload.into())], vec![], Bytes::new()).await;
    }

    /// Положить объект любого размера из потока частей (≥ 5 МиБ каждая, кроме последней).
    pub async fn put_large(&self, key: &str, first: Bytes, rest: &mut (dyn FnMut() -> Option<Result<Bytes>> + Send), headers: Vec<(String, String)>) -> Result<()> {
        let id = self.multipart_create(key, headers).await?;
        let r: Result<()> = async {
            let mut parts = Vec::new();
            let mut n = 1u32;
            parts.push((n, self.multipart_part(key, &id, n, first).await?));
            while let Some(b) = rest() {
                n += 1;
                parts.push((n, self.multipart_part(key, &id, n, b?).await?));
            }
            self.multipart_complete(key, &id, &parts).await
        }
        .await;
        if r.is_err() { self.multipart_abort(key, &id).await; }
        r.context(format!("загрузка {key}"))
    }
}

pub fn head_of(h: &reqwest::header::HeaderMap) -> Head {
    let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    Head {
        size: g("content-length").parse().unwrap_or(0),
        etag: g("etag").trim_matches('"').to_string(),
        content_type: g("content-type"),
        last_modified: DateTime::parse_from_rfc2822(&g("last-modified")).ok().map(|d| d.with_timezone(&Utc)),
        meta: h.iter().filter_map(|(k, v)| k.as_str().strip_prefix("x-amz-meta-").map(|n| (n.to_string(), v.to_str().unwrap_or("").to_string()))).collect(),
    }
}

/// Заголовки Object Lock: объект нельзя удалить или перезаписать до даты (WORM).
pub fn lock_headers(mode: &str, days: u32) -> Vec<(String, String)> {
    let m = mode.trim().to_uppercase();
    if days == 0 || (m != "GOVERNANCE" && m != "COMPLIANCE") { return vec![]; }
    let until = (Utc::now() + chrono::Duration::days(days as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    vec![("x-amz-object-lock-mode".into(), m), ("x-amz-object-lock-retain-until-date".into(), until)]
}
