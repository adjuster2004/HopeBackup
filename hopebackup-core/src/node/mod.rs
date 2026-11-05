//! Узел хранения на площадке: принимает зашифрованные блоки по HTTPS и хранит репозитории.
//!
//! Права токенов: `read` — только чтение (восстановление), `append` — чтение и дописывание
//! (агенты), `admin` — всё, включая очистку по политике хранения. Агент не может удалить копии:
//! даже скомпрометированный источник не уничтожит то, что уже на площадке.

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::store::backend::Backend;
use crate::store::local::LocalBackend;
use crate::store::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Append,
    Admin,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeToken {
    pub name: String,
    /// SHA-256 токена (сам токен на узле не хранится).
    pub sha256: String,
    pub role: Role,
    /// Ограничение одним репозиторием; пусто — все.
    #[serde(default)]
    pub repo: String,
    pub created: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeConfig {
    pub listen: String,
    pub tokens: Vec<NodeToken>,
}

pub fn config_path(data: &std::path::Path) -> PathBuf { data.join("node.json") }

pub fn load_config(data: &std::path::Path) -> Result<NodeConfig> {
    let p = config_path(data);
    let b = std::fs::read(&p).with_context(|| format!("нет {} — сначала выполните `hopebackup node init`", p.display()))?;
    Ok(serde_json::from_slice(&b)?)
}

/// Создать конфигурацию узла: токены администратора и агентов, сертификат.
pub fn init(data: &std::path::Path, listen: &str) -> Result<()> {
    if config_path(data).exists() { bail!("узел уже инициализирован: {}", config_path(data).display()); }
    std::fs::create_dir_all(data.join("repos"))?;
    let admin = crate::util::token();
    let append = crate::util::token();
    let now = chrono::Utc::now();
    let cfg = NodeConfig {
        listen: listen.to_string(),
        tokens: vec![
            NodeToken { name: "admin".into(), sha256: crate::util::sha256_hex(&admin), role: Role::Admin, repo: String::new(), created: now },
            NodeToken { name: "agents".into(), sha256: crate::util::sha256_hex(&append), role: Role::Append, repo: String::new(), created: now },
        ],
    };
    crate::util::write_json(&config_path(data), &cfg, true)?;
    let (cert, _) = crate::tlsutil::ensure_cert(data, &[])?;
    println!("Узел хранения инициализирован: {}", data.display());
    println!("  Адрес:                  https://{}:{}", crate::util::local_ips().first().cloned().unwrap_or_else(crate::util::hostname), listen.rsplit(':').next().unwrap_or("8443"));
    println!("  Отпечаток сертификата:  {}", crate::tlsutil::fingerprint_pem(&cert)?);
    println!("  Токен администратора:   {admin}");
    println!("  Токен агентов (append): {append}");
    println!("Сохраните токены: на узле хранятся только их хэши.");
    Ok(())
}

/// Узел внутри сервера управления (площадка «диск сервера»): конфигурация в `data`, токены для
/// сервера — администратор и агенты. Существующий каталог узла подхватывается (токены перевыпускаются).
pub fn init_embedded(data: &std::path::Path, site: &str) -> Result<(String, String)> {
    std::fs::create_dir_all(data.join("repos")).with_context(|| format!("создание {}", data.display()))?;
    if !config_path(data).exists() {
        crate::util::write_json(&config_path(data), &NodeConfig { listen: String::new(), tokens: vec![] }, true)?;
    }
    let admin = add_token(data, &format!("server-admin-{site}"), Role::Admin, "")?;
    let append = add_token(data, &format!("server-agents-{site}"), Role::Append, "")?;
    Ok((admin, append))
}

pub fn add_token(data: &std::path::Path, name: &str, role: Role, repo: &str) -> Result<String> {
    let mut cfg = load_config(data)?;
    let t = crate::util::token();
    cfg.tokens.retain(|x| x.name != name);
    cfg.tokens.push(NodeToken { name: name.into(), sha256: crate::util::sha256_hex(&t), role, repo: repo.into(), created: chrono::Utc::now() });
    crate::util::write_json(&config_path(data), &cfg, true)?;
    Ok(t)
}

struct NodeState {
    data: PathBuf,
    cfg: RwLock<NodeConfig>,
    repos: RwLock<HashMap<String, LocalBackend>>,
}

impl NodeState {
    fn repo(&self, name: &str) -> Result<LocalBackend, ApiErr> {
        if !(1..=64).contains(&name.len()) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(ApiErr(StatusCode::BAD_REQUEST, "недопустимое имя репозитория".into()));
        }
        if let Some(r) = self.repos.read().get(name) { return Ok(r.clone()); }
        let b = LocalBackend::open(self.data.join("repos").join(name)).map_err(ApiErr::from)?;
        crate::util::trim_memory();
        self.repos.write().insert(name.to_string(), b.clone());
        Ok(b)
    }

    fn auth(&self, h: &HeaderMap, repo: &str, need: Role) -> Result<(), ApiErr> {
        let tok = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if tok.is_empty() { return Err(ApiErr(StatusCode::UNAUTHORIZED, "нужен токен".into())); }
        let hash = crate::util::sha256_hex(tok);
        let cfg = self.cfg.read();
        let Some(t) = cfg.tokens.iter().find(|t| crate::util::ct_eq(&t.sha256, &hash)) else {
            return Err(ApiErr(StatusCode::UNAUTHORIZED, "неверный токен".into()));
        };
        if !t.repo.is_empty() && t.repo != repo { return Err(ApiErr(StatusCode::FORBIDDEN, "токен не для этого репозитория".into())); }
        if t.role < need { return Err(ApiErr(StatusCode::FORBIDDEN, format!("токену «{}» не хватает прав", t.name))); }
        Ok(())
    }
}

pub struct ApiErr(StatusCode, String);

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        let code = if e.downcast_ref::<NotFound>().is_some() {
            StatusCode::NOT_FOUND
        } else if e.downcast_ref::<Conflict>().is_some() {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiErr(code, format!("{e:#}"))
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response { (self.0, self.1).into_response() }
}

type S = State<Arc<NodeState>>;
type R<T> = Result<T, ApiErr>;

/// Страница для браузера на «/»: у узла нет веб-интерфейса — объяснить это и, если на этой же машине работает
/// сервер управления (порт 4443), дать ссылку на его панель. Ничего секретного: версия и так видна в /v1/health.
async fn index(State(s): S, uri: axum::http::Uri, h: HeaderMap) -> Response {
    // HTTP/2 (браузеры по TLS) передаёт адрес в :authority, HTTP/1.1 — в заголовке Host.
    let authority = uri.authority().map(|a| a.as_str().to_string())
        .or_else(|| h.get("host").and_then(|v| v.to_str().ok()).map(str::to_string)).unwrap_or_default();
    let (host, port) = host_port(&authority);
    let listen = s.cfg.read().listen.clone();
    let port = port.unwrap_or_else(|| listen.rsplit(':').next().unwrap_or("8443").to_string());
    let host = host.unwrap_or_else(|| crate::util::local_ips().first().cloned().unwrap_or_else(crate::util::hostname));
    let panel_here = tokio::time::timeout(std::time::Duration::from_millis(300), tokio::net::TcpStream::connect(("127.0.0.1", 4443)))
        .await.map(|r| r.is_ok()).unwrap_or(false);
    let panel = if panel_here {
        format!(r#"<a class="btn" href="https://{h}:4443/">Открыть панель управления <span aria-hidden="true">→</span></a>"#, h = esc(&host))
    } else {
        r#"<div class="hint">Панель управления открывается на сервере Ковчега — обычно <code>https://адрес-сервера:4443</code>.</div>"#.to_string()
    };
    let html = include_str!("page.html")
        .replace("{{VERSION}}", &esc(crate::VERSION))
        .replace("{{SELF}}", &esc(&format!("https://{host}:{port}")))
        .replace("{{PANEL}}", &panel);
    (
        [
            ("content-type", "text/html; charset=utf-8"),
            ("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ],
        html,
    )
        .into_response()
}

/// Хост и порт из заголовка Host — только если они похожи на имя/адрес (заголовок присылает клиент).
fn host_port(raw: &str) -> (Option<String>, Option<String>) {
    let raw = raw.trim();
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, p)) => (format!("[{h}]"), p.strip_prefix(':').map(str::to_string)),
            None => return (None, None),
        }
    } else {
        match raw.matches(':').count() {
            0 => (raw.to_string(), None),
            1 => { let (h, p) = raw.split_once(':').unwrap(); (h.to_string(), Some(p.to_string())) }
            _ => return (None, None),
        }
    };
    let host_ok = !host.is_empty() && host.len() <= 253 && host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c));
    let port_ok = |p: &String| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit());
    if !host_ok { return (None, None); }
    (Some(host), port.filter(port_ok))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Место на диске каталога данных узла — для карточки площадки на сервере управления.
async fn disk(State(s): S) -> Json<serde_json::Value> {
    let (total, used) = crate::plugins::host::inventory::statvfs(&s.data.to_string_lossy());
    Json(serde_json::json!({ "total": total, "used": used, "free": total.saturating_sub(used) }))
}

async fn health() -> impl IntoResponse { Json(serde_json::json!({ "ok": true, "version": crate::VERSION, "role": "node" })) }

async fn config_get(State(s): S, Path(repo): Path<String>, h: HeaderMap) -> R<Response> {
    s.auth(&h, &repo, Role::Read)?;
    match s.repo(&repo)?.read_config().await? {
        Some(c) => Ok(Json(c).into_response()),
        None => Ok((StatusCode::NOT_FOUND, "репозиторий не создан").into_response()),
    }
}

async fn config_put(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(c): Json<RepoConfig>) -> R<StatusCode> {
    s.auth(&h, &repo, Role::Append)?;
    s.repo(&repo)?.write_config(&c).await?;
    tracing::info!("создан репозиторий {repo}");
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
struct MissingReq {
    ids: Vec<ChunkId>,
}

async fn missing(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(q): Json<MissingReq>) -> R<Json<serde_json::Value>> {
    s.auth(&h, &repo, Role::Append)?;
    if q.ids.len() > 100_000 { return Err(ApiErr(StatusCode::BAD_REQUEST, "слишком много идентификаторов".into())); }
    let m = s.repo(&repo)?.missing(q.ids).await?;
    Ok(Json(serde_json::json!({ "missing": m })))
}

async fn pack_put(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap, body: Bytes) -> R<StatusCode> {
    s.auth(&h, &repo, Role::Append)?;
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) { return Err(ApiErr(StatusCode::BAD_REQUEST, "неверный идентификатор pack-файла".into())); }
    if body.len() < 4 { return Err(ApiErr(StatusCode::BAD_REQUEST, "пустое тело".into())); }
    let n = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
    if body.len() < 4 + n { return Err(ApiErr(StatusCode::BAD_REQUEST, "обрезанное тело".into())); }
    let entries: Vec<IndexEntry> = serde_json::from_slice(&body[4..4 + n]).map_err(|e| ApiErr(StatusCode::BAD_REQUEST, e.to_string()))?;
    s.repo(&repo)?.put_pack(&id, body.slice(4 + n..), entries).await?;
    Ok(StatusCode::CREATED)
}

async fn blob_get(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap) -> R<Vec<u8>> {
    s.auth(&h, &repo, Role::Read)?;
    let id = ChunkId::from_hex(&id)?;
    Ok(s.repo(&repo)?.get_blob(&id).await?)
}

#[derive(serde::Deserialize)]
struct BlobsReq { ids: Vec<String> }

/// Несколько блоков одним запросом (репликация через медленные каналы): ответ — подряд «длина (u32, BE) + блок»
/// в порядке запроса; блоков может быть меньше, чем запрошено (ответ ограничен ~32 МиБ) — остальные клиент
/// запрашивает снова. С 0.9.5.
async fn blobs_get(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(q): Json<BlobsReq>) -> R<Vec<u8>> {
    s.auth(&h, &repo, Role::Read)?;
    if q.ids.len() > crate::store::types::BLOBS_MAX_IDS { return Err(ApiErr(StatusCode::BAD_REQUEST, format!("не больше {} блоков в запросе", crate::store::types::BLOBS_MAX_IDS))); }
    let ids = q.ids.iter().map(|x| ChunkId::from_hex(x)).collect::<Result<Vec<_>>>()?;
    let blobs = s.repo(&repo)?.get_blobs(&ids).await?;
    let mut body = Vec::with_capacity(blobs.iter().map(|b| b.len() + 4).sum());
    for b in blobs { body.extend_from_slice(&(b.len() as u32).to_be_bytes()); body.extend_from_slice(&b); }
    Ok(body)
}

/// Индекс pack-файла (какие блоки в нём) без данных: репликация проверяет, нужен ли файл принимающей площадке.
async fn pack_index(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap) -> R<Json<Vec<IndexEntry>>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.pack_index(&id).await?))
}

/// Запись снапшота растёт с объёмом источника (≈95 МБ на 1 ТБ) — принимается потоком во временный
/// файл и проверяется разбором потоком, в память целиком не читается.
async fn snapshot_put(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap, body: axum::body::Body) -> R<StatusCode> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    s.auth(&h, &repo, Role::Append)?;
    let r = s.repo(&repo)?;
    let tmp = r.tmp_file();
    let res: Result<SnapshotHeader> = async {
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut stream = body.into_data_stream();
        let mut n = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow::anyhow!("приём снапшота: {e}"))?;
            n += chunk.len() as u64;
            if n > SNAPSHOT_MAX { bail!("запись снапшота больше {}", crate::util::human_bytes(SNAPSHOT_MAX)); }
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        drop(f);
        r.put_snapshot_file(tmp.clone(), id.clone()).await
    }
    .await;
    let hd = match res {
        Ok(hd) => hd,
        Err(e) => { let _ = tokio::fs::remove_file(&tmp).await; return Err(e.into()); }
    };
    tracing::info!("{repo}: снапшот {id} ({}, {}, {})", hd.instance, hd.kind, crate::util::human_bytes(hd.size));
    Ok(StatusCode::CREATED)
}

const SNAPSHOT_MAX: u64 = 64 << 30;

/// Запись снапшота отдаётся потоком из файла.
async fn snapshot_get(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap) -> R<Response> {
    s.auth(&h, &repo, Role::Read)?;
    let p = s.repo(&repo)?.snapshot_file(&id)?;
    let f = tokio::fs::File::open(&p).await.map_err(|e| ApiErr::from(anyhow::Error::from(e)))?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f, 1 << 16));
    Ok(([(axum::http::header::CONTENT_TYPE, "application/json".to_string()), (axum::http::header::CONTENT_LENGTH, len.to_string())], body).into_response())
}

#[derive(Deserialize)]
struct ListQ {
    instance: Option<String>,
}

async fn snapshots(State(s): S, Path(repo): Path<String>, h: HeaderMap, Query(q): Query<ListQ>) -> R<Json<Vec<SnapshotHeader>>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.list_snapshots(q.instance.as_deref()).await?))
}

fn split_obj(path: &str) -> R<(String, String)> {
    let (ns, name) = path.rsplit_once('/').ok_or_else(|| ApiErr(StatusCode::BAD_REQUEST, "путь объекта: <пространство>/<имя>".into()))?;
    Ok((ns.to_string(), name.to_string()))
}

async fn obj_put(State(s): S, Path((repo, path)): Path<(String, String)>, h: HeaderMap, body: Bytes) -> R<StatusCode> {
    s.auth(&h, &repo, Role::Append)?;
    let (ns, name) = split_obj(&path)?;
    s.repo(&repo)?.put_object(&ns, &name, body.to_vec()).await?;
    Ok(StatusCode::CREATED)
}

async fn obj_get(State(s): S, Path((repo, path)): Path<(String, String)>, h: HeaderMap) -> R<Response> {
    s.auth(&h, &repo, Role::Read)?;
    let (ns, name) = split_obj(&path)?;
    match s.repo(&repo)?.get_object(&ns, &name).await? {
        Some(b) => Ok(b.into_response()),
        None => Ok((StatusCode::NOT_FOUND, "нет объекта").into_response()),
    }
}

#[derive(Deserialize)]
struct ObjsQ {
    ns: String,
    #[serde(default)]
    prefix: String,
}

async fn objs(State(s): S, Path(repo): Path<String>, h: HeaderMap, Query(q): Query<ObjsQ>) -> R<Json<Vec<ObjInfo>>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.list_objects(&q.ns, &q.prefix).await?))
}

async fn stats(State(s): S, Path(repo): Path<String>, h: HeaderMap) -> R<Json<RepoStats>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.stats().await?))
}

async fn prune(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(p): Json<PrunePolicy>) -> R<Json<PruneReport>> {
    s.auth(&h, &repo, Role::Admin)?;
    let r = s.repo(&repo)?.prune(&p).await;
    crate::util::trim_memory();
    let r = r?;
    tracing::info!("{repo}: очистка — снапшотов {}, pack-файлов {}, освобождено {}, WAL {}", r.snapshots_removed.len(), r.packs_removed, crate::util::human_bytes(r.bytes_freed), r.wal_removed);
    Ok(Json(r))
}

async fn packs(State(s): S, Path(repo): Path<String>, h: HeaderMap) -> R<Json<Vec<PackInfo>>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.list_packs().await?))
}

/// pack-файл целиком для репликации: u32 BE длины JSON индекса + индекс + данные (как при записи).
async fn pack_get(State(s): S, Path((repo, id)): Path<(String, String)>, h: HeaderMap) -> R<Vec<u8>> {
    s.auth(&h, &repo, Role::Read)?;
    let (entries, data) = s.repo(&repo)?.get_pack(&id).await?;
    let ej = serde_json::to_vec(&entries).map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut body = Vec::with_capacity(4 + ej.len() + data.len());
    body.extend_from_slice(&(ej.len() as u32).to_be_bytes());
    body.extend_from_slice(&ej);
    body.extend_from_slice(&data);
    Ok(body)
}

async fn allobjs(State(s): S, Path(repo): Path<String>, h: HeaderMap) -> R<Json<Vec<NsObj>>> {
    s.auth(&h, &repo, Role::Read)?;
    Ok(Json(s.repo(&repo)?.list_all_objects().await?))
}

// ---------------------------------------------------------------- готовые образы (qcow2) — файлами рядом с репозиторием
//
// Продукт «Образы qcow2» на сервере управления собирает файлы образов и отправляет их сюда только изменениями
// (см. plugins::image::filesync). Файлы лежат открыто (их можно сразу подключить к гипервизору площадки):
// <данные узла>/images/<репозиторий>/<экземпляр>/<набор ГГГГММДДTччммссZ>/<диск>.qcow2. Пока набор не
// принят целиком, он лежит в .part-<набор>. Записывать — токен append; удалять старые наборы — только admin.

use crate::plugins::image::filesync;

fn img_root(s: &NodeState, repo: &str) -> R<PathBuf> {
    if !(1..=64).contains(&repo.len()) || !repo.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "недопустимое имя репозитория".into()));
    }
    Ok(s.data.join("images").join(repo))
}

fn img_file_ok(f: &str) -> bool { filesync::valid_name(f) && [".qcow2", ".vmdk", ".vhd", ".img"].iter().any(|e| f.ends_with(e)) }

/// Каталог незаконченного набора и файл в нём (с проверкой имён).
fn img_part(s: &NodeState, repo: &str, inst: &str, set: &str, file: &str) -> R<(PathBuf, PathBuf)> {
    if !filesync::valid_name(inst) || !crate::plugins::image::formats::is_set_name(set) || !img_file_ok(file) {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "недопустимое имя экземпляра, набора или файла образа".into()));
    }
    let dir = img_root(s, repo)?.join(inst).join(format!(".part-{set}"));
    let f = dir.join(file);
    Ok((dir, f))
}

fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> impl std::future::Future<Output = R<T>> {
    async move { tokio::task::spawn_blocking(f).await.map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(ApiErr::from) }
}

/// Наборы образов репозитория: экземпляр, набор, файлы (размер и занято на диске), готов ли.
async fn images_list(State(s): S, Path(repo): Path<String>, h: HeaderMap) -> R<Json<serde_json::Value>> {
    s.auth(&h, &repo, Role::Read)?;
    let root = img_root(&s, &repo)?;
    let r2 = root.clone();
    let sets = blocking(move || {
        use std::os::unix::fs::MetadataExt;
        let mut out = Vec::new();
        let Ok(insts) = std::fs::read_dir(&r2) else { return Ok(out) };
        for i in insts.flatten() {
            let inst = i.file_name().to_string_lossy().to_string();
            let Ok(sets) = std::fs::read_dir(i.path()) else { continue };
            for d in sets.flatten() {
                let n = d.file_name().to_string_lossy().to_string();
                let (set, complete) = match n.strip_prefix(".part-") { Some(x) => (x.to_string(), false), None => (n.clone(), true) };
                if !crate::plugins::image::formats::is_set_name(&set) { continue; }
                let mut files = Vec::new();
                for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                    if let Ok(m) = f.metadata() { files.push(serde_json::json!({ "name": f.file_name().to_string_lossy(), "size": m.len(), "used": m.blocks() * 512 })); }
                }
                out.push(serde_json::json!({ "instance": inst, "set": set, "complete": complete, "files": files, "path": d.path() }));
            }
        }
        Ok(out)
    }).await?;
    Ok(Json(serde_json::json!({ "root": root, "sets": sets })))
}

#[derive(Deserialize)]
struct ImgBegin { instance: String, set: String, file: String, size: u64, #[serde(default)] base: String }

/// Начать (или продолжить) приём файла: новый — копией файла из прошлого набора (разрежённо), иначе пустым.
async fn images_begin(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(q): Json<ImgBegin>) -> R<Json<serde_json::Value>> {
    s.auth(&h, &repo, Role::Append)?;
    let (dir, f) = img_part(&s, &repo, &q.instance, &q.set, &q.file)?;
    let base = if !q.base.is_empty() && crate::plugins::image::formats::is_set_name(&q.base) && q.base != q.set {
        Some(img_root(&s, &repo)?.join(&q.instance).join(&q.base).join(&q.file)).filter(|p| p.is_file())
    } else { None };
    let size = q.size;
    let (resumed, from_base) = blocking(move || {
        std::fs::create_dir_all(&dir)?;
        if std::fs::metadata(&f).map(|m| m.len() == size).unwrap_or(false) { return Ok((true, false)); }
        let from = match &base { Some(b) => { filesync::copy_sparse(b, &f)?; true } None => { std::fs::File::create(&f)?; false } };
        std::fs::OpenOptions::new().write(true).open(&f)?.set_len(size)?;
        Ok((false, from))
    }).await?;
    Ok(Json(serde_json::json!({ "resumed": resumed, "from_base": from_base })))
}

#[derive(Deserialize)]
struct ImgQ { instance: String, set: String, file: String, #[serde(default)] offset: u64, #[serde(default)] raw: u64 }

/// Суммы участков принимаемого файла (по 16 байт на 4 МиБ).
async fn images_sums(State(s): S, Path(repo): Path<String>, h: HeaderMap, Query(q): Query<ImgQ>) -> R<Vec<u8>> {
    s.auth(&h, &repo, Role::Append)?;
    let (_, f) = img_part(&s, &repo, &q.instance, &q.set, &q.file)?;
    blocking(move || Ok(filesync::sums_bytes(&filesync::sums(&f)?))).await
}

/// Участок файла: тело — данные zstd, `raw` — их открытая длина.
async fn images_chunk(State(s): S, Path(repo): Path<String>, h: HeaderMap, Query(q): Query<ImgQ>, body: Bytes) -> R<StatusCode> {
    s.auth(&h, &repo, Role::Append)?;
    let (_, f) = img_part(&s, &repo, &q.instance, &q.set, &q.file)?;
    if q.raw == 0 || q.raw > filesync::CHUNK { return Err(ApiErr(StatusCode::BAD_REQUEST, "длина участка".into())); }
    let (off, raw) = (q.offset, q.raw);
    blocking(move || {
        use std::os::unix::fs::FileExt;
        let data = zstd::bulk::decompress(&body, raw as usize)?;
        if data.len() as u64 != raw { anyhow::bail!("участок: {} байт вместо {raw}", data.len()); }
        let file = std::fs::OpenOptions::new().write(true).open(&f)?;
        if off + raw > file.metadata()?.len() { anyhow::bail!("участок за концом файла"); }
        file.write_all_at(&data, off)?;
        Ok(())
    }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ImgZero { instance: String, set: String, file: String, offset: u64, len: u64 }

async fn images_zero(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(q): Json<ImgZero>) -> R<StatusCode> {
    s.auth(&h, &repo, Role::Append)?;
    let (_, f) = img_part(&s, &repo, &q.instance, &q.set, &q.file)?;
    blocking(move || { let file = std::fs::OpenOptions::new().write(true).open(&f)?; filesync::zero_range(&file, q.offset, q.len) }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ImgFinish { instance: String, set: String, files: Vec<String>, #[serde(default)] keep: usize }

/// Набор принят: .part-<набор> становится <набор>. С токеном admin — удалить наборы сверх `keep`.
async fn images_finish(State(s): S, Path(repo): Path<String>, h: HeaderMap, Json(q): Json<ImgFinish>) -> R<Json<serde_json::Value>> {
    s.auth(&h, &repo, Role::Append)?;
    let admin = s.auth(&h, &repo, Role::Admin).is_ok();
    let mut part = None;
    for f in &q.files { let (d, p) = img_part(&s, &repo, &q.instance, &q.set, f)?; if !p.is_file() { return Err(ApiErr(StatusCode::BAD_REQUEST, format!("файл {f} набора не принят"))); } part = Some(d); }
    let part = part.ok_or_else(|| ApiErr(StatusCode::BAD_REQUEST, "в наборе нет файлов".into()))?;
    let inst_dir = img_root(&s, &repo)?.join(&q.instance);
    let (set, keep) = (q.set.clone(), q.keep);
    let removed = blocking(move || {
        let fin = inst_dir.join(&set);
        if fin.exists() { std::fs::remove_dir_all(&fin)?; }
        std::fs::rename(&part, &fin)?;
        let mut removed = Vec::new();
        if admin && keep > 0 {
            let mut sets: Vec<String> = std::fs::read_dir(&inst_dir)?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| crate::plugins::image::formats::is_set_name(n)).collect();
            sets.sort();
            if sets.len() > keep { for n in &sets[..sets.len() - keep] { if std::fs::remove_dir_all(inst_dir.join(n)).is_ok() { removed.push(n.clone()); } } }
        }
        Ok(removed)
    }).await?;
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed, "pruned": admin })))
}

/// Токен проверяется до чтения тела запроса: без него узел не принимает ни байта (pack-файлы до 1 ГБ).
async fn token_gate(State(s): S, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if req.uri().path() == "/v1/health" { return next.run(req).await; }
    let tok = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let ok = !tok.is_empty() && { let h = crate::util::sha256_hex(tok); s.cfg.read().tokens.iter().any(|t| crate::util::ct_eq(&t.sha256, &h)) };
    if !ok { return (StatusCode::UNAUTHORIZED, "нужен действительный токен").into_response(); }
    next.run(req).await
}

pub fn router(data: PathBuf, cfg: NodeConfig) -> Router {
    let st = Arc::new(NodeState { data, cfg: RwLock::new(cfg), repos: RwLock::new(HashMap::new()) });
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/disk", get(disk))
        .route("/v1/r/:repo/config", get(config_get).put(config_put))
        .route("/v1/r/:repo/missing", post(missing))
        .route("/v1/r/:repo/pack/:id", get(pack_get).put(pack_put))
        .route("/v1/r/:repo/packs", get(packs))
        .route("/v1/r/:repo/allobjs", get(allobjs))
        .route("/v1/r/:repo/blob/:id", get(blob_get))
        .route("/v1/r/:repo/blobs", post(blobs_get))
        .route("/v1/r/:repo/pack/:id/index", get(pack_index))
        .route("/v1/r/:repo/snapshot/:id", get(snapshot_get).put(snapshot_put))
        .route("/v1/r/:repo/snapshots", get(snapshots))
        .route("/v1/r/:repo/obj/*path", get(obj_get).put(obj_put))
        .route("/v1/r/:repo/objs", get(objs))
        .route("/v1/r/:repo/stats", get(stats))
        .route("/v1/r/:repo/prune", post(prune))
        .route("/v1/r/:repo/images", get(images_list))
        .route("/v1/r/:repo/images/begin", post(images_begin))
        .route("/v1/r/:repo/images/sums", get(images_sums))
        .route("/v1/r/:repo/images/chunk", axum::routing::put(images_chunk))
        .route("/v1/r/:repo/images/zero", post(images_zero))
        .route("/v1/r/:repo/images/finish", post(images_finish))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .route_layer(axum::middleware::from_fn_with_state(st.clone(), token_gate))
        // После route_layer: страница для браузера доступна без токена.
        .route("/", get(index))
        .with_state(st)
}

/// Запустить узел (HTTPS с самоподписанным сертификатом; `plain` — HTTP для стендов и тестов).
pub async fn run(data: PathBuf, plain: bool) -> Result<()> {
    let cfg = load_config(&data)?;
    let addr: std::net::SocketAddr = cfg.listen.parse().with_context(|| format!("адрес {}", cfg.listen))?;
    let app = router(data.clone(), cfg);
    if plain {
        tracing::warn!("узел слушает {addr} без TLS — только для стенда");
        let l = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(l, app).await?;
        return Ok(());
    }
    let (cert, key) = crate::tlsutil::ensure_cert(&data, &[])?;
    tracing::info!("узел хранения: https://{addr}, отпечаток {}", crate::tlsutil::fingerprint_pem(&cert)?);
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await?;
    axum_server::bind_rustls(addr, tls).serve(app.into_make_service()).await?;
    Ok(())
}

#[cfg(test)]
mod page_tests {
    use super::*;
    #[test]
    fn host_header_parsing() {
        assert_eq!(host_port("10.77.3.101:8443"), (Some("10.77.3.101".into()), Some("8443".into())));
        assert_eq!(host_port("node.example"), (Some("node.example".into()), None));
        assert_eq!(host_port("[fe80::1]:8443"), (Some("[fe80::1]".into()), Some("8443".into())));
        assert_eq!(host_port("x\"><script>:1"), (None, None));
        assert_eq!(host_port("a:b:c"), (None, None));
        assert_eq!(host_port("h:80x"), (Some("h".into()), None));
        assert_eq!(esc("<a href=\"x\">'"), "&lt;a href=&quot;x&quot;&gt;&#39;");
    }
}
