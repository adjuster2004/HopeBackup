//! Узлы управления: один активный и принимающие (на других площадках).
//!
//! Активный узел каждые 30 секунд (и сразу после изменений) передаёт принимающим весь каталог:
//! площадки, коннекторы, экземпляры, цели, пользователей, агентов, а также сводку заданий. Данные
//! копий передаёт репликация площадок. Если активная площадка потеряна, оператор открывает панель
//! принимающего узла и нажимает «Принять управление»: эпоха увеличивается, узел становится активным,
//! агенты переключаются на него сами (их список узлов приходит с каждым опросом).
//!
//! Эпоха защищает от двух активных узлов: прежний активный, вернувшись, получает синхронизацию с
//! большей эпохой и сам становится принимающим; агенты не выполняют задания узла с меньшей эпохой.

use anyhow::{bail, Result};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::api::{ApiError, R};
use super::auth::Who;
use super::state::{Peer, State as Catalog};
use super::Server;

/// Локальные настройки узла (не передаются другим узлам): `<data>/server/cluster.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Local {
    pub node_id: String,
    #[serde(default)]
    pub name: String,
    /// `active` или `standby`.
    #[serde(default = "d_active")]
    pub role: String,
    #[serde(default)]
    pub epoch: u64,
    /// Адрес этого узла для других узлов и агентов.
    #[serde(default)]
    pub self_url: String,
    #[serde(default)]
    pub active_url: String,
    #[serde(default)]
    pub active_fp: String,
    #[serde(default)]
    pub last_sync: Option<DateTime<Utc>>,
    /// Идентификатор активного узла, от которого принимается каталог.
    #[serde(default)]
    pub active_id: String,
    /// Версия последнего принятого каталога (защита от повтора старых сообщений).
    #[serde(default)]
    pub last_version: u64,
    /// Доступ к площадкам с этого узла (у принимающей площадки свой узел хранения — рядом).
    #[serde(default)]
    pub site_overrides: HashMap<String, SiteOverride>,
    #[serde(default)]
    pub invites: Vec<Invite>,
    /// Коды подключения этого (нового) узла с активного — когда сам он до активного не достаёт.
    #[serde(default)]
    pub offers: Vec<Invite>,
}
fn d_active() -> String { "active".into() }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SiteOverride {
    /// Адрес узла хранения с этого узла управления (пусто — как в каталоге).
    #[serde(default)]
    pub url: String,
    /// Без коннекторов (площадка рядом).
    #[serde(default)]
    pub direct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub hash: String,
    pub expires: DateTime<Utc>,
}

impl Local {
    pub fn load(dir: &std::path::Path) -> Local {
        let p = dir.join("cluster.json");
        std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| Local {
            node_id: crate::util::short_id(),
            name: crate::util::hostname(),
            role: "active".into(),
            epoch: 1,
            self_url: String::new(),
            active_url: String::new(),
            active_fp: String::new(),
            last_sync: None,
            active_id: String::new(),
            last_version: 0,
            site_overrides: HashMap::new(),
            invites: vec![],
            offers: vec![],
        })
    }
}

impl Server {
    pub fn is_active(&self) -> bool { self.cluster.read().role == "active" }

    pub fn save_cluster(&self) {
        let c = self.cluster.read().clone();
        if let Err(e) = crate::util::write_json(&self.dir.join("cluster.json"), &c, true) { tracing::warn!("cluster.json: {e:#}"); }
    }

    fn self_url_or(&self, h: &HeaderMap) -> String {
        let c = self.cluster.read();
        if !c.self_url.is_empty() { return c.self_url.clone(); }
        let host = h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
        if host.is_empty() { format!("https://{}:{}", crate::util::local_ips().first().cloned().unwrap_or_else(crate::util::hostname), self.listen_port) } else { format!("https://{host}") }
    }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("ключ HMAC");
    m.update(body);
    hex::encode(m.finalize().into_bytes())
}

type S = State<Arc<Server>>;

fn bad(m: impl Into<String>) -> ApiError { ApiError(StatusCode::BAD_REQUEST, m.into()) }

fn admin_only(w: &Who) -> R<()> {
    if w.role != "admin" { return Err(ApiError(StatusCode::FORBIDDEN, "действия кластера — только для администратора".into())); }
    Ok(())
}

// ---------------------------------------------------------------- панель

pub async fn status(State(s): S) -> Json<Value> {
    let c = s.cluster.read().clone();
    let peers = s.store.read(|st| st.peers.clone());
    let ps = s.peer_status.lock().clone();
    Json(json!({
        "node_id": c.node_id, "name": c.name, "role": c.role, "epoch": c.epoch, "self_url": c.self_url,
        "active_url": if c.role == "active" { c.self_url.clone() } else { c.active_url.clone() },
        "last_sync": c.last_sync, "fingerprint": s.fingerprint,
        "peers": peers.iter().map(|p| json!({ "id": p.id, "name": p.name, "url": p.url, "fingerprint": p.fingerprint, "joined": p.joined, "self": p.id == c.node_id,
            "sync": ps.get(&p.id), "update": ps.get(&format!("update:{}", p.id)), "connector_id": p.connector_id,
            "version": if p.id == c.node_id { Some(json!(crate::VERSION)) } else { ps.get(&p.id).and_then(|v| v.get("version")).filter(|v| v.is_string()).cloned() } })).collect::<Vec<_>>(),
        "version": crate::VERSION,
        "site_overrides": c.site_overrides,
    }))
}

#[derive(Deserialize)]
pub struct InviteReq {
    #[serde(default)]
    self_url: String,
    #[serde(default)]
    name: String,
}

/// Приглашение для принимающего узла (одноразовый код на 24 часа).
pub async fn invite(State(s): S, Extension(w): Extension<Who>, h: HeaderMap, Json(q): Json<InviteReq>) -> R<Json<Value>> {
    admin_only(&w)?;
    if !s.is_active() { return Err(bad("пригласить можно только с активного узла")); }
    let url = if q.self_url.trim().is_empty() { s.self_url_or(&h) } else { q.self_url.trim().trim_end_matches('/').to_string() };
    let token = crate::util::token();
    {
        let mut c = s.cluster.write();
        c.self_url = url.clone();
        if !q.name.trim().is_empty() { c.name = q.name.trim().to_string(); }
        c.invites.retain(|i| i.expires > Utc::now());
        c.invites.push(Invite { hash: crate::util::sha256_hex(&token), expires: Utc::now() + chrono::Duration::hours(24) });
    }
    s.save_cluster();
    let (id, name) = { let c = s.cluster.read(); (c.node_id.clone(), c.name.clone()) };
    s.store.update(|st| {
        if st.cluster_secret.is_empty() { st.cluster_secret = crate::util::token(); }
        match st.peers.iter_mut().find(|p| p.id == id) {
            Some(p) => { p.url = url.clone(); p.fingerprint = s.fingerprint.clone(); p.name = name.clone(); }
            None => st.peers.push(Peer { id: id.clone(), name: name.clone(), url: url.clone(), fingerprint: s.fingerprint.clone(), joined: Utc::now(), connector_id: String::new() }),
        }
        Ok(())
    })?;
    let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({ "url": url, "fp": s.fingerprint, "token": token }).to_string());
    s.audit(&w.user, "cluster_invite", url.clone());
    Ok(Json(json!({ "code": code, "url": url, "expires": Utc::now() + chrono::Duration::hours(24) })))
}

#[derive(Deserialize)]
pub struct JoinReq {
    code: String,
    #[serde(default)]
    self_url: String,
    #[serde(default)]
    name: String,
    /// Подключить, даже если у узла уже есть свой каталог (он будет заменён; резервная копия — рядом).
    #[serde(default)]
    force: bool,
}

/// Подключить этот (новый) узел принимающим к активному по коду приглашения.
pub async fn join(State(s): S, Extension(w): Extension<Who>, h: HeaderMap, Json(q): Json<JoinReq>) -> R<Json<Value>> {
    admin_only(&w)?;
    // Принимающим становится новый узел: рабочий каталог случайно не затираем.
    let busy = s.store.read(|st| !st.sites.is_empty() || !st.instances.is_empty() || !st.agents.is_empty() || st.peers.len() > 1);
    if busy && !q.force { return Err(bad("у этого узла уже есть площадки, экземпляры или агенты — подключать принимающим можно только новый узел (или с подтверждением замены каталога)")); }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(q.code.trim()).map_err(|_| bad("неверный код приглашения"))?;
    let inv: Value = serde_json::from_slice(&raw).map_err(|_| bad("неверный код приглашения"))?;
    let (aurl, afp, token) = (inv["url"].as_str().unwrap_or(""), inv["fp"].as_str().unwrap_or(""), inv["token"].as_str().unwrap_or(""));
    if aurl.is_empty() || token.is_empty() { return Err(bad("неверный код приглашения")); }
    let me = if q.self_url.trim().is_empty() { s.self_url_or(&h) } else { q.self_url.trim().trim_end_matches('/').to_string() };
    let (id, name) = { let c = s.cluster.read(); (c.node_id.clone(), if q.name.trim().is_empty() { c.name.clone() } else { q.name.trim().to_string() }) };
    let c = crate::tlsutil::client(Some(afp))?;
    let r = c.post(format!("{aurl}/api/cluster/peer/join")).json(&json!({ "token": token, "id": id, "name": name, "url": me, "fingerprint": s.fingerprint })).timeout(Duration::from_secs(30)).send().await.map_err(|e| bad(format!("активный узел {aurl} недоступен: {e}")))?;
    if !r.status().is_success() { return Err(bad(format!("активный узел отказал: {}", r.text().await.unwrap_or_default()))); }
    let v: Value = r.json().await.map_err(|e| bad(e.to_string()))?;
    let st: Catalog = serde_json::from_value(v["state"].clone()).map_err(|e| bad(format!("каталог: {e}")))?;
    let backup = s.dir.join(format!("state.before-join-{}.json", Utc::now().format("%Y%m%d%H%M%S")));
    crate::util::write_json(&backup, &s.store.get(), true)?;
    let version = st.version;
    s.store.replace(st)?;
    {
        let mut c = s.cluster.write();
        c.role = "standby".into();
        c.epoch = v["epoch"].as_u64().unwrap_or(1);
        c.self_url = me.clone();
        c.name = name.clone();
        c.active_url = aurl.to_string();
        c.active_fp = afp.to_string();
        c.active_id = v["active_id"].as_str().unwrap_or("").to_string();
        c.last_version = version;
        c.last_sync = Some(Utc::now());
    }
    s.save_cluster();
    s.audit(&w.user, "cluster_join", format!("принимающий узел для {aurl}; прежний каталог — {}", backup.display()));
    Ok(Json(json!({ "ok": true, "active_url": aurl })))
}

#[derive(Deserialize, Default)]
pub struct TakeoverReq {
    /// Принять управление, даже если активный узел отвечает (осознанное разделение).
    #[serde(default)]
    force: bool,
}

/// Состояние узлов кластера (открытый запрос, без секретов): роль и эпоха.
async fn peers_state(s: &Server) -> Vec<(Peer, Option<Value>)> {
    let me = s.cluster.read().node_id.clone();
    let peers: Vec<Peer> = s.store.read(|st| st.peers.clone()).into_iter().filter(|p| p.id != me).collect();
    let futs = peers.into_iter().map(|p| async move {
        let v = async {
            let c = crate::tlsutil::client(Some(&p.fingerprint)).ok()?;
            let r = c.get(format!("{}/api/cluster/peer/status", p.url)).timeout(Duration::from_secs(5)).send().await.ok()?;
            r.json::<Value>().await.ok()
        }
        .await;
        (p, v)
    });
    futures::future::join_all(futs).await
}

/// Принять управление: этот узел становится активным. Эпоха — больше любой известной в кластере;
/// если активный узел отвечает, управление не перехватывается (кроме явного `force`).
pub async fn takeover(State(s): S, Extension(w): Extension<Who>, body: Option<Json<TakeoverReq>>) -> R<Json<Value>> {
    admin_only(&w)?;
    if s.is_active() { return Err(bad("узел уже активный")); }
    let force = body.map(|b| b.force).unwrap_or(false);
    let states = peers_state(&s).await;
    let mut max_epoch = s.cluster.read().epoch;
    for (p, v) in &states {
        let Some(v) = v else { continue };
        let e = v["epoch"].as_u64().unwrap_or(0);
        max_epoch = max_epoch.max(e);
        if v["role"] == "active" && !force {
            return Err(bad(format!("активный узел «{}» ({}) отвечает (эпоха {e}) — управление не перехватывается. Если он работает с ошибками, остановите его или примите управление принудительно", p.name, p.url)));
        }
    }
    let epoch = {
        let mut c = s.cluster.write();
        c.epoch = max_epoch + 1;
        c.role = "active".into();
        c.active_url = c.self_url.clone();
        c.active_id = c.node_id.clone();
        c.epoch
    };
    s.save_cluster();
    let reach = states.iter().filter(|(_, v)| v.is_some()).count();
    s.audit(&w.user, "cluster_takeover", format!("узел стал активным, эпоха {epoch} (узлов на связи: {reach}){}", if force { ", принудительно" } else { "" }));
    s.jobs.await_running();
    super::notify::emit("cluster", format!("Управление перешло на узел {}", crate::util::hostname()), format!("Узел управления {} стал активным (эпоха {epoch}, узлов на связи: {reach}{}). Принял управление: {}.\n\nАгенты переключатся на этот узел сами.", crate::util::hostname(), if force { ", принудительно" } else { "" }, w.user));
    tracing::warn!("узел принял управление: эпоха {epoch}");
    s.sync_now.notify_one();
    Ok(Json(json!({ "ok": true, "epoch": epoch })))
}

/// Открытое состояние узла для других узлов: идентификатор, роль, эпоха (секретов нет).
pub async fn peer_status(State(s): S) -> Json<Value> {
    let c = s.cluster.read();
    Json(json!({ "node_id": c.node_id, "role": c.role, "epoch": c.epoch, "version": crate::VERSION }))
}

pub async fn override_site(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(o): Json<SiteOverride>) -> R<Json<Value>> {
    admin_only(&w)?;
    {
        let mut c = s.cluster.write();
        if o.url.trim().is_empty() && !o.direct { c.site_overrides.remove(&id); } else { c.site_overrides.insert(id.clone(), SiteOverride { url: o.url.trim().trim_end_matches('/').to_string(), direct: o.direct }); }
    }
    s.save_cluster();
    s.audit(&w.user, "site_override", id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn peer_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    admin_only(&w)?;
    if id == s.cluster.read().node_id { return Err(bad("нельзя удалить сам узел")); }
    s.store.update(|st| { st.peers.retain(|p| p.id != id); Ok(()) })?;
    s.peer_status.lock().remove(&id);
    s.audit(&w.user, "cluster_peer_delete", id);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- между узлами

#[derive(Deserialize)]
pub struct PeerJoin {
    token: String,
    id: String,
    name: String,
    url: String,
    fingerprint: String,
}

pub async fn peer_join(State(s): S, Json(q): Json<PeerJoin>) -> R<Json<Value>> {
    if !s.is_active() { return Err(ApiError(StatusCode::CONFLICT, "этот узел не активный".into())); }
    let hash = crate::util::sha256_hex(&q.token);
    let valid = s.cluster.read().invites.iter().any(|i| crate::util::ct_eq(&i.hash, &hash) && i.expires > Utc::now());
    if !valid { return Err(ApiError(StatusCode::FORBIDDEN, "код приглашения неверный или истёк".into())); }
    // Проверки — до того, как код сгорит: опечатка в адресе не должна стоить приглашения.
    let url = q.url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("https://") || crate::tlsutil::normalize_fp(&q.fingerprint).len() != 64 { return Err(bad("узел не сообщил адрес https:// и отпечаток")); }
    if q.id.is_empty() || !q.id.chars().all(|c| c.is_ascii_alphanumeric()) || q.id.len() > 32 { return Err(bad("неверный идентификатор узла")); }
    let me = s.cluster.read().node_id.clone();
    if q.id == me { return Err(bad("идентификатор совпадает с активным узлом")); }
    let clash = s.store.read(|st| st.peers.iter().any(|p| (p.id == q.id || p.url == url) && crate::tlsutil::normalize_fp(&p.fingerprint) != crate::tlsutil::normalize_fp(&q.fingerprint)));
    if clash { return Err(bad("узел с таким идентификатором или адресом уже есть (другой сертификат) — удалите его из кластера и подключите заново")); }
    {
        let mut c = s.cluster.write();
        let pos = c.invites.iter().position(|i| crate::util::ct_eq(&i.hash, &hash) && i.expires > Utc::now()).ok_or_else(|| ApiError(StatusCode::FORBIDDEN, "код приглашения неверный или истёк".into()))?;
        c.invites.remove(pos);
    }
    s.save_cluster();
    s.store.update(|st| {
        st.peers.retain(|p| p.id != q.id && p.url != url);
        st.peers.push(Peer { id: q.id.clone(), name: q.name.clone(), url: url.clone(), fingerprint: q.fingerprint.clone(), joined: Utc::now(), connector_id: String::new() });
        Ok(())
    })?;
    s.audit("system", "cluster_peer_join", format!("{} ({})", q.name, q.url));
    let epoch = s.cluster.read().epoch;
    s.sync_now.notify_one();
    Ok(Json(json!({ "state": s.store.get(), "epoch": epoch, "active_id": me })))
}

#[derive(Serialize, Deserialize)]
struct SyncMsg {
    from: String,
    url: String,
    fingerprint: String,
    epoch: u64,
    /// Время отправки (UNIX, с) и версия каталога: подпись не даёт подделать, а они — повторить старое.
    ts: i64,
    version: u64,
    state: Catalog,
    #[serde(default)]
    jobs: Vec<super::jobs::Job>,
    #[serde(default)]
    wal: HashMap<String, crate::proto::WalStatus>,
    #[serde(default)]
    repl: HashMap<String, Value>,
}

pub async fn peer_sync(State(s): S, h: HeaderMap, body: Bytes) -> Response {
    let secret = s.store.read(|st| st.cluster_secret.clone());
    let sig = h.get("x-hopebackup-sig").and_then(|v| v.to_str().ok()).unwrap_or("");
    if secret.is_empty() || !crate::util::ct_eq(&sign(&secret, &body), sig) {
        return (StatusCode::UNAUTHORIZED, "подпись синхронизации не совпадает").into_response();
    }
    let m: SyncMsg = match serde_json::from_slice(&body) { Ok(m) => m, Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response() };
    let (role, epoch, me, active_id, last_version) = { let c = s.cluster.read(); (c.role.clone(), c.epoch, c.node_id.clone(), c.active_id.clone(), c.last_version) };
    if m.from == me { return (StatusCode::BAD_REQUEST, "синхронизация самому себе").into_response(); }
    if (Utc::now().timestamp() - m.ts).abs() > 300 { return (StatusCode::BAD_REQUEST, "сообщение синхронизации устарело (или часы узлов расходятся больше чем на 5 минут)").into_response(); }
    // Меньшая эпоха — устаревший активный узел; при равной эпохе у двух активных побеждает меньший id,
    // принимающий при равной эпохе слушает только своего активного (или узел с меньшим id).
    let stale = m.epoch < epoch
        || (m.epoch == epoch && role == "active" && m.from > me)
        || (m.epoch == epoch && role != "active" && !active_id.is_empty() && m.from != active_id && m.from > active_id);
    if stale {
        let (aurl, afp) = { let c = s.cluster.read(); if role == "active" { (c.self_url.clone(), s.fingerprint.clone()) } else { (c.active_url.clone(), c.active_fp.clone()) } };
        return (StatusCode::CONFLICT, Json(json!({ "epoch": epoch, "active_url": aurl, "active_fp": afp }))).into_response();
    }
    // Тот же отправитель в той же эпохе прислал более старую версию каталога — повтор.
    let same = m.epoch == epoch && m.from == active_id;
    if same && m.version < last_version { return (StatusCode::BAD_REQUEST, "устаревшая версия каталога (повтор сообщения)").into_response(); }
    if role == "active" {
        tracing::warn!("узел {} с эпохой {} активен — этот узел становится принимающим", m.url, m.epoch);
        s.audit("system", "cluster_demote", format!("активен {} (эпоха {})", m.url, m.epoch));
    }
    if !(same && m.version == last_version) {
        if let Err(e) = s.store.replace(m.state) { return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(); }
    }
    s.jobs.import(m.jobs);
    { let mut w = s.wal.lock(); for (k, v) in m.wal { w.insert(k, v); } }
    { let mut r = s.repl.lock(); for (k, v) in m.repl { r.insert(k, v); } }
    {
        let mut c = s.cluster.write();
        c.role = "standby".into();
        c.epoch = m.epoch;
        c.active_url = m.url;
        c.active_fp = m.fingerprint;
        c.active_id = m.from;
        c.last_version = m.version;
        c.last_sync = Some(Utc::now());
    }
    s.save_cluster();
    (StatusCode::OK, Json(json!({ "ok": true, "version": crate::VERSION }))).into_response()
}

/// Активный узел: передать каталог принимающим (раз в 30 с и после изменений).
pub async fn sync_loop(s: Arc<Server>) {
    loop {
        let _ = tokio::time::timeout(Duration::from_secs(30), s.sync_now.notified()).await;
        if !s.is_active() { continue; }
        let peers = s.store.read(|st| st.peers.clone());
        let me = s.cluster.read().clone();
        let others: Vec<Peer> = peers.into_iter().filter(|p| p.id != me.node_id).collect();
        if others.is_empty() { continue; }
        let state = s.store.get();
        let msg = SyncMsg {
            from: me.node_id.clone(),
            url: me.self_url.clone(),
            fingerprint: s.fingerprint.clone(),
            epoch: me.epoch,
            ts: Utc::now().timestamp(),
            version: state.version,
            state,
            jobs: s.jobs.list(None, 300),
            wal: s.wal.lock().clone(),
            repl: s.repl.lock().clone(),
        };
        let body = match serde_json::to_vec(&msg) { Ok(b) => b, Err(_) => continue };
        let sig = sign(&msg.state.cluster_secret, &body);
        for p in others {
            let mut version = None;
            let r: Result<()> = async {
                s.peer_link(&p.connector_id, &p.url).await?;
                let c = crate::tlsutil::client(Some(&p.fingerprint))?;
                let resp = c.post(format!("{}/api/cluster/peer/sync", p.url)).header("x-hopebackup-sig", &sig).header("content-type", "application/json").body(body.clone()).timeout(Duration::from_secs(30)).send().await?;
                if resp.status() == StatusCode::CONFLICT {
                    let v: Value = resp.json().await.unwrap_or_default();
                    let e = v["epoch"].as_u64().unwrap_or(0);
                    if e >= me.epoch {
                        // Есть узел с большей эпохой: управление уже принято там.
                        {
                            let mut c = s.cluster.write();
                            if c.role == "active" && c.epoch == me.epoch {
                                c.role = "standby".into();
                                c.epoch = e;
                                c.active_url = v["active_url"].as_str().unwrap_or(&p.url).to_string();
                                c.active_fp = v["active_fp"].as_str().unwrap_or(&p.fingerprint).to_string();
                            }
                        }
                        s.save_cluster();
                        s.audit("system", "cluster_demote", format!("узел {} с эпохой {e} активен — этот узел стал принимающим", p.url));
                        tracing::warn!("узел {} с эпохой {e} активен — этот узел стал принимающим", p.url);
                    }
                    bail!("узел ответил: эпоха {e}");
                }
                if !resp.status().is_success() { bail!("{}: {}", resp.status(), resp.text().await.unwrap_or_default()); }
                version = resp.json::<Value>().await.ok().and_then(|v| v["version"].as_str().map(String::from));
                Ok(())
            }
            .await;
            // Версия узла — из последнего ответа (при ошибке — прежняя).
            let version = version.or_else(|| s.peer_status.lock().get(&p.id).and_then(|v| v["version"].as_str().map(String::from)));
            let v = match r { Ok(()) => json!({ "ok": true, "time": Utc::now(), "error": null, "version": version }), Err(e) => json!({ "ok": false, "time": Utc::now(), "error": format!("{e:#}"), "version": version }) };
            s.peer_status.lock().insert(p.id.clone(), v);
            if !s.is_active() { break; }
        }
    }
}

/// Узлы для агентов: все узлы кластера (адрес и отпечаток).
pub fn servers_for_agents(s: &Server) -> Vec<crate::proto::ServerRef> {
    s.store.read(|st| st.peers.iter().map(|p| crate::proto::ServerRef { url: p.url.clone(), fingerprint: p.fingerprint.clone() }).collect())
}

/// Изменения на принимающем узле запрещены (их перезапишет синхронизация) — кроме восстановления,
/// проверок и действий кластера.
pub fn allowed_on_standby(m: &axum::http::Method, path: &str) -> bool {
    *m == axum::http::Method::GET
        || path == "/api/restore"
        || path.starts_with("/api/cluster/")
        || path == "/api/sites/probe"
        || (path.ends_with("/test") && (path.starts_with("/api/sites/") || path.starts_with("/api/connectors/") || path.starts_with("/api/targets/")))
}

// ---------------------------------------------------------------- подключение со стороны активного узла

/// Новый узел без каталога (можно сделать принимающим, ничего не потеряв).
fn fresh(s: &Server) -> bool {
    s.store.read(|st| st.sites.is_empty() && st.instances.is_empty() && st.agents.is_empty() && st.peers.len() <= 1)
}

fn b64(v: &Value) -> String { base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string()) }

impl Server {
    /// Поднять коннектор до узла управления, если он указан у узла: адрес узла — через VPN.
    pub async fn peer_link(&self, connector_id: &str, url: &str) -> Result<()> {
        if connector_id.is_empty() { return Ok(()); }
        let c = self.store.read(|st| st.connectors.iter().find(|c| c.id == connector_id).cloned())
            .ok_or_else(|| anyhow::anyhow!("коннектор до узла не найден (удалён?) — укажите другой"))?;
        if c.kind == "direct" { return Ok(()); }
        if c.kind == "ssh_tunnel" { bail!("до узлов управления — только VPN-коннекторы (L2TP/IPsec, IPsec, WireGuard, OpenVPN)"); }
        let spec = crate::proto::ConnectorSpec { id: c.id.clone(), name: c.name.clone(), kind: c.kind.clone(), params: c.params.clone() };
        self.tunnels.ensure(&spec, url).await.map(|_| ())
    }
}

#[derive(Deserialize)]
pub struct OfferReq {
    #[serde(default)]
    self_url: String,
    #[serde(default)]
    name: String,
}

/// Код подключения этого (нового) узла: адрес, отпечаток, одноразовый ключ на 24 часа. Его вводят на активном
/// узле — тот сам заходит сюда и передаёт каталог (когда отсюда до активного не достать: VPN в одну сторону).
pub async fn offer(State(s): S, Extension(w): Extension<Who>, h: HeaderMap, Json(q): Json<OfferReq>) -> R<Json<Value>> {
    admin_only(&w)?;
    if !fresh(&s) { return Err(bad("у этого узла уже есть площадки, экземпляры, агенты или другие узлы — принимающим можно сделать только новый узел")); }
    let url = if q.self_url.trim().is_empty() { s.self_url_or(&h) } else { q.self_url.trim().trim_end_matches('/').to_string() };
    if !url.starts_with("https://") || reqwest::Url::parse(&url).is_err() { return Err(bad("адрес этого узла: https://сервер:4443")); }
    let token = crate::util::token();
    let expires = Utc::now() + chrono::Duration::hours(24);
    {
        let mut c = s.cluster.write();
        c.self_url = url.clone();
        if !q.name.trim().is_empty() { c.name = q.name.trim().to_string(); }
        c.offers.retain(|i| i.expires > Utc::now());
        c.offers.push(Invite { hash: crate::util::sha256_hex(&token), expires });
    }
    s.save_cluster();
    s.audit(&w.user, "cluster_offer", url.clone());
    Ok(Json(json!({ "code": b64(&json!({ "kind": "offer", "url": url, "fp": s.fingerprint, "token": token })), "url": url, "expires": expires })))
}

#[derive(Deserialize)]
pub struct AdoptReq {
    code: String,
    #[serde(default)]
    self_url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    connector_id: String,
}

/// Активный узел: подключить принимающий узел по его коду. Активный сам заходит на него (через коннектор, если
/// указан) и передаёт каталог; дальше — обычная синхронизация каждые 30 с тем же путём.
pub async fn adopt(State(s): S, Extension(w): Extension<Who>, h: HeaderMap, Json(q): Json<AdoptReq>) -> R<Json<Value>> {
    admin_only(&w)?;
    if !s.is_active() { return Err(bad("подключать принимающие узлы можно только с активного узла")); }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(q.code.trim()).map_err(|_| bad("неверный код подключения"))?;
    let o: Value = serde_json::from_slice(&raw).map_err(|_| bad("неверный код подключения"))?;
    if o["kind"] != "offer" {
        return Err(bad("это код приглашения активного узла — его вводят на принимающем узле («Подключиться к активному узлу»). Здесь нужен код, полученный на принимающем узле кнопкой «Подготовить к подключению»"));
    }
    let (purl, pfp, token) = (o["url"].as_str().unwrap_or("").trim_end_matches('/').to_string(), o["fp"].as_str().unwrap_or("").to_string(), o["token"].as_str().unwrap_or("").to_string());
    if !purl.starts_with("https://") || token.is_empty() || crate::tlsutil::normalize_fp(&pfp).len() != 64 { return Err(bad("неверный код подключения")); }
    if !q.connector_id.is_empty() && !s.store.read(|st| st.connectors.iter().any(|c| c.id == q.connector_id)) { return Err(bad("коннектор не найден")); }
    s.peer_link(&q.connector_id, &purl).await.map_err(|e| bad(format!("коннектор до узла: {e:#}")))?;
    // Этот узел — в списке узлов кластера, общий ключ синхронизации задан (как при приглашении).
    let me_url = if q.self_url.trim().is_empty() { s.self_url_or(&h) } else { q.self_url.trim().trim_end_matches('/').to_string() };
    let (id, name) = {
        let mut c = s.cluster.write();
        c.self_url = me_url.clone();
        if !q.name.trim().is_empty() { c.name = q.name.trim().to_string(); }
        (c.node_id.clone(), c.name.clone())
    };
    s.save_cluster();
    s.store.update(|st| {
        if st.cluster_secret.is_empty() { st.cluster_secret = crate::util::token(); }
        match st.peers.iter_mut().find(|p| p.id == id) {
            Some(p) => { p.url = me_url.clone(); p.fingerprint = s.fingerprint.clone(); p.name = name.clone(); }
            None => st.peers.push(Peer { id: id.clone(), name: name.clone(), url: me_url.clone(), fingerprint: s.fingerprint.clone(), joined: Utc::now(), connector_id: String::new() }),
        }
        Ok(())
    })?;
    let epoch = s.cluster.read().epoch;
    let c = crate::tlsutil::client(Some(&pfp))?;
    let r = c.post(format!("{purl}/api/cluster/peer/adopt"))
        .json(&json!({ "token": token, "active": { "id": id, "name": name, "url": me_url, "fingerprint": s.fingerprint }, "epoch": epoch, "state": s.store.get() }))
        .timeout(Duration::from_secs(60)).send().await
        .map_err(|e| bad(format!("узел {purl} недоступен: {e}{}", if q.connector_id.is_empty() { " — если до него можно дойти только через VPN, выберите коннектор" } else { "" })))?;
    if r.status() == StatusCode::NOT_FOUND { return Err(bad(format!("на узле {purl} версия старше 0.8.14 — обновите его в его собственной панели («Обновления»), затем получите код подключения заново"))); }
    if !r.status().is_success() { return Err(bad(format!("узел {purl} отказал: {}", r.text().await.unwrap_or_default()))); }
    let v: Value = r.json().await.map_err(|e| bad(e.to_string()))?;
    let pid = v["id"].as_str().unwrap_or("").to_string();
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_alphanumeric()) || pid.len() > 32 || pid == id { return Err(bad("узел ответил неверным идентификатором")); }
    let pname = v["name"].as_str().unwrap_or(&pid).to_string();
    s.store.update(|st| {
        st.peers.retain(|p| p.id != pid && p.url != purl);
        st.peers.push(Peer { id: pid.clone(), name: pname.clone(), url: purl.clone(), fingerprint: pfp.clone(), joined: Utc::now(), connector_id: q.connector_id.clone() });
        Ok(())
    })?;
    if let Some(ver) = v["version"].as_str() { s.peer_status.lock().insert(pid.clone(), json!({ "ok": true, "time": Utc::now(), "error": null, "version": ver })); }
    s.audit(&w.user, "cluster_adopt", format!("{pname} ({purl})"));
    s.sync_now.notify_one();
    Ok(Json(json!({ "ok": true, "id": pid, "name": pname })))
}

#[derive(Deserialize)]
pub struct PeerInfo {
    id: String,
    name: String,
    url: String,
    fingerprint: String,
}

#[derive(Deserialize)]
pub struct PeerAdopt {
    token: String,
    active: PeerInfo,
    epoch: u64,
    state: Catalog,
}

/// Новый узел: активный узел подключает его по коду подключения и передаёт каталог.
pub async fn peer_adopt(State(s): S, Json(q): Json<PeerAdopt>) -> R<Json<Value>> {
    let hash = crate::util::sha256_hex(&q.token);
    let valid = s.cluster.read().offers.iter().any(|i| crate::util::ct_eq(&i.hash, &hash) && i.expires > Utc::now());
    if !valid { return Err(ApiError(StatusCode::FORBIDDEN, "код подключения неверный или истёк — получите новый на принимающем узле".into())); }
    if !fresh(&s) { return Err(ApiError(StatusCode::CONFLICT, "у узла уже есть свой каталог (площадки, экземпляры, агенты) — принимающим можно сделать только новый узел".into())); }
    let a = &q.active;
    if !a.url.starts_with("https://") || crate::tlsutil::normalize_fp(&a.fingerprint).len() != 64 { return Err(bad("активный узел не сообщил адрес https:// и отпечаток")); }
    if a.id.is_empty() || !a.id.chars().all(|c| c.is_ascii_alphanumeric()) || a.id.len() > 32 { return Err(bad("неверный идентификатор активного узла")); }
    if a.id == s.cluster.read().node_id { return Err(bad("идентификатор совпадает с этим узлом")); }
    if q.state.cluster_secret.is_empty() { return Err(bad("в каталоге нет ключа синхронизации")); }
    {
        let mut c = s.cluster.write();
        let pos = c.offers.iter().position(|i| crate::util::ct_eq(&i.hash, &hash) && i.expires > Utc::now())
            .ok_or_else(|| ApiError(StatusCode::FORBIDDEN, "код подключения неверный или истёк".into()))?;
        c.offers.remove(pos);
    }
    let backup = s.dir.join(format!("state.before-join-{}.json", Utc::now().format("%Y%m%d%H%M%S")));
    crate::util::write_json(&backup, &s.store.get(), true)?;
    let version = q.state.version;
    s.store.replace(q.state)?;
    {
        let mut c = s.cluster.write();
        c.role = "standby".into();
        c.epoch = q.epoch.max(1);
        c.active_url = a.url.trim_end_matches('/').to_string();
        c.active_fp = a.fingerprint.clone();
        c.active_id = a.id.clone();
        c.last_version = version;
        c.last_sync = Some(Utc::now());
    }
    s.save_cluster();
    s.audit("system", "cluster_join", format!("подключён принимающим узлом с активного {} ({}); прежний каталог — {}", a.name, a.url, backup.display()));
    let c = s.cluster.read();
    Ok(Json(json!({ "ok": true, "id": c.node_id, "name": c.name, "url": c.self_url, "fingerprint": s.fingerprint, "version": crate::VERSION })))
}

// ---------------------------------------------------------------- обновление принимающих узлов с активного

fn update_msg(peer_id: &str, ts: i64, sha: &str) -> String { format!("update|{peer_id}|{ts}|{sha}") }

/// Активный узел: обновить принимающий узел до своей версии (архив выпуска из своей программы и комплекта).
pub async fn peer_update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    admin_only(&w)?;
    if !s.is_active() { return Err(bad("обновлять другие узлы можно только с активного узла")); }
    let me = s.cluster.read().node_id.clone();
    if id == me { return Err(bad("это этот узел — он обновляется выше, на этой же странице")); }
    let p = s.store.read(|st| st.peers.iter().find(|p| p.id == id).cloned()).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "узел не найден".into()))?;
    let key = format!("update:{id}");
    {
        let mut ps = s.peer_status.lock();
        if ps.get(&key).map(|v| v["running"] == true).unwrap_or(false) { return Err(bad("обновление этого узла уже идёт")); }
        ps.insert(key.clone(), json!({ "running": true, "ok": null, "time": Utc::now(), "target": crate::VERSION, "error": null }));
    }
    s.audit(&w.user, "cluster_peer_update", format!("{} → {}", p.name, crate::VERSION));
    let s2 = s.clone();
    tokio::spawn(async move {
        let r = push_update(&s2, &p).await;
        let v = match &r {
            Ok(ver) => json!({ "running": false, "ok": true, "time": Utc::now(), "target": ver, "error": null }),
            Err(e) => json!({ "running": false, "ok": false, "time": Utc::now(), "target": crate::VERSION, "error": format!("{e:#}") }),
        };
        if let Err(e) = &r { tracing::warn!("обновление узла {}: {e:#}", p.name); }
        s2.peer_status.lock().insert(key, v);
    });
    Ok(Json(json!({ "ok": true })))
}

async fn push_update(s: &Arc<Server>, p: &Peer) -> Result<String> {
    use sha2::Digest;
    s.peer_link(&p.connector_id, &p.url).await?;
    let path = super::updates::own_release_archive(s).await?;
    let body = tokio::fs::read(&path).await?;
    let _ = std::fs::remove_file(&path);
    let sha = hex::encode(sha2::Sha256::digest(&body));
    let ts = Utc::now().timestamp();
    let secret = s.store.read(|st| st.cluster_secret.clone());
    if secret.is_empty() { bail!("кластер не настроен"); }
    let sig = sign(&secret, update_msg(&p.id, ts, &sha).as_bytes());
    let c = crate::tlsutil::client(Some(&p.fingerprint))?;
    let r = c.post(format!("{}/api/cluster/peer/update", p.url))
        .header("x-hopebackup-ts", ts.to_string()).header("x-hopebackup-sha256", &sha).header("x-hopebackup-sig", sig)
        .body(body).timeout(Duration::from_secs(1200)).send().await?;
    if r.status() == StatusCode::NOT_FOUND { bail!("на узле версия старше 0.8.14 — один раз обновите его в его собственной панели («Обновления» → загрузить архив выпуска), дальше — отсюда"); }
    if !r.status().is_success() { bail!("{}", r.text().await.unwrap_or_default()); }
    let v: Value = r.json().await.unwrap_or_default();
    Ok(v["version"].as_str().unwrap_or(crate::VERSION).to_string())
}

/// Принимающий узел: пакет обновления от активного. Подпись — общим ключом кластера, с адресатом и временем
/// (чужому узлу или повторно позже не подсунуть); сам пакет проверяется как обычная загрузка.
pub async fn peer_update_recv(State(s): S, h: HeaderMap, body: Bytes) -> Response {
    use sha2::Digest;
    if s.is_active() { return (StatusCode::CONFLICT, "этот узел активный — его обновляют на его странице «Обновления»").into_response(); }
    let secret = s.store.read(|st| st.cluster_secret.clone());
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let ts: i64 = hv("x-hopebackup-ts").parse().unwrap_or(0);
    if (Utc::now().timestamp() - ts).abs() > 300 { return (StatusCode::BAD_REQUEST, "запрос устарел (или часы узлов расходятся больше чем на 5 минут)").into_response(); }
    let sha = hex::encode(sha2::Sha256::digest(&body));
    if hv("x-hopebackup-sha256") != sha { return (StatusCode::BAD_REQUEST, "пакет повреждён при передаче (контрольная сумма)").into_response(); }
    let me = s.cluster.read().node_id.clone();
    if secret.is_empty() || !crate::util::ct_eq(&sign(&secret, update_msg(&me, ts, &sha).as_bytes()), &hv("x-hopebackup-sig")) {
        return (StatusCode::UNAUTHORIZED, "подпись обновления не совпадает").into_response();
    }
    match super::updates::apply_from_peer(s.clone(), body).await {
        Ok(v) => (StatusCode::OK, Json(json!({ "ok": true, "version": v }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}
