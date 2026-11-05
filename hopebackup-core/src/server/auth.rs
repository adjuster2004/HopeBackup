//! Вход в панель, сеансы, роли и права (как в HopeDFS: право выводится из метода и пути запроса,
//! поэтому новые обработчики защищены по умолчанию).

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use hmac::{Hmac, Mac};
use std::sync::Arc;

use super::Server;

/// Хэш пароля панели: PBKDF2-HMAC-SHA256.
pub fn hash_password(pw: &str) -> String {
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    let iter = 100_000u32;
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pw.as_bytes(), &salt, iter, &mut out);
    let e = base64::engine::general_purpose::STANDARD;
    format!("pbkdf2${}${}${}", iter, e.encode(salt), e.encode(out))
}

pub fn verify_password(pw: &str, h: &str) -> bool {
    let p: Vec<&str> = h.split('$').collect();
    if p.len() != 4 || p[0] != "pbkdf2" { return false; }
    let e = base64::engine::general_purpose::STANDARD;
    let (Ok(iter), Ok(salt), Ok(exp)) = (p[1].parse::<u32>(), e.decode(p[2]), e.decode(p[3])) else { return false };
    let mut out = vec![0u8; exp.len()];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pw.as_bytes(), &salt, iter, &mut out);
    subtle::ConstantTimeEq::ct_eq(out.as_slice(), exp.as_slice()).into()
}

fn sign(secret: &str, payload: &str) -> String {
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("ключ HMAC");
    m.update(payload.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// Токен сеанса на 12 часов; `gen` — поколение сеансов пользователя (смена пароля закрывает старые).
pub fn issue(secret: &str, user: &str, gen: u64) -> String {
    let exp = chrono::Utc::now().timestamp() + 12 * 3600;
    let payload = format!("{user}|{gen}|{exp}");
    let p64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
    format!("{p64}.{}", sign(secret, &payload))
}

/// Пользователь и поколение сеансов из действительного токена.
pub fn check(secret: &str, token: &str) -> Option<(String, u64)> {
    let (p64, sig) = token.split_once('.')?;
    let payload = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p64).ok()?).ok()?;
    if !crate::util::ct_eq(&sign(secret, &payload), sig) { return None; }
    let (rest, exp) = payload.rsplit_once('|')?;
    let (user, gen) = rest.rsplit_once('|')?;
    if exp.parse::<i64>().ok()? < chrono::Utc::now().timestamp() { return None; }
    Some((user.to_string(), gen.parse().ok()?))
}

pub fn perms(role: &str) -> Vec<&'static str> {
    match role {
        "admin" => vec!["view", "edit", "restore", "admin"],
        "operator" => vec!["view", "edit"],
        "restore" => vec!["view", "restore"],
        "auditor" => vec!["view"],
        _ => vec![],
    }
}

/// Какое право нужно для запроса.
fn need(m: &Method, path: &str) -> &'static str {
    if path.starts_with("/api/users") { return "admin"; }
    // Обновления: замена программы сервера и агентов — только администратор (и для просмотра).
    if path.starts_with("/api/updates") { return "admin"; }
    // Установка пакетов дистрибутива на сервере и агентах — только администратор.
    if path == "/api/connectors/install" { return "admin"; }
    if path.starts_with("/api/settings") && *m != Method::GET { return "admin"; }
    // Почта и уведомления: пароль SMTP и адреса — только администратор (и для просмотра).
    if path.starts_with("/api/mail") { return "admin"; }
    // Учётные данные SSH серверов (root) — только администратор; опись и проверка — оператор.
    if path.starts_with("/api/machines") && *m != Method::GET {
        return if path.ends_with("/inventory") || path.ends_with("/test") { "edit" } else { "admin" };
    }
    // Кластер управления: каталог целиком (с секретами) уходит на другие узлы — только администратор.
    if path.starts_with("/api/cluster") && *m != Method::GET { return "admin"; }
    if path == "/api/restore" { return "restore"; }
    // Скачивание образа диска (одноразовая ссылка) — только администратор.
    // Оценка места под образы — для формы продукта «Образы qcow2» (просмотр); скачивание образов — только admin.
    if path == "/api/image/estimate" || path == "/api/image/requirements" { return "view"; }
    if path.starts_with("/api/image") { return "admin"; }
    if *m == Method::GET { "view" } else { "edit" }
}

#[derive(Clone, Debug)]
pub struct Who {
    pub user: String,
    pub role: String,
}

pub async fn middleware(State(s): State<Arc<Server>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    // Восстановление доступа (ссылка по почте, аварийный токен) — без входа, с ограничением частоты.
    let recovery = ["/api/auth/recover-request", "/api/auth/recover-check", "/api/auth/recover-reset", "/api/auth/recovery-request", "/api/auth/recover-emergency"].contains(&path.as_str());
    // Скачивание образа по одноразовой ссылке: сам токен — ключ (ссылку выдал администратор, она
    // разовая и с коротким сроком), поэтому заголовок не нужен — иначе браузер не передаст его по ссылке.
    let open = !path.starts_with("/api/") || path == "/api/auth/login" || recovery || path == "/api/health" || path.starts_with("/api/agent/") || path.starts_with("/api/cluster/peer/") || path.starts_with("/api/image/dl/");
    if open { return next.run(req).await; }
    let tok = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").to_string();
    let secret = s.store.read(|st| st.session_secret.clone());
    let Some((user, gen)) = check(&secret, &tok) else {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "требуется вход" }))).into_response();
    };
    let Some((role, cur_gen)) = s.store.read(|st| st.users.iter().find(|u| u.name == user).map(|u| (u.role.clone(), u.session_gen))) else {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "пользователь удалён" }))).into_response();
    };
    if gen != cur_gen {
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "пароль изменён — войдите заново" }))).into_response();
    }
    let n = need(req.method(), &path);
    if !perms(&role).contains(&n) {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": format!("недостаточно прав (нужно «{n}»)") }))).into_response();
    }
    if !s.is_active() && !super::cluster::allowed_on_standby(req.method(), &path) {
        let active = s.cluster.read().active_url.clone();
        return (StatusCode::CONFLICT, axum::Json(serde_json::json!({ "error": format!("это принимающий узел: настройки меняются на активном узле {active} (или нажмите «Принять управление» в разделе «Узлы управления»)") }))).into_response();
    }
    req.extensions_mut().insert(Who { user, role });
    next.run(req).await
}
