//! Общий интерфейс «хранилища объектов по ключу»: S3, WebDAV (Nextcloud, ownCloud), Яндекс Диск,
//! Google Диск. Репозиторий площадки (`store::s3::ObjBackend`) работает поверх любого из них.

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ObjMeta {
    /// Ключ относительно корня хранилища (`main/packs/ab/….pack`).
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

#[async_trait]
pub trait ObjStore: Send + Sync {
    /// Уникальный признак хранилища (для каталога кэша).
    fn id(&self) -> String;
    /// Объекты под префиксом (префикс оканчивается на `/`). `shallow` — только непосредственно в нём.
    async fn list(&self, prefix: &str, shallow: bool) -> Result<Vec<ObjMeta>>;
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes>;
    async fn exists(&self, key: &str) -> Result<bool>;
    /// `immutable` — данные копии (S3: с Object Lock); иначе служебный объект, который перезаписывается.
    async fn put(&self, key: &str, body: Bytes, immutable: bool) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
    /// Доступ есть (и что известно об учётной записи и месте).
    async fn check(&self) -> Result<serde_json::Value>;
}

/// Повтор запроса при сбоях сети, 429 и 5xx (облачные диски ограничивают частоту запросов).
pub async fn send_retry(mk: impl Fn() -> reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
    let mut delay = Duration::from_millis(500);
    let mut last;
    let started = std::time::Instant::now();
    let patience = crate::util::net_patience();
    // Сеть: ждём до 10 минут; 429/5xx — тоже, с паузой из Retry-After.
    loop {
        match mk().send().await {
            // 507 (нет места) и 501 повтор не исправит.
            Ok(r) if r.status().as_u16() == 429 || (r.status().is_server_error() && r.status().as_u16() != 507 && r.status().as_u16() != 501) => {
                let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
                last = format!("{} {}", r.status(), r.text().await.unwrap_or_default().chars().take(300).collect::<String>());
                if let Some(s) = retry_after { delay = Duration::from_secs(s.min(60)); }
            }
            Ok(r) => return Ok(r),
            Err(e) => last = format!("{e}"),
        }
        if started.elapsed() + delay > patience { break; }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
    anyhow::bail!("{what}: облачная площадка недоступна дольше {} с: {last}", patience.as_secs())
}

/// Ошибка ответа с текстом сервера.
pub async fn fail(r: reqwest::Response, what: &str) -> anyhow::Error {
    let st = r.status();
    let body = r.text().await.unwrap_or_default();
    let msg: String = body.chars().take(400).collect();
    match st.as_u16() {
        401 => anyhow::anyhow!("{what}: нет доступа (401) — проверьте логин, пароль или токен{}", if msg.is_empty() { String::new() } else { format!(": {msg}") }),
        403 => anyhow::anyhow!("{what}: запрещено (403){}", if msg.is_empty() { String::new() } else { format!(": {msg}") }),
        507 => anyhow::anyhow!("{what}: в облаке закончилось место (507)"),
        _ => anyhow::anyhow!("{what}: {st} {msg}"),
    }
}

/// Диапазон из ответа: сервер мог проигнорировать Range и прислать файл целиком.
pub fn take_range(st: u16, b: Bytes, off: u64, len: u64) -> Result<Bytes> {
    if st == 206 { return Ok(b); }
    let (s, e) = (off as usize, (off + len) as usize);
    if e > b.len() { anyhow::bail!("ответ короче запрошенного диапазона"); }
    Ok(b.slice(s..e))
}

// ------------------------------------------------------------------ S3

pub struct S3Store {
    pub c: super::s3::S3,
    pub worm: Vec<(String, String)>,
    pub tag: String,
}

#[async_trait]
impl ObjStore for S3Store {
    fn id(&self) -> String { self.tag.clone() }
    async fn list(&self, prefix: &str, shallow: bool) -> Result<Vec<ObjMeta>> {
        let (v, _) = self.c.list(prefix, if shallow { Some("/") } else { None }).await?;
        Ok(v.into_iter().map(|o| ObjMeta { key: o.key, size: o.size, last_modified: o.last_modified }).collect())
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> { self.c.get(key).await }
    async fn get_range(&self, key: &str, off: u64, len: u64) -> Result<Bytes> { self.c.get_range(key, off, len).await }
    async fn exists(&self, key: &str) -> Result<bool> { Ok(self.c.head(key).await?.is_some()) }
    async fn put(&self, key: &str, body: Bytes, immutable: bool) -> Result<()> { self.c.put(key, body, if immutable { self.worm.clone() } else { vec![] }).await }
    async fn delete(&self, key: &str) -> Result<()> { self.c.delete(key).await }
    async fn check(&self) -> Result<serde_json::Value> { self.c.head_bucket().await?; Ok(serde_json::json!({})) }
}
