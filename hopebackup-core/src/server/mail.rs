//! Исходящая почта (SMTP) и раздел «Почта и уведомления» (как в HopeDFS): уведомления администраторам
//! и ссылки сброса пароля к панели.
//!
//! Сервер почты организации (Exchange, Postfix, Яндекс 360…) задаётся в панели. Шифрование: STARTTLS (587),
//! TLS (465) или без шифрования (25, только для доверенного ретранслятора). Сертификат сервера проверяется по
//! корням Mozilla, системному хранилищу ЦС и, при необходимости, по сертификату ЦС из настроек; проверку
//! можно отключить.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use lettre::message::{header::ContentType, Mailbox};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::api::{bad, ApiError, R};
use super::auth::Who;
use super::Server;

type S = State<Arc<Server>>;

/// Настройки сервера исходящей почты.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MailSettings {
    #[serde(default)]
    pub host: String,
    /// 0 — порт по умолчанию для способа шифрования.
    #[serde(default)]
    pub port: u16,
    /// starttls | tls | none
    #[serde(default)]
    pub security: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub from_name: String,
    /// Адрес панели для ссылок в письмах (пусто — https://<IP сервера>:<порт>).
    #[serde(default)]
    pub panel_url: String,
    #[serde(default)]
    pub ca_pem: String,
    #[serde(default)]
    pub insecure_tls: bool,
}

/// Корректный адрес электронной почты.
pub fn valid_address(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.len() <= 254 && s.parse::<lettre::Address>().is_ok()
}

/// Заданы ли параметры, без которых письмо не отправить.
pub fn configured(m: &MailSettings) -> bool { !m.host.trim().is_empty() && valid_address(&m.from) }

/// Порт по умолчанию для способа шифрования.
pub fn default_port(security: &str) -> u16 { match security { "tls" => 465, "none" => 25, _ => 587 } }

/// https://<IP сервера>:<порт панели> — когда адрес панели в настройках не задан.
pub fn default_panel_url(s: &Server) -> String {
    let host = crate::util::local_ips().into_iter().next().unwrap_or_else(crate::util::hostname);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("https://{host}:{}", s.listen_port)
}

/// Адрес панели для ссылок в письмах.
pub fn panel_url(s: &Server) -> String {
    let u = s.store.read(|st| st.mail.panel_url.clone());
    let u = u.trim().trim_end_matches('/');
    if !u.is_empty() { return u.to_string(); }
    default_panel_url(s)
}

/// Подпись письма: сервер и адрес панели.
pub fn footer(s: &Server) -> String {
    format!("\n\n—\nHopeBackup (Ковчег), сервер управления {}\nПанель управления: {}\nЭто письмо отправлено автоматически, отвечать на него не нужно.", crate::util::hostname(), panel_url(s))
}

fn system_ca_bundle() -> Option<Vec<u8>> {
    for p in ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/ca-bundle.pem", "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", "/etc/ssl/cert.pem"] {
        if let Ok(b) = std::fs::read(p) { return Some(b); }
    }
    None
}

fn tls_params(m: &MailSettings) -> Result<TlsParameters, String> {
    let mut b = TlsParameters::builder(m.host.trim().to_string());
    if let Some(pem) = system_ca_bundle() {
        if let Ok(c) = Certificate::from_pem(&pem) { b = b.add_root_certificate(c); }
    }
    if !m.ca_pem.trim().is_empty() {
        let c = Certificate::from_pem(m.ca_pem.trim().as_bytes()).map_err(|e| format!("сертификат ЦС в настройках почты не разобран: {e}"))?;
        b = b.add_root_certificate(c);
    }
    if m.insecure_tls { b = b.dangerous_accept_invalid_certs(true).dangerous_accept_invalid_hostnames(true); }
    b.build_rustls().map_err(|e| e.to_string())
}

fn mailbox(addr: &str, name: &str) -> Result<Mailbox, String> {
    let a: lettre::Address = addr.trim().parse().map_err(|_| format!("неверный адрес {addr}"))?;
    Ok(Mailbox::new(if name.trim().is_empty() { None } else { Some(name.trim().to_string()) }, a))
}

/// Понятный текст ошибки SMTP.
fn explain(e: &lettre::transport::smtp::Error) -> String {
    let s = e.to_string();
    if e.is_timeout() { return format!("сервер почты не ответил вовремя ({s})"); }
    if let Some(code) = e.status() {
        let c = code.to_string();
        if c.starts_with("535") || c.starts_with("534") || c.starts_with("530") { return format!("сервер почты отклонил вход ({c}): {s}"); }
        return format!("сервер почты ответил {c}: {s}");
    }
    if s.contains("No compatible authentication mechanism") { return "сервер почты не предлагает вход (AUTH) на этом подключении — проверьте способ шифрования и порт или оставьте пользователя пустым, если вход не нужен".into(); }
    if s.contains("certificate") || s.contains("Certificate") || s.contains("UnknownIssuer") { return format!("сертификат сервера почты не прошёл проверку: {s}"); }
    if s.contains("Connection refused") { return format!("сервер почты не принимает подключения ({s})"); }
    s
}

/// Отправить текстовое письмо. Ошибка — понятным текстом.
pub async fn send_with(m: &MailSettings, to: &[String], subject: &str, body: &str) -> Result<(), String> {
    if m.host.trim().is_empty() { return Err("не задан сервер исходящей почты".into()); }
    if !valid_address(&m.from) { return Err("не задан адрес отправителя".into()); }
    let to: Vec<&String> = to.iter().filter(|t| !t.trim().is_empty()).collect();
    if to.is_empty() { return Err("нет получателей".into()); }
    let mut mb = Message::builder().from(mailbox(&m.from, if m.from_name.is_empty() { "HopeBackup" } else { &m.from_name })?).subject(subject).header(ContentType::TEXT_PLAIN);
    for t in &to { mb = mb.to(mailbox(t, "")?); }
    let msg = mb.body(body.to_string()).map_err(|e| e.to_string())?;
    let security = if m.security.is_empty() { "starttls" } else { m.security.as_str() };
    let port = if m.port == 0 { default_port(security) } else { m.port };
    let tls = match security {
        "none" => Tls::None,
        "tls" => Tls::Wrapper(tls_params(m)?),
        _ => Tls::Required(tls_params(m)?),
    };
    let mut tb = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(m.host.trim()).port(port).tls(tls).timeout(Some(Duration::from_secs(20)));
    if !m.username.trim().is_empty() { tb = tb.credentials(Credentials::new(m.username.trim().to_string(), m.password.clone())); }
    let tr = tb.build();
    match tokio::time::timeout(Duration::from_secs(45), tr.send(msg)).await {
        Ok(r) => r.map(|_| ()).map_err(|e| explain(&e)),
        Err(_) => Err(format!("сервер почты не ответил вовремя ({}:{port})", m.host.trim())),
    }
}

// ---------------------------------------------------------------- API раздела

fn mail_json(m: &MailSettings, default_url: &str) -> Value {
    json!({ "host": m.host, "port": m.port, "security": if m.security.is_empty() { "starttls" } else { &m.security }, "username": m.username,
        "has_password": !m.password.is_empty(), "from": m.from, "from_name": m.from_name, "panel_url": m.panel_url, "panel_url_default": default_url,
        "ca_pem": m.ca_pem, "insecure_tls": m.insecure_tls })
}

pub async fn get(State(s): S) -> Json<Value> {
    let (mail, notify) = s.store.read(|st| (st.mail.clone(), st.notify.clone()));
    let catalog: Vec<Value> = super::notify::EVENTS.iter().map(|e| json!({ "id": e.id, "group": e.group, "title": e.title, "text": e.text })).collect();
    Json(json!({ "mail": mail_json(&mail, &default_panel_url(&s)), "configured": configured(&mail), "notify": notify, "catalog": catalog, "log": super::notify::log(&s) }))
}

#[derive(Deserialize, Default, Clone)]
pub struct SmtpReq {
    #[serde(default)] host: String,
    #[serde(default)] port: u16,
    #[serde(default)] security: String,
    #[serde(default)] username: String,
    /// Пусто — оставить сохранённый пароль.
    #[serde(default)] password: String,
    #[serde(default)] clear_password: bool,
    #[serde(default)] from: String,
    #[serde(default)] from_name: String,
    #[serde(default)] panel_url: String,
    #[serde(default)] ca_pem: String,
    #[serde(default)] insecure_tls: bool,
}

/// Собрать настройки из формы (пароль — сохранённый, если в форме пусто).
fn build(r: &SmtpReq, saved: &MailSettings) -> Result<MailSettings, String> {
    let host = r.host.trim().to_string();
    if host.contains(['/', ' ', ':']) { return Err("сервер почты: укажите имя или IP-адрес без порта и схемы".into()); }
    let security = match r.security.as_str() { "tls" | "none" | "starttls" => r.security.clone(), "" => "starttls".into(), _ => return Err("неизвестный способ шифрования".into()) };
    let from = r.from.trim().to_string();
    if !from.is_empty() && !valid_address(&from) { return Err("неверный адрес отправителя".into()); }
    let url = r.panel_url.trim().trim_end_matches('/').to_string();
    if !url.is_empty() && !(url.starts_with("https://") || url.starts_with("http://")) { return Err("адрес панели должен начинаться с https://".into()); }
    if !r.ca_pem.trim().is_empty() && !r.ca_pem.contains("-----BEGIN CERTIFICATE-----") { return Err("сертификат ЦС — в формате PEM (-----BEGIN CERTIFICATE-----)".into()); }
    let password = if r.clear_password { String::new() } else if r.password.is_empty() { saved.password.clone() } else { r.password.clone() };
    Ok(MailSettings { host, port: r.port, security, username: r.username.trim().to_string(), password, from, from_name: r.from_name.trim().to_string(),
        panel_url: url, ca_pem: r.ca_pem.trim().to_string(), insecure_tls: r.insecure_tls })
}

pub async fn put_smtp(State(s): S, Extension(w): Extension<Who>, Json(r): Json<SmtpReq>) -> R<Json<Value>> {
    let saved = s.store.read(|st| st.mail.clone());
    let m = build(&r, &saved).map_err(bad)?;
    let desc = format!("сервер почты {}:{} ({}), отправитель {}", m.host, if m.port == 0 { default_port(&m.security) } else { m.port }, m.security, m.from);
    s.store.update(|st| { st.mail = m.clone(); Ok(()) })?;
    s.audit(&w.user, "mail", desc);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct NotifyReq {
    #[serde(default)] enabled: bool,
    #[serde(default)] recipients: Vec<String>,
    #[serde(default)] events: Vec<String>,
}

fn clean_recipients(v: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in v.iter().flat_map(|x| x.split([',', ';', '\n', ' '])).map(|x| x.trim()).filter(|x| !x.is_empty()) {
        if !valid_address(r) { return Err(format!("неверный адрес получателя: {r}")); }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(r)) { out.push(r.to_string()); }
    }
    if out.len() > 50 { return Err("не больше 50 получателей".into()); }
    Ok(out)
}

pub async fn put_notify(State(s): S, Extension(w): Extension<Who>, Json(r): Json<NotifyReq>) -> R<Json<Value>> {
    let recipients = clean_recipients(&r.recipients).map_err(bad)?;
    if r.enabled && recipients.is_empty() { return Err(bad("укажите хотя бы одного получателя уведомлений")); }
    let mut events: Vec<String> = Vec::new();
    for e in &r.events {
        if !super::notify::EVENTS.iter().any(|x| x.id == e) { return Err(bad(format!("неизвестное событие {e}"))); }
        if !events.contains(e) { events.push(e.clone()); }
    }
    let desc = format!("уведомления {}: {} получ., событий {}", if r.enabled { "включены" } else { "выключены" }, recipients.len(), events.len());
    let n = super::notify::NotifySettings { enabled: r.enabled, recipients, events };
    s.store.update(|st| { st.notify = n.clone(); Ok(()) })?;
    super::notify::settings_changed();
    s.audit(&w.user, "notify", desc);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TestReq {
    #[serde(default)] to: String,
    #[serde(default)] smtp: Option<SmtpReq>,
}

/// Тестовое письмо — с настройками из формы (ещё не сохранёнными) или сохранёнными.
pub async fn test(State(s): S, Extension(w): Extension<Who>, Json(r): Json<TestReq>) -> R<Json<Value>> {
    let to = r.to.trim().to_string();
    if !valid_address(&to) { return Err(bad("укажите адрес, на который отправить тестовое письмо")); }
    let saved = s.store.read(|st| st.mail.clone());
    let m = match &r.smtp { Some(f) => build(f, &saved).map_err(bad)?, None => saved };
    let body = format!("Это тестовое письмо HopeBackup: сервер исходящей почты настроен верно.\n\nСервер: {}:{} ({})\nОтправил: {}{}", m.host,
        if m.port == 0 { default_port(&m.security) } else { m.port }, m.security, w.user, footer(&s));
    let res = send_with(&m, &[to.clone()], &format!("[HopeBackup {}] Проверка почты", crate::util::hostname()), &body).await;
    s.audit(&w.user, "mail_test", match &res { Ok(()) => format!("тестовое письмо отправлено на {to}"), Err(e) => format!("тестовое письмо на {to} не отправлено: {e}") });
    match res {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(e) => Err(ApiError(StatusCode::BAD_GATEWAY, e)),
    }
}

/// Проверочное уведомление получателям из настроек (сохранённые параметры).
pub async fn test_notify(State(s): S, Extension(w): Extension<Who>) -> R<Json<Value>> {
    let (mail, notify) = s.store.read(|st| (st.mail.clone(), st.notify.clone()));
    if notify.recipients.is_empty() { return Err(bad("не заданы получатели уведомлений")); }
    let subject = format!("[HopeBackup {}] Проверка уведомлений", crate::util::hostname());
    let list: Vec<&str> = super::notify::EVENTS.iter().filter(|e| notify.events.iter().any(|x| x == e.id)).map(|e| e.title).collect();
    let body = format!("Это проверочное уведомление HopeBackup. Так будут приходить письма о выбранных событиях:\n\n{}\n\nУведомления {}. Отправил: {}{}",
        list.iter().map(|t| format!("• {t}")).collect::<Vec<_>>().join("\n"), if notify.enabled { "включены" } else { "сейчас выключены" }, w.user, footer(&s));
    let res = send_with(&mail, &notify.recipients, &subject, &body).await;
    super::notify::push_log(&s, super::notify::LogEntry { at: chrono::Utc::now(), event: "test".into(), subject, to: notify.recipients.clone(), ok: res.is_ok(), error: res.clone().err().unwrap_or_default() });
    match res {
        Ok(()) => Ok(Json(json!({ "ok": true, "to": notify.recipients }))),
        Err(e) => Err(ApiError(StatusCode::BAD_GATEWAY, e)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn addresses() {
        assert!(super::valid_address("admin@corp.local"));
        assert!(!super::valid_address("admin"));
        assert!(!super::valid_address(""));
        assert_eq!(super::default_port("tls"), 465);
        assert_eq!(super::default_port("starttls"), 587);
    }
}
