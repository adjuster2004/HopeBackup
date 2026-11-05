//! Восстановление доступа к панели (как в HopeDFS): ссылка сброса пароля по почте и аварийный токен вендора.
//!
//! **По почте.** Пользователь вводит имя учётной записи панели или её адрес почты. Если учётная запись есть,
//! у неё указан адрес и сервер исходящей почты настроен — на этот адрес уходит одноразовая ссылка (действует
//! 30 минут). Иначе ссылка записывается на сервере управления в `<server>/password_reset_link.txt` (0600,
//! только root). Ответ одинаковый для любых имён — по нему нельзя узнать, существует ли учётная запись.
//! Токен ссылки хранится в каталоге только в виде SHA-256, поэтому ссылка работает на любом узле управления.
//!
//! **Аварийно.** Для закрытого контура или когда почты нет: панель показывает код запроса с отпечатком
//! сервера (как запрос лицензии HopeDFS); вендор выпускает токен (RS256-JWT, тот же ключ, что у лицензий
//! HopeDFS) с новым паролем. Токен принимается только на этом сервере (`sub` = отпечаток), до срока `exp` и
//! один раз (`jti`). Учётная запись — `user` из токена (по умолчанию Administrator); если её нет, она
//! создаётся с ролью «Администратор».
//!
//! После сброса пароля все открытые сеансы этой учётной записи закрываются.

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use super::api::{ApiError, R};
use super::Server;

type S = State<Arc<Server>>;

pub const LINK_TTL_SECS: i64 = 30 * 60;
pub const PURPOSE: &str = "admin-recovery";
pub const PRODUCT: &str = "HopeBackup";
pub const ISSUER: &str = "HOPE-LICENSE-SERVER";

#[cfg(not(feature = "testkey"))]
const VENDOR_PUBKEY: &str = include_str!("vendor_public.pem");
#[cfg(feature = "testkey")]
const VENDOR_PUBKEY: &str = include_str!(env!("HOPEBACKUP_TEST_VENDOR_PUBKEY"));

/// Ссылка сброса пароля (в каталоге — только хэш токена).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PasswordReset {
    pub user: String,
    pub hash: String,
    pub expires: chrono::DateTime<Utc>,
}

fn err(code: StatusCode, m: impl Into<String>) -> ApiError { ApiError(code, m.into()) }

/// Ограничение частоты по адресу клиента: не больше 10 запросов за 15 минут на каждое действие.
static RATE: Mutex<Option<HashMap<String, Vec<i64>>>> = parking_lot::const_mutex(None);
fn rate(kind: &str, ip: &str) -> Result<(), ApiError> {
    let now = Utc::now().timestamp();
    let mut g = RATE.lock();
    let m = g.get_or_insert_with(HashMap::new);
    let v = m.entry(format!("{kind}|{ip}")).or_default();
    v.retain(|t| now - *t < 900);
    if v.len() >= 10 { return Err(err(StatusCode::TOO_MANY_REQUESTS, "слишком много попыток — повторите через 15 минут")); }
    v.push(now);
    if m.len() > 10_000 { m.retain(|_, v| v.iter().any(|t| now - *t < 900)); }
    Ok(())
}

fn sha(s: &str) -> String { crate::util::sha256_hex(s) }

/// Отпечаток сервера — как у лицензий HopeDFS (machine-id, только буквы и цифры, заглавными).
pub fn fingerprint() -> String {
    let raw = ["/var/lib/dbus/machine-id", "/etc/machine-id"].iter().find_map(|p| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())).unwrap_or_default();
    let f: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    if f.is_empty() { "HOPEBACKUPUNKNOWNHW".to_string() } else { f }
}

pub fn request_text() -> String {
    format!("=== HOPE BACKUP ADMIN RECOVERY REQUEST ===\nProduct: {PRODUCT}\nPurpose: {PURPOSE}\nNode: {}\nFingerprint: {}\n=== END REQUEST ===", crate::util::hostname(), fingerprint())
}

/// Задать пароль учётной записи и закрыть её сеансы; `create` — создать администратора, если её нет.
fn set_password(s: &Server, user: &str, password: &str, create: bool) -> anyhow::Result<String> {
    let hash = super::auth::hash_password(password);
    s.store.update(|st| {
        if let Some(u) = st.users.iter_mut().find(|u| u.name.eq_ignore_ascii_case(user)) {
            u.pw_hash = hash.clone();
            u.session_gen += 1;
            return Ok(u.name.clone());
        }
        if !create { anyhow::bail!("учётная запись не найдена"); }
        st.users.push(super::state::User { name: user.to_string(), pw_hash: hash.clone(), role: "admin".into(), email: String::new(), session_gen: 0, created: Utc::now() });
        Ok(user.to_string())
    })
}

// ----------------------------------------------------------------- по почте

#[derive(Deserialize)]
pub struct RecoverReq {
    #[serde(default)]
    identifier: String,
}

pub async fn request(State(s): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(r): Json<RecoverReq>) -> R<Json<Value>> {
    let ip = peer.ip().to_string();
    rate("request", &ip)?;
    let id = r.identifier.trim().to_string();
    if id.is_empty() || id.len() > 254 { return Err(err(StatusCode::BAD_REQUEST, "укажите имя учётной записи или адрес почты")); }
    let st = s.store.get();
    let user = st.users.iter().find(|u| !u.email.is_empty() && u.email.eq_ignore_ascii_case(&id)).or_else(|| st.users.iter().find(|u| u.name.eq_ignore_ascii_case(&id))).cloned();
    let generic = json!({ "ok": true });
    let Some(u) = user else {
        s.audit(&id, "password_reset", format!("запрос сброса пароля с {ip}: учётная запись не найдена"));
        return Ok(Json(generic));
    };
    // Токен ссылки: 32 случайных байта; в каталоге — только SHA-256.
    let token = crate::util::token();
    let (hash, name) = (sha(&token), u.name.clone());
    s.store.update(|st| {
        let now = Utc::now();
        st.password_resets.retain(|p| p.expires > now && p.user != name);
        st.password_resets.push(PasswordReset { user: name.clone(), hash: hash.clone(), expires: now + chrono::Duration::seconds(LINK_TTL_SECS) });
        Ok(())
    })?;
    let link = format!("{}/reset-password?token={token}", super::mail::panel_url(&s));
    let mut via = "file";
    if super::mail::configured(&st.mail) && !u.email.is_empty() {
        let body = format!("Здравствуйте!\n\nДля учётной записи {} запрошен сброс пароля к панели HopeBackup (с адреса {ip}).\n\nЧтобы задать новый пароль, откройте ссылку (действует 30 минут, один раз):\n{link}\n\nЕсли вы не запрашивали сброс, просто удалите это письмо — пароль не изменится.{}",
            u.name, super::mail::footer(&s));
        match super::mail::send_with(&st.mail, &[u.email.clone()], &format!("[HopeBackup {}] Сброс пароля к панели", crate::util::hostname()), &body).await {
            Ok(()) => via = "mail",
            Err(e) => tracing::warn!("письмо со ссылкой сброса пароля для {} не отправлено: {e} — ссылка записана в файл", u.name),
        }
    }
    if via == "file" {
        let f = s.dir.join("password_reset_link.txt");
        let txt = format!("Ссылка сброса пароля к панели HopeBackup для {}\nЗапрошена: {} UTC с адреса {ip}\nДействует 30 минут, один раз:\n{link}\n", u.name, Utc::now().format("%d.%m.%Y %H:%M:%S"));
        if let Err(e) = crate::util::write_secret(&f, txt.as_bytes()) { tracing::error!("не удалось записать {}: {e}", f.display()); }
    }
    let how = if via == "mail" { format!("отправлена на {}", u.email) } else { "записана в файл password_reset_link.txt на сервере управления".into() };
    tracing::warn!("запрошен сброс пароля к панели для {} с {ip} — ссылка {how}", u.name);
    s.audit(&u.name, "password_reset", format!("запрошена ссылка сброса пароля с {ip} ({how})"));
    super::notify::emit("password_reset", format!("Запрошен сброс пароля для {}", u.name),
        format!("Для учётной записи {} запрошена ссылка сброса пароля к панели HopeBackup.\nАдрес клиента: {ip}\nСсылка {}.\n\nЕсли это были не вы и не ваши коллеги — проверьте журнал аудита.", u.name,
            if via == "mail" { "отправлена на адрес почты учётной записи" } else { "записана на сервере управления в файл password_reset_link.txt" }));
    Ok(Json(generic))
}

#[derive(Deserialize)]
pub struct CheckQ {
    #[serde(default)]
    token: String,
}

/// Действительна ли ссылка (для страницы сброса): имя учётной записи.
pub async fn check(State(s): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, Query(q): Query<CheckQ>) -> R<Json<Value>> {
    rate("check", &peer.ip().to_string())?;
    let h = sha(q.token.trim());
    let now = Utc::now();
    let p = s.store.read(|st| st.password_resets.iter().find(|p| p.expires > now && crate::util::ct_eq(&p.hash, &h)).cloned());
    let Some(p) = p else { return Err(err(StatusCode::GONE, "ссылка недействительна или её срок истёк — запросите новую")) };
    Ok(Json(json!({ "ok": true, "user": p.user, "expires": p.expires })))
}

#[derive(Deserialize)]
pub struct ResetReq {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
}

pub async fn reset(State(s): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(r): Json<ResetReq>) -> R<Json<Value>> {
    let ip = peer.ip().to_string();
    rate("reset", &ip)?;
    if r.password.chars().count() < 8 { return Err(err(StatusCode::BAD_REQUEST, "пароль должен быть не короче 8 символов")); }
    let h = sha(r.token.trim());
    // Найти и сразу погасить ссылку (одноразовая).
    let found = s.store.update(|st| {
        let now = Utc::now();
        st.password_resets.retain(|p| p.expires > now);
        let pos = st.password_resets.iter().position(|p| crate::util::ct_eq(&p.hash, &h)).ok_or_else(|| anyhow::anyhow!("ссылка недействительна или её срок истёк — запросите новую"))?;
        let p = st.password_resets.remove(pos);
        if !st.users.iter().any(|u| u.name == p.user) { anyhow::bail!("учётная запись не найдена"); }
        Ok(p.user)
    });
    let name = match found {
        Ok(n) => n,
        Err(e) => {
            s.audit("", "password_reset", format!("сброс пароля по ссылке с {ip} отклонён: {e}"));
            return Err(err(StatusCode::GONE, e.to_string()));
        }
    };
    set_password(&s, &name, &r.password, false)?;
    let _ = std::fs::remove_file(s.dir.join("password_reset_link.txt"));
    tracing::warn!("пароль {name} сменён по ссылке сброса (клиент {ip})");
    s.audit(&name, "password_reset", format!("пароль сменён по ссылке сброса (клиент {ip}); открытые сеансы закрыты"));
    super::notify::emit("password_reset", format!("Пароль {name} сменён по ссылке сброса"),
        format!("Пароль учётной записи {name} к панели HopeBackup сменён по ссылке сброса.\nАдрес клиента: {ip}\n\nЕсли это были не вы — смените пароль и проверьте журнал аудита."));
    Ok(Json(json!({ "ok": true, "user": name })))
}

// ----------------------------------------------------------------- аварийный токен

pub async fn emergency_request() -> Json<Value> {
    Json(json!({ "fingerprint": fingerprint(), "node": crate::util::hostname(), "req": request_text(), "product": PRODUCT }))
}

#[derive(Debug, Deserialize)]
pub struct RecoveryClaims {
    pub sub: String,
    pub exp: i64,
    #[serde(default)]
    pub iss: String,
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub new_password: String,
    #[serde(default)]
    pub jti: String,
}

/// Проверка токена: подпись вендора, назначение, продукт, отпечаток сервера, срок.
pub fn verify_token(token: &str) -> Result<RecoveryClaims, String> {
    use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
    let token: String = token.split_whitespace().collect();
    if token.split('.').count() != 3 { return Err("это не токен восстановления (ожидается JWT из трёх частей)".into()); }
    let dk = DecodingKey::from_rsa_pem(VENDOR_PUBKEY.as_bytes()).map_err(|e| format!("вшитый ключ: {e}"))?;
    let mut val = Validation::new(Algorithm::RS256);
    val.validate_exp = false;
    val.validate_aud = false;
    val.required_spec_claims.clear();
    let c = decode::<RecoveryClaims>(&token, &dk, &val).map_err(|e| format!("подпись не прошла проверку — токен выпущен не вендором ({e})"))?.claims;
    if c.purpose != PURPOSE { return Err(format!("токен не для восстановления доступа (назначение «{}»)", c.purpose)); }
    if !c.product.is_empty() && !c.product.eq_ignore_ascii_case(PRODUCT) { return Err(format!("токен выпущен для другого продукта ({})", c.product)); }
    if !c.iss.is_empty() && !c.iss.eq_ignore_ascii_case(ISSUER) { return Err(format!("токен выпущен другим издателем ({})", c.iss)); }
    let fp = fingerprint();
    if !c.sub.eq_ignore_ascii_case(&fp) { return Err(format!("токен выпущен для другого сервера (отпечаток {}, у этого сервера {fp})", c.sub)); }
    if c.exp <= Utc::now().timestamp() { return Err("срок действия токена истёк — запросите новый у вендора".into()); }
    if c.new_password.chars().count() < 8 { return Err("в токене нет нового пароля (не короче 8 символов)".into()); }
    Ok(c)
}

#[derive(Deserialize)]
pub struct EmergencyReq {
    #[serde(default)]
    token: String,
}

pub async fn emergency(State(s): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(r): Json<EmergencyReq>) -> R<Json<Value>> {
    let ip = peer.ip().to_string();
    rate("emergency", &ip)?;
    let c = match verify_token(&r.token) {
        Ok(c) => c,
        Err(e) => {
            s.audit("", "password_reset", format!("аварийный токен с {ip} отклонён: {e}"));
            return Err(err(StatusCode::BAD_REQUEST, e));
        }
    };
    let tok_id = if c.jti.trim().is_empty() { sha(&r.token.split_whitespace().collect::<String>()) } else { c.jti.trim().to_string() };
    let user = { let u = c.user.trim(); if u.is_empty() { "Administrator".to_string() } else { u.rsplit('\\').next().unwrap_or("Administrator").to_string() } };
    if user.is_empty() || user.len() > 64 || user.chars().any(|ch| ch.is_control()) { return Err(err(StatusCode::BAD_REQUEST, "недопустимое имя учётной записи в токене")); }
    // Погасить токен (один раз), затем сменить пароль.
    let tid = tok_id.clone();
    let used = s.store.update(|st| {
        if st.recovery_used.iter().any(|x| *x == tid) { anyhow::bail!("этот токен уже использован — запросите новый у вендора"); }
        st.recovery_used.push(tid.clone());
        while st.recovery_used.len() > 100 { st.recovery_used.remove(0); }
        Ok(())
    });
    if let Err(e) = used {
        s.audit(&user, "password_reset", format!("аварийный токен с {ip} отклонён: {e}"));
        return Err(err(StatusCode::CONFLICT, e.to_string()));
    }
    let name = set_password(&s, &user, &c.new_password, true)?;
    tracing::warn!("доступ к панели восстановлен аварийным токеном вендора: пароль {name} задан из токена (клиент {ip})");
    s.audit(&name, "password_reset", format!("пароль задан аварийным токеном вендора (токен {}, клиент {ip}); открытые сеансы закрыты", &tok_id[..tok_id.len().min(16)]));
    super::notify::emit("password_reset", format!("Доступ восстановлен аварийным токеном: {name}"),
        format!("Пароль учётной записи {name} к панели HopeBackup задан аварийным токеном вендора.\nАдрес клиента: {ip}\n\nСразу после входа смените пароль."));
    Ok(Json(json!({ "ok": true, "user": name })))
}
