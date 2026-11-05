//! OAuth 2.0 для облачных дисков: вход по коду устройства (администратор открывает страницу Google
//! или Яндекса, вводит код и разрешает доступ) и обновление токена доступа.
//!
//! * Google: приложение OAuth типа «TVs and Limited Input devices», права `drive.file`.
//! * Яндекс: приложение на oauth.yandex.ru с правами «Яндекс Диск REST API: доступ к папке приложения»
//!   (`cloud_api:disk.app_folder`) или ко всему диску.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

#[derive(Debug, Clone)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,
    pub expires_in: u64,
}

pub enum Poll {
    Pending,
    SlowDown,
    Done(Tokens),
}

pub struct Endpoints {
    pub device: String,
    pub token: String,
}

/// Адреса OAuth провайдера (`endpoint` — для имитатора в тестах).
pub fn endpoints(provider: &str, endpoint: &str) -> Result<Endpoints> {
    let ep = endpoint.trim().trim_end_matches('/');
    Ok(match (provider, ep.is_empty()) {
        ("gdrive", true) => Endpoints { device: "https://oauth2.googleapis.com/device/code".into(), token: "https://oauth2.googleapis.com/token".into() },
        ("yadisk", true) => Endpoints { device: "https://oauth.yandex.ru/device/code".into(), token: "https://oauth.yandex.ru/token".into() },
        ("gdrive" | "yadisk", false) => Endpoints { device: format!("{ep}/device/code"), token: format!("{ep}/token") },
        (p, _) => bail!("вход OAuth для «{p}» не поддерживается"),
    })
}

fn http() -> Result<reqwest::Client> { crate::tlsutil::client(None) }

fn err_text(v: &Value) -> String {
    let e = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
    let d = v.get("error_description").and_then(|x| x.as_str()).unwrap_or("");
    if d.is_empty() { e.to_string() } else { format!("{e}: {d}") }
}

pub async fn device_start(provider: &str, endpoint: &str, client_id: &str) -> Result<DeviceStart> {
    if client_id.trim().is_empty() { bail!("укажите Client ID приложения OAuth"); }
    let ep = endpoints(provider, endpoint)?;
    let mut form: Vec<(&str, String)> = vec![("client_id", client_id.trim().into())];
    if provider == "gdrive" { form.push(("scope", "https://www.googleapis.com/auth/drive.file".into())); }
    if provider == "yadisk" { form.push(("device_name", format!("HopeBackup {}", crate::util::hostname()))); }
    let r = http()?.post(&ep.device).form(&form).send().await?;
    let st = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !st.is_success() { bail!("вход OAuth: {}", err_text(&v)); }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let url = { let u = s("verification_url"); if u.is_empty() { s("verification_uri") } else { u } };
    Ok(DeviceStart {
        device_code: s("device_code"),
        user_code: s("user_code"),
        verification_url: url,
        interval: v.get("interval").and_then(|x| x.as_u64()).unwrap_or(5).max(2),
        expires_in: v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(600),
    })
}

pub async fn device_poll(provider: &str, endpoint: &str, client_id: &str, client_secret: &str, device_code: &str) -> Result<Poll> {
    let ep = endpoints(provider, endpoint)?;
    let mut form: Vec<(&str, String)> = vec![("client_id", client_id.trim().into()), ("client_secret", client_secret.trim().into())];
    match provider {
        "gdrive" => { form.push(("device_code", device_code.into())); form.push(("grant_type", "urn:ietf:params:oauth:grant-type:device_code".into())); }
        _ => { form.push(("code", device_code.into())); form.push(("grant_type", "device_code".into())); }
    }
    let r = http()?.post(&ep.token).form(&form).send().await?;
    let st = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if st.is_success() { return Ok(Poll::Done(serde_json::from_value(v)?)); }
    match v.get("error").and_then(|x| x.as_str()).unwrap_or("") {
        "authorization_pending" => Ok(Poll::Pending),
        "slow_down" => Ok(Poll::SlowDown),
        _ => Err(anyhow!("вход OAuth: {}", err_text(&v))),
    }
}

/// Новый токен доступа по токену обновления: (токены, срок действия — unix-время).
pub async fn refresh(provider: &str, token_url: &str, client_id: &str, client_secret: &str, refresh_token: &str) -> Result<(Tokens, i64)> {
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh_token.trim()), ("client_id", client_id.trim()), ("client_secret", client_secret.trim())];
    let r = super::objstore::send_retry(|| http().expect("http").post(token_url).form(&form), "обновление токена OAuth").await?;
    let st = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !st.is_success() {
        let e = err_text(&v);
        if e.contains("invalid_grant") { bail!("{provider}: доступ отозван или истёк — подключите облако заново ({e})"); }
        bail!("{provider}: обновление токена: {e}");
    }
    let t: Tokens = serde_json::from_value(v)?;
    let exp = chrono::Utc::now().timestamp() + if t.expires_in > 0 { t.expires_in } else { 3600 };
    Ok((t, exp))
}
