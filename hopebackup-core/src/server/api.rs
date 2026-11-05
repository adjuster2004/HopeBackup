//! REST API панели (контракт — docs/API.md).

use anyhow::anyhow;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::auth::{self, Who};
use super::state::*;
use super::Server;
use crate::proto::JobSpec;

pub struct ApiError(pub StatusCode, pub String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self { ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response { (self.0, Json(json!({ "error": self.1 }))).into_response() }
}

pub type R<T> = Result<T, ApiError>;
type S = State<Arc<Server>>;

pub fn bad(m: impl Into<String>) -> ApiError { ApiError(StatusCode::BAD_REQUEST, m.into()) }
pub(crate) fn not_found(m: impl Into<String>) -> ApiError { ApiError(StatusCode::NOT_FOUND, m.into()) }

pub const MASK: &str = "********";

fn is_secret_key(k: &str) -> bool { crate::connectors::SECRET_KEYS.contains(&k) || k.ends_with("_token") || k.ends_with("secret") || k == "token" }

/// Скрыть секреты в параметрах (на любой глубине: вложенные объекты и массивы).
pub(crate) fn mask(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| {
            let nv = if is_secret_key(k) {
                match x {
                    Value::String(s) if !s.is_empty() => json!(MASK),
                    Value::String(_) | Value::Null => json!(""),
                    _ => json!(MASK),
                }
            } else {
                mask(x)
            };
            (k.clone(), nv)
        }).collect()),
        Value::Array(a) => Value::Array(a.iter().map(mask).collect()),
        o => o.clone(),
    }
}

/// Вернуть секреты, которые панель прислала замаскированными или пустыми (на любой глубине).
pub(crate) fn unmask(new: &mut Value, old: &Value) {
    if let (Some(n), Some(o)) = (new.as_object_mut(), old.as_object()) {
        for (k, v) in n.iter_mut() {
            if is_secret_key(k) && (v.as_str() == Some(MASK) || v.as_str() == Some("")) {
                if let Some(ov) = o.get(k) { *v = ov.clone(); }
            } else if is_secret_key(k) && v.is_null() {
                // null — панель просит удалить заданный секрет.
                *v = json!("");
            } else if let Some(ov) = o.get(k) {
                unmask(v, ov);
            }
        }
    }
}

pub(crate) fn slug(s: &str) -> String {
    let tr = |c: char| -> &str {
        match c {
            'а' => "a", 'б' => "b", 'в' => "v", 'г' => "g", 'д' => "d", 'е' | 'ё' => "e", 'ж' => "zh", 'з' => "z", 'и' | 'й' => "i", 'к' => "k", 'л' => "l", 'м' => "m",
            'н' => "n", 'о' => "o", 'п' => "p", 'р' => "r", 'с' => "s", 'т' => "t", 'у' => "u", 'ф' => "f", 'х' => "h", 'ц' => "c", 'ч' => "ch", 'ш' => "sh", 'щ' => "sch",
            'ы' => "y", 'э' => "e", 'ю' => "yu", 'я' => "ya", _ => "",
        }
    };
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() { out.push(c) } else if !tr(c).is_empty() { out.push_str(tr(c)) } else if !out.ends_with('-') { out.push('-') }
    }
    let out = out.trim_matches('-').to_string();
    let out: String = out.chars().take(40).collect();
    if out.is_empty() { crate::util::short_id() } else { out }
}

// ---------------------------------------------------------------- вход

#[derive(Deserialize)]
pub struct LoginReq {
    user: String,
    password: String,
}

pub async fn login(State(s): S, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, Json(q): Json<LoginReq>) -> R<Json<Value>> {
    let u = s.store.read(|st| st.users.iter().find(|u| u.name.eq_ignore_ascii_case(&q.user)).cloned());
    let ip = peer.ip().to_string();
    match u {
        Some(u) if auth::verify_password(&q.password, &u.pw_hash) => {
            let secret = s.store.read(|st| st.session_secret.clone());
            s.audit(&u.name, "login", format!("вход в панель с {ip}"));
            Ok(Json(json!({ "token": auth::issue(&secret, &u.name, u.session_gen), "user": u.name, "role": u.role })))
        }
        _ => {
            s.audit(&q.user, "login_failed", format!("неверный пароль, адрес {ip}"));
            super::notify::login_failed(q.user.chars().take(64).collect::<String>().as_str(), &ip);
            Err(ApiError(StatusCode::UNAUTHORIZED, "неверное имя пользователя или пароль".into()))
        }
    }
}

pub async fn me(State(_s): S, Extension(w): Extension<Who>) -> Json<Value> {
    Json(json!({ "user": w.user, "role": w.role, "perms": auth::perms(&w.role), "version": crate::VERSION, "hostname": crate::util::hostname() }))
}

// ---------------------------------------------------------------- обзор и каталог

pub(crate) fn agent_online(a: &Agent) -> bool { a.last_seen.map(|t| (Utc::now() - t).num_seconds() < 90).unwrap_or(false) }

/// Состояние экземпляра для панели: последняя копия, RPO, WAL, следующий запуск.
pub(crate) fn instance_view(s: &Server, i: &Instance, jobs: &[super::jobs::Job]) -> Value {
    let mine: Vec<&super::jobs::Job> = jobs.iter().filter(|j| j.instance_id.as_deref() == Some(i.id.as_str())).collect();
    let last_ok = mine.iter().find(|j| j.kind == "backup" && j.status == "ok" && j.result.get("snapshot").is_some());
    let last_job = mine.iter().find(|j| j.kind == "backup");
    let last_backup = last_ok.map(|j| {
        let h = &j.result["snapshot"];
        json!({ "snapshot_id": h["id"], "time": h["time"], "kind": h["kind"], "size": h["size"], "new_bytes": h["new_bytes"], "uploaded": h["uploaded"] })
    });
    let wal = s.wal.lock().get(&i.id).cloned();
    // Сервер управления (продукты без агента) «в сети» всегда — он сам и выполняет задания.
    let online = i.agent_id == "server" || s.store.read(|st| st.agents.iter().find(|a| a.id == i.agent_id).map(agent_online).unwrap_or(false));
    let last_time = last_backup.as_ref().and_then(|b| b["time"].as_str()).and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
    let wal_healthy = wal.as_ref().map(|w| w.last_error.is_none() && w.queued <= 2 && online).unwrap_or(false);
    let rpo = if last_time.is_none() {
        None
    } else if wal_healthy {
        // Журнал уходит непрерывно: потеря ограничена очередью спула и периодом отправки.
        // Журнал уходит непрерывно: потеря ограничена archive_timeout (60 с) и очередью спула.
        Some(wal.as_ref().map(|w| 60 + w.queued as i64 * 5).unwrap_or(60))
    } else {
        let lt = [last_time, wal.as_ref().and_then(|w| w.last_time)].into_iter().flatten().max().unwrap();
        Some((Utc::now() - lt).num_seconds().max(0))
    };
    let target = (i.rpo_target_min * 60) as i64;
    let failed = last_job.map(|j| j.status == "failed").unwrap_or(false);
    let status = if last_backup.is_none() {
        if failed { "error" } else { "never" }
    } else if failed || wal.as_ref().map(|w| w.last_error.is_some()).unwrap_or(false) {
        "error"
    } else if rpo.map(|r| r > target.max(60) * if i.plugin == "postgresql" { 1 } else { 96 }).unwrap_or(false) || !online {
        "warn"
    } else {
        "ok"
    };
    let next = [i.schedule.full.as_deref(), i.schedule.dump.as_deref(), i.schedule.incr.as_deref()].into_iter().flatten().filter(|e| i.schedule.enabled && !e.is_empty()).filter_map(super::next_run).min();
    // Предупреждения последней удачной копии (строки журнала «внимание: …») — на обзор экземпляра.
    let mut warnings: Vec<String> = Vec::new();
    if let Some(j) = last_job.filter(|j| j.status == "ok") {
        for l in &j.log {
            if let Some(w) = l.m.trim_start().strip_prefix("внимание: ") {
                let w = w.trim().to_string();
                if !warnings.contains(&w) && warnings.len() < 10 { warnings.push(w); }
            }
        }
    }
    let mut v = serde_json::to_value(i).unwrap_or(Value::Null);
    v["config"] = mask(&i.config);
    v["runtime"] = json!({
        "status": status,
        "last_backup": last_backup,
        "last_job": last_job.map(|j| json!({ "id": j.id, "status": j.status, "finished": j.finished, "error": j.error })),
        "rpo_seconds": rpo,
        "wal": wal,
        "next_run": next,
        "agent_online": online,
        "warnings": warnings,
    });
    v
}

pub async fn status(State(s): S) -> Json<Value> {
    let st = s.store.get();
    let jobs = s.jobs.all();
    let day = Utc::now() - chrono::Duration::hours(24);
    let recent: Vec<&super::jobs::Job> = jobs.iter().filter(|j| j.created > day).collect();
    let (mut read, mut new, mut up) = (0u64, 0u64, 0u64);
    for j in recent.iter().filter(|j| j.kind == "backup" && j.status == "ok") {
        read += j.result["stats"]["bytes_read"].as_u64().unwrap_or(0);
        new += j.result["stats"]["new_bytes"].as_u64().unwrap_or(0);
        up += j.result["stats"]["uploaded"].as_u64().unwrap_or(0);
    }
    let views: Vec<Value> = st.instances.iter().map(|i| instance_view(&s, i, &jobs)).collect();
    let worst = views.iter().filter_map(|v| v["runtime"]["rpo_seconds"].as_i64().map(|r| (r, v))).max_by_key(|(r, _)| *r).map(|(r, v)| json!({ "instance_id": v["id"], "name": v["name"], "seconds": r, "target_seconds": v["rpo_target_min"].as_i64().unwrap_or(15) * 60 }));
    let mut next: Vec<Value> = views.iter().filter_map(|v| v["runtime"]["next_run"].as_str().map(|t| json!({ "instance_id": v["id"], "name": v["name"], "kind": if v["plugin"] == "files" { "files" } else { "full" }, "at": t }))).collect();
    next.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
    next.truncate(8);
    // Ошибки на «Обзоре»: без скрытых вручную и без тех, после которых такое же задание экземпляра прошло успешно.
    let open: Vec<&&super::jobs::Job> = recent.iter().filter(|j| j.status == "failed" && j.ack.is_none() && !superseded(j, &jobs)).collect();
    // Повторы одного и того же (экземпляр, вид, режим) — одной строкой: последняя ошибка и сколько раз.
    let mut groups: Vec<(String, Vec<&super::jobs::Job>)> = Vec::new();
    for j in &open {
        let key = match j.instance_id.as_deref() { Some(i) => format!("{i}|{}|{}", j.kind, j.mode), None => j.id.clone() };
        match groups.iter_mut().find(|(k, _)| *k == key) { Some((_, v)) => v.push(j), None => groups.push((key, vec![j])) }
    }
    let failures: Vec<Value> = groups.iter().take(20).map(|(_, v)| {
        let j = v[0];
        json!({ "job_id": j.id, "title": j.title, "error": j.error, "finished": j.finished, "count": v.len(), "ids": v.iter().map(|x| x.id.clone()).collect::<Vec<_>>() })
    }).collect();
    Json(json!({
        "version": crate::VERSION,
        "hostname": crate::util::hostname(),
        "uptime": (Utc::now() - s.started).num_seconds(),
        "fingerprint": s.fingerprint,
        "counts": {
            "instances": st.instances.len(), "agents": st.agents.len(), "agents_online": st.agents.iter().filter(|a| agent_online(a)).count(),
            "sites": st.sites.len(), "connectors": st.connectors.len(), "targets": st.targets.len()
        },
        "jobs24": {
            "ok": recent.iter().filter(|j| j.status == "ok").count(),
            "failed": recent.iter().filter(|j| j.status == "failed").count(),
            "running": jobs.iter().filter(|j| j.status == "running" || j.status == "queued").count()
        },
        "transfer24": { "read": read, "new": new, "uploaded": up },
        "worst_rpo": worst,
        "recent_failures": failures,
        "recent_failures_total": groups.len(),
        "next_runs": next,
    }))
}

pub async fn catalog() -> Json<Value> {
    Json(json!({ "products": crate::plugins::catalog(), "connector_kinds": crate::connectors::kinds(), "target_kinds": super::vm::kinds() }))
}

// ---------------------------------------------------------------- экземпляры

pub async fn instances(State(s): S) -> Json<Vec<Value>> {
    let jobs = s.jobs.all();
    let list = s.store.read(|st| st.instances.clone());
    Json(list.iter().map(|i| instance_view(&s, i, &jobs)).collect())
}

pub async fn instance_get(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let i = s.instance(&id).map_err(|e| not_found(e.to_string()))?;
    Ok(Json(instance_view(&s, &i, &s.jobs.list(Some(&id), 200))))
}

#[derive(Deserialize)]
#[derive(Default)]
pub struct InstanceReq {
    pub name: String,
    #[serde(default)]
    pub catalog_id: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub site_id: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub retention: Option<Retention>,
    #[serde(default)]
    pub rpo_target_min: Option<u64>,
    /// Повторы: `inherit` — как в настройках сервера, `custom` — свои (`retry`); не задано — не менять.
    #[serde(default)]
    pub retry_mode: Option<String>,
    #[serde(default)]
    pub retry: Option<crate::server::state::RetryPolicy>,
}

/// Своя политика повторов экземпляра из запроса: Some(None) — общая, Some(Some(p)) — своя, None — не менять.
fn retry_of(q: &InstanceReq) -> R<Option<Option<crate::server::state::RetryPolicy>>> {
    match q.retry_mode.as_deref() {
        None | Some("") => Ok(None),
        Some("inherit") => Ok(Some(None)),
        Some("custom") => {
            let p = q.retry.clone().ok_or_else(|| bad("укажите число попыток и паузу"))?;
            p.validate().map_err(bad)?;
            Ok(Some(Some(p)))
        }
        Some(_) => Err(bad("повторы: inherit или custom")),
    }
}

fn validate_schedule(sch: &Schedule) -> R<()> {
    for e in [&sch.full, &sch.dump].into_iter().flatten() {
        if !e.trim().is_empty() && croner::Cron::new(e).parse().is_err() { return Err(bad(format!("неверное расписание cron: «{e}»"))); }
    }
    Ok(())
}

pub async fn instance_create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<InstanceReq>) -> R<Json<Value>> {
    let inst = create_instance(&s, &w.user, q)?;
    Ok(Json(instance_view(&s, &inst, &[])))
}

/// Создать экземпляр (из панели или из рекомендаций описи).
/// Продукты, которые может выполнять сам сервер управления (агент «server»): им нужен только доступ по SSH
/// к машине или репозиторий, а не программа на машине продукта.
pub const SERVER_RUN: &[&str] = &["machine", "qcow2images"];

/// Продукты, площадка которых всегда совпадает с площадкой источника (`config.source`).
pub const FOLLOWS_SOURCE: &[&str] = &["qcow2images"];

/// Площадка экземпляра-источника «Образ машины по SSH».
pub fn source_site_of(st: &super::state::State, config: &Value) -> anyhow::Result<String> {
    let src = config["source"].as_str().unwrap_or("");
    if src.is_empty() { return Err(anyhow!("выберите источник — экземпляр «Образ машины по SSH» (сначала создайте его и сделайте копию)")); }
    let i = st.instances.iter().find(|i| i.id == src).ok_or_else(|| anyhow!("источник «{src}» не найден"))?;
    if i.plugin != "machine" { return Err(anyhow!("источник — экземпляр «Образ машины по SSH»")); }
    Ok(i.site_id.clone())
}

/// Продукты, которые выполняет только сервер управления: сборка и конвертация образов — в его локальной папке.
pub const SERVER_ONLY: &[&str] = &["qcow2images"];

fn check_agent(st: &super::state::State, agent_id: &str, plugin: &str) -> anyhow::Result<()> {
    if SERVER_ONLY.contains(&plugin) && agent_id != "server" {
        return Err(anyhow!("«Образы qcow2» собирает и конвертирует сам сервер управления (в своей локальной папке) — агент для этого продукта не выбирается"));
    }
    if agent_id == "server" {
        if SERVER_RUN.contains(&plugin) { return Ok(()); }
        return Err(anyhow!("этот продукт выполняет агент на машине с продуктом — выберите агента"));
    }
    if !st.agents.iter().any(|a| a.id == agent_id) { return Err(anyhow!("выберите агента")); }
    Ok(())
}

pub fn create_instance(s: &Server, user: &str, q: InstanceReq) -> R<Instance> {
    let cat = crate::plugins::catalog().into_iter().find(|c| c.id == q.catalog_id).ok_or_else(|| bad("неизвестный продукт каталога"))?;
    if !cat.available { return Err(bad("этот продукт пока в разработке")); }
    if q.name.trim().is_empty() { return Err(bad("укажите имя экземпляра")); }
    let retry = retry_of(&q)?.flatten();
    let sch = q.schedule.unwrap_or(Schedule { enabled: true, full: Some("0 1 * * *".into()), dump: None, incr: None });
    validate_schedule(&sch)?;
    let mut config = cat.defaults.clone();
    if let (Some(c), Some(n)) = (config.as_object_mut(), q.config.as_object()) { for (k, v) in n { c.insert(k.clone(), v.clone()); } }
    let inst = s.store.update(|st| {
        check_agent(st, &q.agent_id, cat.plugin)?;
        // «Образы qcow2»: площадка — всегда та, где копии источника (оттуда берётся копия для сборки).
        let site_id = if FOLLOWS_SOURCE.contains(&cat.plugin) { source_site_of(st, &config)? } else { q.site_id.clone() };
        if !st.sites.iter().any(|x| x.id == site_id) { return Err(anyhow!("выберите площадку")); }
        let mut id = slug(&format!("{}-{}", cat.id, q.name));
        if st.instances.iter().any(|i| i.id == id) { id = format!("{id}-{}", &crate::util::short_id()[..4]); }
        let i = Instance {
            id,
            name: q.name.trim().to_string(),
            catalog_id: cat.id.to_string(),
            plugin: cat.plugin.to_string(),
            agent_id: q.agent_id.clone(),
            site_id,
            config,
            schedule: sch,
            retention: q.retention.unwrap_or_default(),
            rpo_target_min: q.rpo_target_min.unwrap_or(15),
            created: Utc::now(),
            retry: retry.clone(),
            replicate: Vec::new(),
            repl_set: false,
            site_repl: None,
        };
        st.instances.push(i.clone());
        Ok(i)
    })?;
    s.audit(user, "instance_create", format!("{} ({})", inst.name, inst.id));
    Ok(inst)
}

pub async fn instance_update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<InstanceReq>) -> R<Json<Value>> {
    if let Some(sch) = &q.schedule { validate_schedule(sch)?; }
    let retry = retry_of(&q)?;
    let inst = s.store.update(|st| {
        let i = st.instances.iter_mut().find(|i| i.id == id).ok_or_else(|| anyhow!("экземпляр не найден"))?;
        if !q.name.trim().is_empty() { i.name = q.name.trim().to_string(); }
        if !q.agent_id.is_empty() && q.agent_id != i.agent_id {
            let plugin = i.plugin.clone();
            let ok = q.agent_id == "server" && SERVER_RUN.contains(&plugin.as_str());
            if q.agent_id == "server" && !ok { return Err(anyhow!("этот продукт выполняет агент на машине с продуктом — выберите агента")); }
            if SERVER_ONLY.contains(&plugin.as_str()) && q.agent_id != "server" { return Err(anyhow!("«Образы qcow2» собирает и конвертирует только сервер управления")); }
            i.agent_id = q.agent_id.clone();
        }
        if !q.site_id.is_empty() { i.site_id = q.site_id.clone(); }
        if q.config.is_object() {
            let mut c = q.config.clone();
            unmask(&mut c, &i.config);
            i.config = c;
        }
        if let Some(sch) = &q.schedule { i.schedule = sch.clone(); }
        if let Some(r) = &q.retention { i.retention = r.clone(); }
        if let Some(r) = q.rpo_target_min { i.rpo_target_min = r; }
        if let Some(r) = &retry { i.retry = r.clone(); }
        let (plugin, cfg) = (i.plugin.clone(), i.config.clone());
        // «Образы qcow2»: площадка — всегда та же, что у источника.
        if FOLLOWS_SOURCE.contains(&plugin.as_str()) {
            let site = source_site_of(st, &cfg)?;
            if let Some(x) = st.instances.iter_mut().find(|x| x.id == id) { x.site_id = site; }
        }
        Ok(st.instances.iter().find(|x| x.id == id).cloned().ok_or_else(|| anyhow!("экземпляр не найден"))?)
    })?;
    s.audit(&w.user, "instance_update", inst.id.clone());
    Ok(Json(instance_view(&s, &inst, &s.jobs.list(Some(&id), 200))))
}

pub async fn instance_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    if let Some(m) = s.store.read(|st| st.migrations.iter().find(|m| m.instance_id == id).map(|m| m.name.clone())) { return Err(bad(format!("экземпляр — источник переезда «{m}»: удалите переезд на странице «Переезд»"))); }
    let users: Vec<String> = s.store.read(|st| st.instances.iter().filter(|i| FOLLOWS_SOURCE.contains(&i.plugin.as_str()) && i.config["source"] == id.as_str()).map(|i| format!("«{}»", i.name)).collect());
    if !users.is_empty() { return Err(bad(format!("из этого экземпляра собирают образы {} — сначала удалите их или выберите в них другой источник", users.join(", ")))); }
    s.store.update(|st| { st.instances.retain(|i| i.id != id); Ok(()) })?;
    s.audit(&w.user, "instance_delete", id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct BackupReq {
    #[serde(default)]
    kind: Option<String>,
    /// Повторять при ошибке по политике экземпляра (как копии по расписанию).
    #[serde(default)]
    retry: bool,
}

pub async fn instance_backup(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<BackupReq>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    let kind = q.kind.unwrap_or_else(|| if i.plugin == "files" { "files".into() } else { "full".into() });
    if !["full", "files", "dump", "incr"].contains(&kind.as_str()) { return Err(bad("вид копии: full, files, dump или incr")); }
    let job = s.enqueue_backup(&i, &kind, &w.user)?;
    s.jobs.update(&job, |j| j.origin = if q.retry { "manual_retry".into() } else { "manual".into() });
    s.audit(&w.user, "backup", format!("{} ({kind}{})", i.name, if q.retry { ", с повторами" } else { "" }));
    Ok(Json(json!({ "job_id": job })))
}

/// Отправить копии (у «Образов qcow2» — последний набор) на все включённые площадки репликации сейчас.
pub async fn instance_ship(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    let t = super::instrepl::targets(&i);
    if t.is_empty() { return Err(bad("площадки репликации не включены — закладка «Репликация»")); }
    let mut jobs = Vec::new();
    for x in t { if let Ok(j) = super::instrepl::enqueue(&s, &i, &x.site_id, &w.user) { jobs.push(j); } }
    let first = jobs.first().cloned().ok_or_else(|| bad("репликация уже идёт"))?;
    s.audit(&w.user, "replicate", format!("{}: {} площадок", i.name, jobs.len()));
    Ok(Json(json!({ "job_id": first, "jobs": jobs })))
}

/// Закладка «Репликация»: площадки, расписание и состояние по последнему заданию на каждую.
pub async fn replication_get(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    Ok(Json(super::instrepl::status(&s, &i)))
}

#[derive(Deserialize)]
pub struct ReplPut {
    targets: Vec<super::state::ReplTarget>,
    /// Передавать при репликации площадки целиком (не задано — не менять).
    #[serde(default)]
    site_repl: Option<bool>,
}

pub async fn replication_put(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<ReplPut>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    if i.plugin == "replica" { return Err(bad("реплика переезда передаётся на цель переезда — отдельная репликация не нужна")); }
    let mut out: Vec<super::state::ReplTarget> = Vec::new();
    for mut t in q.targets {
        let site = s.store.read(|st| st.sites.iter().find(|x| x.id == t.site_id).cloned()).ok_or_else(|| bad("площадка не найдена"))?;
        if out.iter().any(|x| x.site_id == t.site_id) { continue; }
        if t.site_id == i.site_id && i.plugin != "qcow2images" { return Err(bad(format!("«{}» — площадка самого экземпляра: копии и так там", site.name))); }
        if i.plugin == "qcow2images" && site.kind != "node" { return Err(bad(format!("«{}»: готовые qcow2 передаются только на узлы HopeBackup", site.name))); }
        if !["after_job", "cron"].contains(&t.when.as_str()) { t.when = "after_job".into(); }
        if t.when == "cron" {
            if t.cron.trim().is_empty() { return Err(bad(format!("«{}»: укажите расписание репликации (cron) или выберите «сразу после выполнения задания»", site.name))); }
            croner::Cron::new(t.cron.trim()).parse().map_err(|e| bad(format!("«{}»: расписание «{}»: {e}", site.name, t.cron)))?;
        }
        out.push(t);
    }
    s.store.update(|st| { if let Some(x) = st.instances.iter_mut().find(|x| x.id == id) { x.replicate = out.clone(); x.repl_set = true; if let Some(v) = q.site_repl { x.site_repl = Some(v); } } Ok(()) })?;
    let on: Vec<String> = out.iter().filter(|t| t.enabled).map(|t| format!("{} ({})", t.site_id, if t.when == "cron" { t.cron.as_str() } else { "после задания" })).collect();
    let site_part = match q.site_repl { Some(true) => "; при репликации площадки — передаётся", Some(false) => "; при репликации площадки — не передаётся", None => "" };
    s.audit(&w.user, "replication_set", format!("{}: {}{site_part}", i.name, if on.is_empty() { "выключена".to_string() } else { on.join(", ") }));
    Ok(Json(super::instrepl::status(&s, &s.instance(&id)?)))
}

#[derive(Deserialize)]
pub struct ReplNow { site_id: String }

pub async fn replicate_now(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<ReplNow>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    if q.site_id == i.site_id && i.plugin != "qcow2images" { return Err(bad("это площадка самого экземпляра")); }
    let job = super::instrepl::enqueue(&s, &i, &q.site_id, &w.user).map_err(|e| bad(format!("{e:#}")))?;
    s.audit(&w.user, "replicate", format!("{} → {}", i.name, q.site_id));
    Ok(Json(json!({ "job_id": job })))
}

pub async fn instance_discover(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    let spec = JobSpec { id: String::new(), kind: "discover".into(), instance: Some(s.runtime(&i)), site: None, snapshot_id: None, mode: String::new(), params: Value::Null };
    Ok(Json(json!({ "job_id": s.jobs.enqueue(spec, format!("{} — обнаружение", i.name), &i.agent_id, &w.user) })))
}

#[derive(Deserialize)]
pub struct ActionReq {
    name: String,
}

pub async fn instance_action(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<ActionReq>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    if q.name == "restart" {
        // Перезапуск СУБД оборвал бы идущую копию или восстановление этого экземпляра.
        if s.jobs.list(Some(&id), 50).iter().any(|j| j.status == "running" || j.status == "queued") {
            return Err(bad("у экземпляра выполняются задания — дождитесь их окончания и повторите перезапуск"));
        }
        // Старый агент не знает действия restart: объясняем, что делать, вместо «неизвестное действие».
        if let Some(a) = s.store.read(|st| st.agents.iter().find(|a| a.id == i.agent_id).cloned()) {
            if !a.caps.iter().any(|c| c == "pg-restart") {
                return Err(bad(format!("агент «{}» версии {} не умеет перезапускать PostgreSQL: обновите его до {} («Безопасность» → «Обновления» → таблица агентов → «Обновить») и повторите. Или перезапустите СУБД на сервере вручную — команда под кнопкой («или вручную на сервере»)",
                    a.name, if a.version.is_empty() { "?" } else { &a.version }, crate::VERSION)));
            }
        }
    }
    let title = match q.name.as_str() { "setup-wal" => "настройка архива WAL", "restart" => "перезапуск СУБД", n => n };
    let spec = JobSpec { id: String::new(), kind: "action".into(), instance: Some(s.runtime(&i)), site: None, snapshot_id: None, mode: q.name.clone(), params: Value::Null };
    s.audit(&w.user, "action", format!("{}: {}", i.name, q.name));
    Ok(Json(json!({ "job_id": s.jobs.enqueue(spec, format!("{} — {title}", i.name), &i.agent_id, &w.user) })))
}

#[derive(Deserialize)]
pub struct PointsQ {
    #[serde(default)]
    site: Option<String>,
}

/// Площадка-источник для чтения копий экземпляра: основная или одна из её принимающих.
fn source_site(s: &Server, i: &Instance, want: Option<&str>) -> R<crate::proto::SiteAccess> {
    match want.filter(|x| !x.is_empty() && *x != i.site_id) {
        None => Ok(s.site_access(&i.site_id)?),
        Some(w) => {
            let ok = s.store.read(|st| st.sites.iter().find(|x| x.id == i.site_id).map(|x| x.replicas.iter().any(|r| r == w)).unwrap_or(false))
                || s.store.read(|st| st.sites.iter().find(|x| x.id == w).map(|x| x.replicas.contains(&i.site_id)).unwrap_or(false));
            if !ok { return Err(bad("площадка не связана с площадкой экземпляра репликацией")); }
            Ok(s.site_access(w)?)
        }
    }
}

pub async fn instance_points(State(s): S, Path(id): Path<String>, Query(pq): Query<PointsQ>) -> R<Json<Value>> {
    let i = s.instance(&id)?;
    let site = source_site(&s, &i, pq.site.as_deref())?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let mut snaps = be.list_snapshots(Some(&i.id)).await?;
    snaps.reverse();
    let wal = be.list_objects(&format!("wal/{}", i.id), "").await.unwrap_or_default();
    let segs: Vec<&crate::store::types::ObjInfo> = wal.iter().filter(|o| o.name.len() == 24).collect();
    let wal_v = if segs.is_empty() {
        Value::Null
    } else {
        json!({
            "count": segs.len(),
            "first": segs.first().map(|o| o.name.clone()),
            "last": segs.last().map(|o| o.name.clone()),
            "bytes": segs.iter().map(|o| o.size).sum::<u64>(),
            "last_time": segs.iter().map(|o| o.mtime).max().and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
        })
    };
    // Окно PITR: от самой старой полной копии, для которой есть начало журнала, до последнего сегмента.
    let pitr = segs.first().and_then(|first| {
        let oldest = snaps.iter().rev().find(|h| h.kind == "full" && h.extra.get("wal_start").and_then(|v| v.as_str()).map(|ws| ws >= first.name.as_str()).unwrap_or(false))?;
        let to = segs.iter().map(|o| o.mtime).max().and_then(|t| chrono::DateTime::from_timestamp(t, 0))?;
        Some(json!({ "from": oldest.time, "to": to }))
    });
    Ok(Json(json!({ "snapshots": snaps, "wal": wal_v, "pitr": pitr })))
}

// ---------------------------------------------------------------- площадки

fn s3_view(x: &S3Site) -> Value {
    json!({
        "provider": x.provider, "endpoint": x.endpoint, "region": x.region, "bucket": x.bucket, "prefix": x.prefix,
        "access_key": x.access_key, "secret_key": if x.secret_key.is_empty() { "" } else { MASK },
        "admin_access_key": x.admin_access_key, "admin_secret_key": if x.admin_secret_key.is_empty() { "" } else { MASK },
        "path_style": x.path_style, "lock_mode": x.lock_mode, "lock_days": x.lock_days, "storage_class": x.storage_class, "fingerprint": x.fingerprint
    })
}

fn cloud_view(c: &crate::cloud::drive::CloudAccess) -> Value {
    let m = |v: &str| if v.is_empty() { "" } else { MASK };
    json!({
        "provider": c.provider, "title": c.title(), "url": c.url, "username": c.username, "password": m(&c.password), "folder": c.folder, "folder_id": c.folder_id,
        "client_id": c.client_id, "client_secret": m(&c.client_secret), "refresh_token": m(&c.refresh_token), "access_token": m(&c.access_token),
        "token_expires": c.token_expires, "account": c.account, "fingerprint": c.fingerprint, "endpoint": c.endpoint,
        "connected": !c.refresh_token.is_empty() || !c.access_token.is_empty() || !c.password.is_empty()
    })
}

fn site_view(x: &Site) -> Value {
    site_view_ex(x, None)
}

fn site_view_ex(x: &Site, s: Option<&Server>) -> Value {
    let repl: Value = match s {
        Some(s) => {
            let r = s.repl.lock();
            json!(x.replicas.iter().map(|d| json!({ "site": d, "state": r.get(&super::replication::pair_key(&x.id, d)) })).collect::<Vec<_>>())
        }
        None => json!([]),
    };
    let ov = s.and_then(|s| s.cluster.read().site_overrides.get(&x.id).cloned());
    json!({
        "replicas": x.replicas, "replication_interval_min": x.replication_interval_min, "replication": repl, "override": ov,
        "id": x.id, "name": x.name, "kind": x.kind, "url": match x.kind.as_str() { "s3" => x.s3.as_ref().map(|b| format!("s3://{}/{}", b.bucket, b.prefix)).unwrap_or_default(), "cloud" => x.cloud.as_ref().map(|c| c.url_label()).unwrap_or_default(), _ => x.url.clone() },
        "fingerprint": x.fingerprint, "repo": x.repo, "connector_ids": x.connector_ids,
        "compression": x.compression, "has_token": !x.token.is_empty() || x.s3.as_ref().map(|b| !b.secret_key.is_empty()).unwrap_or(false) || x.cloud.as_ref().map(|c| !c.refresh_token.is_empty() || !c.access_token.is_empty() || !c.password.is_empty()).unwrap_or(false), "has_password": !x.password.is_empty(),
        "has_prune_token": !x.prune_token.is_empty() || x.s3.as_ref().map(|b| !b.admin_secret_key.is_empty()).unwrap_or(false) || x.cloud.is_some(),
        "s3": x.s3.as_ref().map(s3_view), "created": x.created,
        "cloud": x.cloud.as_ref().map(cloud_view),
        "disk": x.disk.as_ref().map(|d| { let (t, u) = super::disk::usage(&d.path); json!({ "path": d.path, "total": t, "used": u, "free": t.saturating_sub(u) }) }),
        "node_disk": s.and_then(|s| s.site_disk.lock().get(&x.id).cloned())
    })
}

pub async fn sites(State(s): S) -> Json<Vec<Value>> {
    let list = s.store.read(|st| st.sites.clone());
    Json(list.iter().map(|x| site_view_ex(x, Some(&s))).collect())
}

#[derive(Deserialize)]
pub struct SiteReq {
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    prune_token: String,
    #[serde(default)]
    connector_ids: Vec<String>,
    #[serde(default)]
    compression: Option<i32>,
    #[serde(default)]
    s3: Option<S3Site>,
    #[serde(default)]
    replicas: Option<Vec<String>>,
    #[serde(default)]
    replication_interval_min: Option<u64>,
    /// Площадка «диск сервера»: каталог и адрес сервера для агентов.
    #[serde(default)]
    disk_path: String,
    #[serde(default)]
    public_url: String,
    #[serde(default)]
    cloud: Option<crate::cloud::drive::CloudAccess>,
}

fn check_site(q: &SiteReq) -> R<()> {
    if q.name.trim().is_empty() { return Err(bad("укажите имя площадки")); }
    if q.kind == "s3" {
        let b = q.s3.as_ref().ok_or_else(|| bad("укажите параметры бакета"))?;
        if b.bucket.trim().is_empty() { return Err(bad("укажите бакет")); }
        if b.access_key.trim().is_empty() { return Err(bad("укажите ключ доступа агентов (access key)")); }
        if b.provider != "yandex" && b.provider != "aws" && !b.endpoint.trim().starts_with("http") { return Err(bad("укажите адрес S3: https://s3.example:9000")); }
        if !b.lock_mode.trim().is_empty() && !["GOVERNANCE", "COMPLIANCE"].contains(&b.lock_mode.trim().to_uppercase().as_str()) { return Err(bad("режим Object Lock: GOVERNANCE или COMPLIANCE")); }
        return Ok(());
    }
    if q.kind == "disk" {
        return Ok(()); // каталог и адрес проверяются при создании
    }
    if q.kind == "cloud" {
        let c = q.cloud.as_ref().ok_or_else(|| bad("укажите параметры облака"))?;
        match c.provider.as_str() {
            "webdav" => {
                if !c.url.trim().starts_with("https://") && !c.url.trim().starts_with("http://") { return Err(bad("адрес WebDAV: https://облако/remote.php/dav/files/<логин>")); }
                if c.username.trim().is_empty() { return Err(bad("укажите логин")); }
            }
            "gdrive" => if c.client_id.trim().is_empty() { return Err(bad("укажите Client ID приложения OAuth Google")); },
            "yadisk" => {}
            _ => return Err(bad("облако: gdrive, yadisk или webdav")),
        }
        return Ok(());
    }
    if q.url.trim().is_empty() { return Err(bad("укажите адрес узла")); }
    if !q.url.starts_with("https://") && !q.url.starts_with("http://") { return Err(bad("адрес узла: https://узел:8443")); }
    if q.url.starts_with("https://") && crate::tlsutil::normalize_fp(&q.fingerprint).len() != 64 { return Err(bad("укажите отпечаток сертификата узла (кнопка «Получить отпечаток»)")); }
    Ok(())
}

/// Секреты S3, которые панель прислала маской или пустыми, — из сохранённых.
fn merge_s3(new: &mut S3Site, old: Option<&S3Site>) {
    if let Some(o) = old {
        // Секрет подставляется только для того же хранилища и того же ключа доступа.
        let same = o.endpoint.trim().trim_end_matches('/') == new.endpoint.trim().trim_end_matches('/') || new.endpoint.trim().is_empty();
        if same && o.access_key == new.access_key && (new.secret_key.is_empty() || new.secret_key == MASK) { new.secret_key = o.secret_key.clone(); }
        if same && o.admin_access_key == new.admin_access_key && (new.admin_secret_key.is_empty() || new.admin_secret_key == MASK) { new.admin_secret_key = o.admin_secret_key.clone(); }
    }
    if new.secret_key == MASK { new.secret_key.clear(); }
    if new.admin_secret_key == MASK { new.admin_secret_key.clear(); }
    new.normalize();
}

/// Секреты облака, которые панель прислала маской или пустыми, — из сохранённых; результат входа OAuth — в токены.
fn merge_cloud(s: &Server, new: &mut crate::cloud::drive::CloudAccess, old: Option<&crate::cloud::drive::CloudAccess>) -> R<()> {
    // Сохранённые секреты подставляются, только если облако то же: иначе их можно было бы отправить
    // на чужой адрес, изменив площадку.
    let same_dest = |o: &crate::cloud::drive::CloudAccess| o.provider == new.provider && o.url.trim() == new.url.trim() && o.endpoint.trim() == new.endpoint.trim() && o.client_id.trim() == new.client_id.trim() && o.username.trim() == new.username.trim();
    if let Some(o) = old.filter(|o| same_dest(o)) {
        for (n, ov) in [(&mut new.password, &o.password), (&mut new.client_secret, &o.client_secret), (&mut new.refresh_token, &o.refresh_token), (&mut new.access_token, &o.access_token)] {
            if n.is_empty() || n == MASK { *n = ov.clone(); }
        }
        if new.token_expires == 0 { new.token_expires = o.token_expires; }
        if new.account.is_empty() { new.account = o.account.clone(); }
    }
    for v in [&mut new.password, &mut new.client_secret, &mut new.refresh_token, &mut new.access_token] { if v == MASK { v.clear(); } }
    if !new.oauth_flow.is_empty() {
        let t = s.oauth_take(&new.oauth_flow, &new.provider, &new.client_id, &new.endpoint).ok_or_else(|| bad("вход в облако не завершён, устарел или выполнен для другого приложения — нажмите «Подключить» ещё раз"))?;
        if !t.refresh_token.is_empty() { new.refresh_token = t.refresh_token; }
        if !t.access_token.is_empty() { new.access_token = t.access_token; }
        new.token_expires = if t.expires_in > 0 { Utc::now().timestamp() + t.expires_in } else { 0 };
        new.oauth_flow.clear();
    }
    new.provider = new.provider.trim().to_lowercase();
    if new.provider == "nextcloud" { new.provider = "webdav".into(); }
    match new.provider.as_str() {
        "gdrive" if new.refresh_token.is_empty() => return Err(bad("Google Диск не подключён: нажмите «Подключить» и разрешите доступ")),
        "yadisk" if new.access_token.is_empty() => return Err(bad("Яндекс Диск не подключён: нажмите «Подключить» или вставьте токен OAuth")),
        "webdav" if new.password.is_empty() => return Err(bad("укажите пароль приложения WebDAV")),
        _ => {}
    }
    Ok(())
}

/// Проверить облако и узнать учётную запись.
async fn cloud_probe(c: &mut crate::cloud::drive::CloudAccess) -> R<Value> {
    let info = tokio::time::timeout(std::time::Duration::from_secs(60), async { c.store()?.check().await }).await.map_err(|_| bad("облако не ответило за 60 с"))?.map_err(|e| bad(&format!("{e:#}")))?;
    if let Some(a) = info.get("account").and_then(|v| v.as_str()).filter(|a| !a.is_empty()) { c.account = a.to_string(); }
    if let Some(f) = info.get("folder_id").and_then(|v| v.as_str()) { if c.folder_id.is_empty() { c.folder_id = f.to_string(); } }
    Ok(info)
}

pub async fn site_create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<SiteReq>) -> R<Json<Value>> {
    check_site(&q)?;
    if q.kind == "disk" || q.kind == "cloud" { return site_create_ex(s, w, q).await; }
    let s3 = q.kind == "s3";
    if !s3 && q.token.is_empty() { return Err(bad("укажите токен узла (append)")); }
    if s3 && q.s3.as_ref().map(|b| b.secret_key.is_empty()).unwrap_or(true) { return Err(bad("укажите секретный ключ агентов")); }
    if q.password.len() < 8 { return Err(bad("пароль репозитория — не короче 8 символов")); }
    let x = s.store.update(|st| {
        let mut b = q.s3.clone();
        if let Some(b) = b.as_mut() { merge_s3(b, None); }
        let x = Site {
            id: crate::util::short_id(),
            name: q.name.trim().into(),
            kind: if s3 { "s3".into() } else { "node".into() },
            s3: if s3 { b } else { None },
            url: if s3 { String::new() } else { q.url.trim().trim_end_matches('/').into() },
            fingerprint: q.fingerprint.trim().into(),
            repo: if q.repo.trim().is_empty() { "main".into() } else { q.repo.trim().into() },
            token: q.token.trim().into(),
            password: q.password.clone(),
            prune_token: q.prune_token.trim().into(),
            connector_ids: q.connector_ids.clone(),
            compression: q.compression.unwrap_or(3).clamp(0, 19),
            replicas: q.replicas.clone().unwrap_or_default().into_iter().filter(|r| st.sites.iter().any(|y| &y.id == r)).collect(),
            replication_interval_min: q.replication_interval_min.unwrap_or(10).clamp(1, 1440),
            created: Utc::now(),
            disk: None,
            cloud: None,
        };
        st.sites.push(x.clone());
        Ok(x)
    })?;
    s.audit(&w.user, "site_create", format!("{} ({})", x.name, site_view(&x)["url"].as_str().unwrap_or("")));
    Ok(Json(site_view(&x)))
}

/// Площадки «диск сервера» и «облачный диск».
async fn site_create_ex(s: Arc<Server>, w: Who, q: SiteReq) -> R<Json<Value>> {
    if q.password.len() < 8 { return Err(bad("пароль репозитория — не короче 8 символов")); }
    let id = crate::util::short_id();
    let mut x = Site {
        id: id.clone(),
        name: q.name.trim().into(),
        kind: q.kind.clone(),
        s3: None,
        disk: None,
        cloud: None,
        url: String::new(),
        fingerprint: String::new(),
        repo: if q.repo.trim().is_empty() { "main".into() } else { q.repo.trim().into() },
        token: String::new(),
        password: q.password.clone(),
        prune_token: String::new(),
        connector_ids: q.connector_ids.clone(),
        compression: q.compression.unwrap_or(3).clamp(0, 19),
        replicas: vec![],
        replication_interval_min: q.replication_interval_min.unwrap_or(10).clamp(1, 1440),
        created: Utc::now(),
    };
    if q.kind == "disk" {
        // Каталог на сервере управления выбирает только администратор.
        if w.role != "admin" { return Err(ApiError(StatusCode::FORBIDDEN, "площадку на диске сервера добавляет администратор".into())); }
        let raw = q.disk_path.trim().trim_end_matches('/').to_string();
        if !raw.starts_with('/') || raw.len() < 2 || raw.split('/').any(|c| c == ".." || c == ".") { return Err(bad("каталог площадки — абсолютный путь без «..», например /mnt/data/hopebackup")); }
        let pu = q.public_url.trim().trim_end_matches('/').to_string();
        if !pu.starts_with("https://") { return Err(bad("адрес сервера для агентов: https://сервер:4443")); }
        let fresh = !std::path::Path::new(&raw).exists();
        std::fs::create_dir_all(&raw).map_err(|e| bad(&format!("создание {raw}: {e}")))?;
        let path = std::fs::canonicalize(&raw).map_err(|e| bad(&format!("{raw}: {e}")))?.to_string_lossy().to_string();
        let under = |d: &str| path == d || path.starts_with(&format!("{d}/"));
        let own = crate::agent::data_dir().to_string_lossy().to_string();
        if path == "/" || ["/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/proc", "/sys", "/dev", "/run", "/root", "/tmp", "/var/tmp", "/var/lib/dpkg", "/var/lib/rpm", own.as_str()].iter().any(|d| under(d)) || path == "/home" || path == "/var" || path == "/mnt" || path == "/srv" || path == "/opt" {
            if fresh { let _ = std::fs::remove_dir(&raw); }
            return Err(bad(&format!("каталог {path} не подходит для копий — выберите отдельный, например /mnt/data/hopebackup")));
        }
        let p = std::path::Path::new(&path);
        let has_node = p.join("node.json").exists();
        let empty = std::fs::read_dir(p).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !has_node && !empty { return Err(bad(&format!("каталог {path} не пуст — укажите пустой каталог (или каталог прежней площадки HopeBackup)"))); }
        if let Some(other) = s.store.read(|st| st.sites.iter().find(|x| x.disk.as_ref().map(|d| d.path == path).unwrap_or(false)).map(|x| x.name.clone())) {
            return Err(bad(&format!("каталог {path} уже занят площадкой «{other}»")));
        }
        let (root_dev, dev) = { use std::os::unix::fs::MetadataExt; (std::fs::metadata("/").map(|m| m.dev()).unwrap_or(0), std::fs::metadata(p).map(|m| m.dev()).unwrap_or(1)) };
        if root_dev == dev { tracing::warn!("площадка {path} на корневой файловой системе сервера"); }
        let (admin, append) = crate::node::init_embedded(p, &id).map_err(|e| bad(&format!("{e:#}")))?;
        x.url = format!("{pu}/node/{id}");
        x.fingerprint = s.fingerprint.clone();
        x.token = append;
        x.prune_token = admin;
        x.disk = Some(DiskSite { path });
    } else {
        let mut c = q.cloud.clone().ok_or_else(|| bad("укажите параметры облака"))?;
        merge_cloud(&s, &mut c, None)?;
        cloud_probe(&mut c).await?;
        x.url = format!("cloud://{}", c.provider);
        x.cloud = Some(c);
    }
    let x = s.store.update(|st| {
        let mut x = x.clone();
        x.replicas = q.replicas.clone().unwrap_or_default().into_iter().filter(|r| st.sites.iter().any(|y| &y.id == r)).collect();
        st.sites.push(x.clone());
        Ok(x)
    })?;
    s.audit(&w.user, "site_create", format!("{} ({})", x.name, site_view(&x)["url"].as_str().unwrap_or("")));
    Ok(Json(site_view(&x)))
}

pub async fn site_update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<SiteReq>) -> R<Json<Value>> {
    let kind = s.store.read(|st| st.sites.iter().find(|x| x.id == id).map(|x| x.kind.clone())).ok_or_else(|| not_found("площадка не найдена"))?;
    let q = SiteReq { kind: kind.clone(), ..q };
    check_site(&q)?;
    let cloud_new = if kind == "cloud" {
        let old = s.store.read(|st| st.sites.iter().find(|x| x.id == id).and_then(|x| x.cloud.clone()));
        let mut c = q.cloud.clone().unwrap_or_default();
        merge_cloud(&s, &mut c, old.as_ref())?;
        let changed = old.as_ref().map(|o| o.provider != c.provider || o.url != c.url || o.username != c.username || o.password != c.password || o.folder != c.folder || o.refresh_token != c.refresh_token || o.access_token != c.access_token || o.endpoint != c.endpoint).unwrap_or(true);
        if changed { cloud_probe(&mut c).await?; }
        Some(c)
    } else { None };
    let x = s.store.update(|st| {
        let x = st.sites.iter_mut().find(|x| x.id == id).ok_or_else(|| anyhow!("площадка не найдена"))?;
        x.name = q.name.trim().into();
        if kind == "s3" {
            let mut b = q.s3.clone().unwrap_or_default();
            merge_s3(&mut b, x.s3.as_ref());
            x.s3 = Some(b);
        } else if kind == "disk" {
            // Каталог и токены встроенного узла не меняются; адрес для агентов — можно.
            let pu = q.public_url.trim().trim_end_matches('/');
            if pu.starts_with("https://") { x.url = format!("{pu}/node/{}", x.id); }
        } else if kind == "cloud" {
            if let Some(c) = cloud_new.clone() { x.cloud = Some(c); }
        } else {
            x.url = q.url.trim().trim_end_matches('/').into();
            x.fingerprint = q.fingerprint.trim().into();
            if !q.token.is_empty() && q.token != MASK { x.token = q.token.trim().into(); }
            if !q.prune_token.is_empty() && q.prune_token != MASK { x.prune_token = q.prune_token.trim().into(); }
        }
        if !q.repo.trim().is_empty() { x.repo = q.repo.trim().into(); }
        if !q.password.is_empty() && q.password != MASK { x.password = q.password.clone(); }
        x.connector_ids = q.connector_ids.clone();
        if let Some(c) = q.compression { x.compression = c.clamp(0, 19); }
        if let Some(m) = q.replication_interval_min { x.replication_interval_min = m.clamp(1, 1440); }
        Ok(x.clone())
    })?;
    if let Some(r) = &q.replicas {
        let r: Vec<String> = r.iter().filter(|d| **d != id).cloned().collect();
        s.store.update(|st| {
            let valid: Vec<String> = r.iter().filter(|d| st.sites.iter().any(|y| &y.id == *d)).cloned().collect();
            if let Some(x) = st.sites.iter_mut().find(|x| x.id == id) { x.replicas = valid; }
            Ok(())
        })?;
    }
    let x = s.store.read(|st| st.sites.iter().find(|y| y.id == id).cloned()).unwrap_or(x);
    s.audit(&w.user, "site_update", x.name.clone());
    Ok(Json(site_view(&x)))
}

pub async fn site_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    s.store.update(|st| {
        if st.instances.iter().any(|i| i.site_id == id) { return Err(anyhow!("площадка используется экземплярами")); }
        st.sites.retain(|x| x.id != id);
        for x in &mut st.sites { x.replicas.retain(|r| *r != id); }
        Ok(())
    })?;
    s.disk_forget(&id);
    s.audit(&w.user, "site_delete", id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct OAuthStart {
    provider: String,
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    endpoint: String,
    /// Площадка, у которой взять сохранённый секрет клиента (повторное подключение).
    #[serde(default)]
    site_id: String,
}

/// Вход в облако по коду устройства: сервер получает код, администратор вводит его на странице
/// Google/Яндекса, сервер сам дожидается токенов (в браузер они не попадают).
pub async fn oauth_start(State(s): S, Extension(w): Extension<Who>, Json(q): Json<OAuthStart>) -> R<Json<Value>> {
    let mut secret = q.client_secret.clone();
    if (secret.is_empty() || secret == MASK) && !q.site_id.is_empty() {
        // Сохранённый секрет — только для того же приложения и адреса, что у площадки.
        secret = s.store.read(|st| st.sites.iter().find(|x| x.id == q.site_id).and_then(|x| x.cloud.as_ref())
            .filter(|c| c.provider == q.provider && c.client_id.trim() == q.client_id.trim() && c.endpoint.trim() == q.endpoint.trim())
            .map(|c| c.client_secret.clone())).unwrap_or_default();
        if secret.is_empty() { return Err(bad("укажите Client secret")); }
    }
    if secret == MASK { secret.clear(); }
    if s.oauth_busy() { return Err(bad("слишком много незавершённых входов — подождите несколько минут")); }
    let d = crate::cloud::oauth::device_start(&q.provider, &q.endpoint, &q.client_id).await.map_err(|e| bad(&format!("{e:#}")))?;
    let flow = crate::util::short_id();
    s.oauth_begin(&flow, &q.provider, &q.endpoint, &q.client_id, &secret, &d);
    s.audit(&w.user, "cloud_connect", q.provider.clone());
    Ok(Json(json!({ "flow": flow, "user_code": d.user_code, "verification_url": d.verification_url, "expires_in": d.expires_in, "interval": d.interval })))
}

pub async fn oauth_status(State(s): S, Path(flow): Path<String>) -> R<Json<Value>> {
    Ok(Json(s.oauth_status(&flow).ok_or_else(|| not_found("вход не найден или устарел"))?))
}

#[derive(Deserialize)]
pub struct ProbeReq {
    url: String,
}

pub async fn site_probe(Json(q): Json<ProbeReq>) -> R<Json<Value>> {
    let url = format!("{}/v1/health", q.url.trim().trim_end_matches('/'));
    Ok(Json(json!({ "fingerprint": crate::tlsutil::probe_fingerprint(&url).await? })))
}

pub async fn site_test(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let site = s.site_access(&id)?;
    let logs = parking_lot::Mutex::new(Vec::<String>::new());
    let remote = s.tunnels.connect(&site, &|m| logs.lock().push(m.to_string())).await?;
    s.note_site_disk(&id, &remote).await;
    let be = crate::store::backend_for(&remote)?;
    let exists = be.read_config().await?.is_some();
    let stats = if exists { be.stats().await.ok() } else { None };
    let cloud = match &remote.cloud { Some(c) => c.store()?.check().await.ok(), None => None };
    Ok(Json(json!({ "ok": true, "via": if remote.cloud.is_some() { site_view_ex(&s.store.read(|st| st.sites.iter().find(|x| x.id == id).cloned()).ok_or_else(|| not_found("площадка не найдена"))?, None)["url"].clone() } else { json!(remote.url) }, "repo_exists": exists, "stats": stats, "cloud": cloud, "log": logs.into_inner() })))
}

pub async fn site_init(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    // У принимающей площадки репозиторий создаёт первая репликация — копией исходного (тот же ключ и пароль).
    // Созданный вручную получил бы свой идентификатор, и репликация в него была бы невозможна.
    let sources: Vec<String> = s.store.read(|st| st.sites.iter().filter(|x| x.replicas.contains(&id)).map(|x| format!("«{}»", x.name)).collect());
    if !sources.is_empty() {
        return Err(bad(&format!("это принимающая площадка для {}: репозиторий на ней создаст первая репликация — нажмите «Синхронизировать сейчас» у источника. Вручную его создавать не нужно", sources.join(", "))));
    }
    let site = s.site_access(&id)?;
    let name = site.name.clone();
    let spec = JobSpec { id: String::new(), kind: "init_repo".into(), instance: None, site: Some(site), snapshot_id: None, mode: String::new(), params: Value::Null };
    let jid = s.jobs.enqueue(spec, format!("Площадка «{name}» — создание репозитория"), "server", &w.user);
    s.spawn_local(jid.clone(), None);
    s.audit(&w.user, "site_init", name);
    Ok(Json(json!({ "job_id": jid })))
}

/// Синхронизировать принимающие площадки сейчас.
pub async fn site_replicate(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    let has = s.store.read(|st| st.sites.iter().find(|x| x.id == id).map(|x| !x.replicas.is_empty())).ok_or_else(|| not_found("площадка не найдена"))?;
    if !has { return Err(bad("у площадки нет принимающих площадок")); }
    let jid = s.enqueue_replication(&id, &w.user).ok_or_else(|| bad("репликация уже выполняется — повторится после неё"))?;
    s.audit(&w.user, "replicate", id);
    Ok(Json(json!({ "job_id": jid })))
}

#[derive(Deserialize)]
pub struct FailoverReq {
    to: String,
}

/// Площадка потеряна: перевести её экземпляры на принимающую площадку (копии и журнал там уже есть).
/// Бывшая площадка становится принимающей для новой, чтобы после возвращения догнать изменения.
pub async fn site_failover(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<FailoverReq>) -> R<Json<Value>> {
    let n = s.store.update(|st| {
        let from = st.sites.iter().find(|x| x.id == id).cloned().ok_or_else(|| anyhow!("площадка не найдена"))?;
        if !from.replicas.contains(&q.to) { return Err(anyhow!("площадка «{}» не принимающая для «{}»", q.to, from.name)); }
        let mut n = 0;
        for i in st.instances.iter_mut().filter(|i| i.site_id == id) { i.site_id = q.to.clone(); n += 1; }
        if let Some(x) = st.sites.iter_mut().find(|x| x.id == id) { x.replicas.retain(|r| *r != q.to); }
        if let Some(x) = st.sites.iter_mut().find(|x| x.id == q.to) { if !x.replicas.contains(&id) { x.replicas.push(id.clone()); } }
        Ok(n)
    })?;
    s.audit(&w.user, "site_failover", format!("{id} → {}: экземпляров {n}", q.to));
    Ok(Json(json!({ "ok": true, "instances": n })))
}

// ---------------------------------------------------------------- коннекторы

fn connector_view(s: &Server, c: &Connector) -> Value {
    let mut p = mask(&c.params);
    if let Some(o) = p.as_object_mut() {
        let has = c.params.get("private_key").and_then(|v| v.as_str()).map(|k| !k.is_empty()).unwrap_or(false);
        o.insert("has_key".into(), json!(has));
        if c.kind == "wireguard" {
            if let Some(k) = c.params.get("private_key").and_then(|v| v.as_str()).filter(|k| !k.is_empty()) {
                o.insert("public_key".into(), json!(crate::connectors::wireguard::pubkey(k).ok()));
            }
        }
    }
    let sites: Vec<String> = s.store.read(|st| st.sites.iter().filter(|x| x.connector_ids.contains(&c.id)).map(|x| x.name.clone()).collect());
    // Агенты, которые пишут на площадки через этот коннектор (им нужны те же пакеты VPN).
    let agents: Vec<Value> = s.store.read(|st| connector_agents(st, &c.id).iter().filter_map(|a| st.agents.iter().find(|x| &x.id == a))
        .map(|a| json!({ "id": a.id, "name": a.name, "can_install": a.caps.iter().any(|c| c == "install-pkgs") })).collect());
    json!({ "id": c.id, "name": c.name, "kind": c.kind, "params": p, "created": c.created, "status": s.tunnels.status().get(&c.id), "sites": sites, "agents": agents })
}

pub async fn connectors(State(s): S) -> Json<Vec<Value>> {
    let list = s.store.read(|st| st.connectors.clone());
    Json(list.iter().map(|c| connector_view(&s, c)).collect())
}

#[derive(Deserialize)]
pub struct ConnectorReq {
    name: String,
    kind: String,
    #[serde(default)]
    params: Value,
}

pub async fn connector_create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<ConnectorReq>) -> R<Json<Value>> {
    if !crate::connectors::AVAILABLE.contains(&q.kind.as_str()) { return Err(bad("этот тип коннектора пока в разработке")); }
    if q.name.trim().is_empty() { return Err(bad("укажите имя")); }
    let c = s.store.update(|st| {
        let mut params = if q.params.is_object() { q.params.clone() } else { json!({}) };
        if let Some(o) = params.as_object_mut() { for k in ["has_key", "public_key"] { o.remove(k); } }
        let c = Connector { id: crate::util::short_id(), name: q.name.trim().into(), kind: q.kind.clone(), params, created: Utc::now() };
        st.connectors.push(c.clone());
        Ok(c)
    })?;
    s.audit(&w.user, "connector_create", c.name.clone());
    Ok(Json(connector_view(&s, &c)))
}

pub async fn connector_update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<ConnectorReq>) -> R<Json<Value>> {
    let c = s.store.update(|st| {
        let c = st.connectors.iter_mut().find(|c| c.id == id).ok_or_else(|| anyhow!("коннектор не найден"))?;
        if !q.name.trim().is_empty() { c.name = q.name.trim().into(); }
        let mut p = if q.params.is_object() { q.params.clone() } else { json!({}) };
        unmask(&mut p, &c.params);
        if let (Some(np), Some(op)) = (p.as_object_mut(), c.params.as_object()) {
            // Секреты, которых форма не прислала, сохраняются; служебные поля представления — не хранятся.
            for k in crate::connectors::SECRET_KEYS {
                if !np.contains_key(*k) { if let Some(v) = op.get(*k) { np.insert(k.to_string(), v.clone()); } }
            }
            for k in ["has_key", "public_key"] { np.remove(k); }
        }
        c.params = p;
        Ok(c.clone())
    })?;
    // Новые параметры применятся при следующем подключении: старый канал опускаем.
    s.tunnels.down(&c.id).await;
    s.audit(&w.user, "connector_update", c.name.clone());
    Ok(Json(connector_view(&s, &c)))
}

pub async fn connector_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    s.tunnels.down(&id).await;
    s.store.update(|st| {
        st.connectors.retain(|c| c.id != id);
        for x in &mut st.sites { x.connector_ids.retain(|c| *c != id); }
        Ok(())
    })?;
    s.audit(&w.user, "connector_delete", id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn connector_test(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let c = s.store.read(|st| st.connectors.iter().find(|c| c.id == id).cloned()).ok_or_else(|| not_found("коннектор не найден"))?;
    if c.kind == "direct" { return Ok(Json(json!({ "ok": true }))); }
    let spec = crate::proto::ConnectorSpec { id: c.id.clone(), name: c.name.clone(), kind: c.kind.clone(), params: c.params.clone() };
    // Адрес узла — от первой площадки, которая пользуется коннектором (для маршрута через VPN).
    let site = s.store.read(|st| st.sites.iter().find(|x| x.connector_ids.contains(&c.id)).map(|x| x.id.clone()));
    let site_url = site.as_ref().and_then(|sid| s.store.read(|st| st.sites.iter().find(|x| &x.id == sid).map(|x| x.url.clone())));
    let mut v = s.tunnels.test(&spec, site_url.as_deref()).await?;
    // Узел площадки через канал.
    if let Some(sid) = site {
        let mut access = s.site_access(&sid)?;
        access.connectors.retain(|x| x.id == c.id);
        match s.tunnels.connect(&access, &|_| {}).await {
            Ok(r) => { v["node"] = json!({ "ok": true, "url": r.url, "site": access.name }); }
            Err(e) => { v["node"] = json!({ "ok": false, "error": format!("{e:#}"), "site": access.name }); }
        }
    }
    v["ok"] = json!(true);
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ReqQ {
    kind: String,
}

/// Пакеты для коннектора этого вида на сервере управления (панель спрашивает после «Сохранить»).
pub async fn connector_requirements(Query(q): Query<ReqQ>) -> Json<Value> {
    Json(crate::connectors::net::requirements_report(&q.kind))
}

#[derive(Deserialize)]
pub struct InstallReq {
    kind: String,
    /// Пусто — на сервере управления; иначе — на этих агентах.
    #[serde(default)]
    agents: Vec<String>,
}

/// Агенты, которые ходят через коннектор: экземпляры на площадках, где он указан.
pub(crate) fn connector_agents(st: &super::state::State, id: &str) -> Vec<String> {
    let sites: Vec<&String> = st.sites.iter().filter(|s| s.connector_ids.iter().any(|c| c == id)).map(|s| &s.id).collect();
    let mut a: Vec<String> = st.instances.iter().filter(|i| sites.contains(&&i.site_id)).map(|i| i.agent_id.clone()).collect();
    a.sort(); a.dedup(); a
}

/// Установить пакеты коннектора — по подтверждению администратора, заданием с журналом.
pub async fn connector_install(State(s): S, Extension(w): Extension<Who>, Json(q): Json<InstallReq>) -> R<Json<Value>> {
    if !["ipsec", "l2tp_ipsec", "openvpn", "wireguard"].contains(&q.kind.as_str()) { return Err(bad("для этого вида коннектора пакеты не нужны")); }
    let spec = |kind: &str| crate::proto::JobSpec { id: String::new(), kind: "install_pkgs".into(), instance: None, site: None, snapshot_id: None, mode: kind.to_string(), params: Value::Null };
    let mut jobs = Vec::new();
    if q.agents.is_empty() {
        let id = s.jobs.enqueue(spec(&q.kind), format!("Установка пакетов {} на сервере управления", vpn_title(&q.kind)), "server", &w.user);
        let (s2, jid) = (s.clone(), id.clone());
        tokio::spawn(async move {
            if s2.jobs.start_local(&jid).is_none() { return; }
            let (s3, j3) = (s2.clone(), jid.clone());
            let log = move |m: &str| s3.jobs.log(&j3, vec![m.to_string()]);
            match crate::connectors::net::install_for(&spec_kind(&s2, &jid), &log).await {
                Ok(v) => { s2.jobs.finish(&jid, true, v, None); }
                Err(e) => { log(&format!("ошибка: {e:#}")); s2.jobs.finish(&jid, false, Value::Null, Some(format!("{e:#}"))); }
            }
        });
        jobs.push(id);
    } else {
        for a in &q.agents {
            let (name, caps, ver) = s.store.read(|st| st.agents.iter().find(|x| &x.id == a).map(|x| (x.name.clone(), x.caps.clone(), x.version.clone()))).ok_or_else(|| not_found("агент не найден"))?;
            if !caps.iter().any(|c| c == "install-pkgs") {
                if caps.is_empty() && ver.is_empty() { return Err(bad(format!("агент «{name}» ещё не выходил на связь — повторите через минуту"))); }
                return Err(bad(format!("агент «{name}» версии {ver} не умеет ставить пакеты — обновите его в «Обновлениях»")));
            }
            jobs.push(s.jobs.enqueue(spec(&q.kind), format!("Установка пакетов {} на «{name}»", vpn_title(&q.kind)), a, &w.user));
        }
    }
    s.audit(&w.user, "connector_install", format!("{} → {}", q.kind, if q.agents.is_empty() { "сервер управления".to_string() } else { q.agents.join(", ") }));
    Ok(Json(json!({ "jobs": jobs })))
}

fn vpn_title(kind: &str) -> &'static str {
    match kind { "ipsec" => "IPsec IKEv2", "l2tp_ipsec" => "L2TP/IPsec", "openvpn" => "OpenVPN", "wireguard" => "WireGuard", _ => "VPN" }
}

fn spec_kind(s: &Server, jid: &str) -> String { s.jobs.get(jid).map(|j| j.mode).unwrap_or_default() }

/// Пара ключей WireGuard для нового коннектора (закрытый ключ сразу уходит в форму коннектора).
pub async fn wg_keygen() -> Json<Value> {
    let (private_key, public_key) = crate::connectors::wireguard::genkey();
    Json(json!({ "private_key": private_key, "public_key": public_key }))
}

// ---------------------------------------------------------------- агенты

pub async fn agents(State(s): S) -> Json<Vec<Value>> {
    let st = s.store.get();
    let wal = s.wal.lock().clone();
    Json(st.agents.iter().map(|a| {
        let mine: Vec<&crate::proto::WalStatus> = st.instances.iter().filter(|i| i.agent_id == a.id).filter_map(|i| wal.get(&i.id)).collect();
        json!({ "id": a.id, "name": a.name, "host": a.host, "os": a.os, "version": a.version, "enrolled": a.enrolled, "last_seen": a.last_seen, "online": agent_online(a), "wal": mine, "caps": a.caps })
    }).collect())
}

#[derive(Deserialize)]
pub struct EnrollTokReq {
    #[serde(default)]
    note: String,
}

pub async fn agent_enroll_token(State(s): S, Extension(w): Extension<Who>, Json(q): Json<EnrollTokReq>) -> R<Json<Value>> {
    let t = crate::util::token();
    let expires = Utc::now() + chrono::Duration::hours(24);
    s.store.update(|st| {
        st.enroll_tokens.retain(|e| e.expires > Utc::now());
        st.enroll_tokens.push(EnrollToken { token_hash: crate::util::sha256_hex(&t), expires, note: q.note.clone(), machine_id: String::new() });
        Ok(())
    })?;
    let host = crate::util::local_ips().first().cloned().unwrap_or_else(crate::util::hostname);
    let url = format!("https://{host}:{}", s.listen_port);
    let cmd = format!("hopebackup agent enroll --server {url} --token {t} --fingerprint {}", s.fingerprint);
    s.audit(&w.user, "agent_token", q.note.clone());
    Ok(Json(json!({ "token": t, "expires": expires, "command": cmd, "server_fingerprint": s.fingerprint, "server_url": url })))
}

pub async fn agent_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    s.store.update(|st| {
        if st.instances.iter().any(|i| i.agent_id == id) { return Err(anyhow!("агент обслуживает экземпляры — сначала перенесите их")); }
        st.agents.retain(|a| a.id != id);
        Ok(())
    })?;
    s.audit(&w.user, "agent_delete", id);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- цели восстановления

fn target_view(t: &Target) -> Value {
    json!({ "id": t.id, "name": t.name, "kind": t.kind, "host": t.host, "port": t.port, "user": t.user, "agent_id": t.agent_id, "has_key": !t.private_key.is_empty(), "has_password": !t.password.is_empty(), "params": mask(&t.params), "created": t.created })
}

pub async fn targets(State(s): S) -> Json<Vec<Value>> { Json(s.store.read(|st| st.targets.iter().map(target_view).collect())) }

#[derive(Deserialize)]
pub struct TargetReq {
    name: String,
    kind: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: String,
    #[serde(default)]
    private_key: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    params: Value,
}

fn check_target(q: &TargetReq, existing: Option<&Target>) -> R<()> {
    if q.name.trim().is_empty() { return Err(bad("укажите имя цели")); }
    match q.kind.as_str() {
        "ssh" => {
            let has_key = !q.private_key.trim().is_empty() || existing.map(|t| !t.private_key.is_empty()).unwrap_or(false);
            let has_pw = (!q.password.is_empty() && q.password != MASK) || existing.map(|t| !t.password.is_empty()).unwrap_or(false);
            if q.host.trim().is_empty() || !(has_key || has_pw) { return Err(bad("для SSH укажите адрес и закрытый ключ или пароль")); }
            crate::util::valid_host(q.host.trim()).map_err(|e| bad(&e.to_string()))?;
            let u = if q.user.trim().is_empty() { existing.map(|t| t.user.clone()).unwrap_or_else(|| "root".into()) } else { q.user.trim().to_string() };
            crate::util::valid_ssh_user(&u).map_err(|e| bad(&e.to_string()))?;
        }
        "agent" => if q.agent_id.is_empty() { return Err(bad("выберите агента")); },
        k if super::vm::is_vm_kind(k) => {
            let mut p = q.params.clone();
            if let Some(e) = existing { unmask(&mut p, &e.params); }
            let probe = Target { id: String::new(), name: q.name.clone(), kind: k.to_string(), host: String::new(), port: 22, user: String::new(), private_key: String::new(), password: String::new(), agent_id: String::new(), params: p, created: Utc::now() };
            super::vm::provider(&probe)?;
        }
        _ => return Err(bad("этот тип цели пока в разработке")),
    }
    Ok(())
}

pub async fn target_create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<TargetReq>) -> R<Json<Value>> {
    check_target(&q, None)?;
    let t = s.store.update(|st| {
        let t = Target {
            id: crate::util::short_id(),
            name: q.name.trim().into(),
            kind: q.kind.clone(),
            host: q.host.trim().into(),
            port: q.port.unwrap_or(22),
            user: if q.user.trim().is_empty() { "root".into() } else { q.user.trim().into() },
            private_key: q.private_key.clone(),
            password: if q.password == MASK { String::new() } else { q.password.clone() },
            agent_id: q.agent_id.clone(),
            params: if q.params.is_object() { q.params.clone() } else { json!({}) },
            created: Utc::now(),
        };
        st.targets.push(t.clone());
        Ok(t)
    })?;
    s.audit(&w.user, "target_create", t.name.clone());
    Ok(Json(target_view(&t)))
}

pub async fn target_update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<TargetReq>) -> R<Json<Value>> {
    let old = s.store.read(|st| st.targets.iter().find(|t| t.id == id).cloned()).ok_or_else(|| not_found("цель не найдена"))?;
    if old.kind != q.kind { return Err(bad("тип цели менять нельзя — создайте новую")); }
    check_target(&q, Some(&old))?;
    let t = s.store.update(|st| {
        let t = st.targets.iter_mut().find(|t| t.id == id).ok_or_else(|| anyhow!("цель не найдена"))?;
        // Адрес, порт или пользователь сменились — сохранённый пароль на новый адрес не отправляем: его вводят заново.
        // (Закрытый ключ сохраняем: при входе по ключу узел получает только подпись, сам ключ не передаётся.)
        let moved = old.host != q.host.trim() || q.port.map(|p| p != old.port).unwrap_or(false) || (!q.user.trim().is_empty() && old.user != q.user.trim());
        t.name = q.name.trim().into();
        t.host = q.host.trim().into();
        if let Some(p) = q.port { t.port = p; }
        if !q.user.trim().is_empty() { t.user = q.user.trim().into(); }
        if !q.private_key.trim().is_empty() && q.private_key != MASK { t.private_key = q.private_key.clone(); }
        if !q.password.is_empty() && q.password != MASK { t.password = q.password.clone(); } else if moved { t.password.clear(); }
        if q.password == "-" { t.password.clear(); }
        if t.kind == "ssh" && t.private_key.trim().is_empty() && t.password.is_empty() { anyhow::bail!("адрес или пользователь изменились — введите пароль заново (или укажите ключ)"); }
        if !q.agent_id.is_empty() { t.agent_id = q.agent_id.clone(); }
        if q.params.is_object() {
            let mut p = q.params.clone();
            unmask(&mut p, &t.params);
            if let (Some(np), Some(op)) = (p.as_object_mut(), t.params.as_object()) {
                for k in crate::connectors::SECRET_KEYS { if !np.contains_key(*k) { if let Some(v) = op.get(*k) { np.insert(k.to_string(), v.clone()); } } }
            }
            t.params = p;
        }
        Ok(t.clone())
    })?;
    s.audit(&w.user, "target_update", t.name.clone());
    Ok(Json(target_view(&t)))
}

pub async fn target_delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    s.store.update(|st| { st.targets.retain(|t| t.id != id); Ok(()) })?;
    s.audit(&w.user, "target_delete", id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn target_test(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let t = s.store.read(|st| st.targets.iter().find(|t| t.id == id).cloned()).ok_or_else(|| not_found("цель не найдена"))?;
    match t.kind.as_str() {
        "ssh" => {
            let out = super::ssh_restore::test(&t).await?;
            Ok(Json(json!({ "ok": true, "output": out })))
        }
        k if super::vm::is_vm_kind(k) => {
            let out = super::vm::test(&t).await?;
            Ok(Json(json!({ "ok": true, "output": out })))
        }
        _ => {
            let online = s.store.read(|st| st.agents.iter().find(|a| a.id == t.agent_id).map(agent_online).unwrap_or(false));
            if !online { return Err(bad("агент цели не в сети")); }
            Ok(Json(json!({ "ok": true, "output": "агент в сети" })))
        }
    }
}

// ---------------------------------------------------------------- восстановление

#[derive(Deserialize)]
pub struct RestoreReq {
    instance_id: String,
    /// Площадка-источник (основная или принимающая); пусто — основная.
    #[serde(default)]
    site_id: String,
    snapshot_id: String,
    target_id: String,
    #[serde(default)]
    via: String,
    #[serde(default)]
    params: Value,
}

pub async fn restore(State(s): S, Extension(w): Extension<Who>, Json(q): Json<RestoreReq>) -> R<Json<Value>> {
    let i = s.instance(&q.instance_id)?;
    let t = s.store.read(|st| st.targets.iter().find(|t| t.id == q.target_id).cloned()).ok_or_else(|| bad("выберите цель восстановления"))?;
    if q.snapshot_id.trim().is_empty() { return Err(bad("выберите точку восстановления")); }
    let site = source_site(&s, &i, Some(&q.site_id))?;
    if t.kind == "agent" && !s.is_active() { return Err(bad("на принимающем узле агенты не подключены — восстановите на сервер по SSH или новую ВМ, или примите управление")); }
    let params = if q.params.is_object() { q.params.clone() } else { json!({}) };
    let spec = JobSpec { id: String::new(), kind: "restore".into(), instance: Some(s.runtime(&i)), site: Some(site), snapshot_id: Some(q.snapshot_id.clone()), mode: String::new(), params };
    let title = format!("{} — восстановление на «{}»", i.name, t.name);
    let jid = match t.kind.as_str() {
        "agent" => s.jobs.enqueue(spec, title, &t.agent_id, &w.user),
        k if k == "ssh" || super::vm::is_vm_kind(k) => {
            let id = s.jobs.enqueue(spec, title, "server", &w.user);
            s.spawn_local(id.clone(), Some((t.clone(), if q.via == "server" { "server".into() } else { "direct".into() })));
            id
        }
        _ => return Err(bad("тип цели не поддерживается")),
    };
    s.audit(&w.user, "restore", format!("{}: снапшот {} → {} ({})", i.name, q.snapshot_id, t.name, q.params.get("target_time").and_then(|v| v.as_str()).unwrap_or("")));
    Ok(Json(json!({ "job_id": jid })))
}

// ---------------------------------------------------------------- образы дисков (развернуть ВМ, скачать)

/// Одноразовая ссылка на скачивание образа диска (в памяти сервера).
#[derive(Clone)]
pub struct ImageGrant {
    pub instance_id: String,
    pub site_id: String,
    pub snapshot_id: String,
    pub disk: String,
    pub format: String,
    pub size: u64,
    pub name: String,
    pub expires: chrono::DateTime<Utc>,
    pub user: String,
}

#[derive(Deserialize)]
pub struct ImageQ {
    instance_id: String,
    #[serde(default)]
    site_id: String,
    snapshot_id: String,
}

/// Диски (образы) снапшота: имя и размер — для панели (развернуть/скачать/выгрузить).
pub async fn image_disks(State(s): S, Query(q): Query<ImageQ>) -> R<Json<Value>> {
    let i = s.instance(&q.instance_id)?;
    let site = source_site(&s, &i, Some(&q.site_id))?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let repo = Arc::new(crate::store::Repo::open(be, &site.password).await?);
    let (_, m) = repo.manifest(&q.snapshot_id).await?;
    let disks: Vec<Value> = m.images.iter().map(|img| json!({ "name": img.name, "size": img.size, "blocks": img.blocks })).collect();
    Ok(Json(json!({ "disks": disks, "machine": m.meta.get("machine").cloned().unwrap_or(Value::Null) })))
}

#[derive(Deserialize)]
pub struct ImageDlReq {
    instance_id: String,
    #[serde(default)]
    site_id: String,
    snapshot_id: String,
    disk: String,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize)]
pub struct ImgReqQ {
    /// Машина из «Серверы и ВМ» или экземпляр «Образ машины по SSH» (машина берётся из него).
    #[serde(default)]
    machine_id: String,
    #[serde(default)]
    instance_id: String,
    /// Площадка копий образа (для формы «Образ машины по SSH»).
    #[serde(default)]
    site_id: String,
    /// «Образы qcow2»: локальная папка, сколько наборов хранить там и на площадках, площадки (через запятую).
    #[serde(default)]
    dir: String,
    #[serde(default)]
    keep: Option<u64>,
    #[serde(default)]
    keep_remote: Option<u64>,
    #[serde(default)]
    sites: String,
    #[serde(default)]
    format: Option<String>,
}

/// Свободно на площадке (по последней проверке/репликации или диску сервера); `None` — неизвестно.
fn site_free(s: &Server, site_id: &str) -> Option<(u64, String)> {
    let site = s.store.read(|st| st.sites.iter().find(|x| x.id == site_id).cloned())?;
    if site.kind == "disk" {
        let d = site.disk.as_ref()?;
        let (t, u) = super::disk::usage(&d.path);
        return (t > 0).then(|| (t.saturating_sub(u), site.name.clone()));
    }
    s.site_disk.lock().get(site_id).and_then(|v| v["free"].as_u64()).map(|f| (f, site.name.clone()))
}

/// Свободно в папке на сервере (по ближайшему существующему каталогу) и на системном ли она диске.
fn dir_free(dir: &str) -> Option<(u64, bool, String)> {
    use std::os::unix::fs::MetadataExt;
    let p = std::path::Path::new(dir);
    if !p.is_absolute() { return None; }
    let at = p.ancestors().find(|a| a.is_dir())?;
    let (t, u) = crate::plugins::host::inventory::statvfs(&at.to_string_lossy());
    let root = std::fs::metadata("/").map(|m| m.dev()).ok()? == std::fs::metadata(at).map(|m| m.dev()).ok()?;
    Some((t.saturating_sub(u), root, at.to_string_lossy().to_string()))
}

/// Сколько места понадобится под образ машины — по описи её дисков (до первой копии): в хранилище площадки,
/// в локальной папке сервера под готовые образы и на площадках, куда копируются qcow2. И сколько там свободно.
pub async fn image_requirements(State(s): S, Query(q): Query<ImgReqQ>) -> R<Json<Value>> {
    let mid = if !q.machine_id.is_empty() { q.machine_id.clone() } else {
        let i = s.instance(&q.instance_id)?;
        if i.plugin != "machine" { return Err(bad("источник — экземпляр «Образ машины по SSH»")); }
        i.config["machine_id"].as_str().unwrap_or("").to_string()
    };
    if mid.is_empty() { return Ok(Json(json!({ "inventory": false, "machine": null, "basis": "none" }))); }
    let mut v = super::machines::image_size(&s, &mid)?;
    v["basis"] = json!(if v["inventory"] == true { "inventory" } else { "none" });
    // Копия уже есть — точнее по ней (ненулевые блоки последней копии), чем по описи.
    if !q.instance_id.is_empty() {
        let exact = async {
            let i = s.instance(&q.instance_id)?;
            let site = s.site_access(&i.site_id)?;
            let remote = s.tunnels.connect(&site, &|_| {}).await?;
            let repo = Arc::new(crate::store::Repo::open(crate::store::backend_for(&remote)?, &site.password).await?);
            let snaps = crate::plugins::appkit::snapshots_of(&repo, &i.id).await?;
            let Some(last) = snaps.last() else { return Ok::<_, anyhow::Error>(None) };
            let (_, m) = repo.manifest(&last.id).await?;
            let e = crate::plugins::image::formats::estimate(&repo, &m.images).await?;
            Ok(Some((last.time, e)))
        }.await;
        match exact {
            Ok(Some((t, e))) => {
                v["basis"] = json!("copy"); v["copy_time"] = json!(t);
                v["estimate"] = json!(e.total); v["largest"] = json!(e.largest);
                v["copy_disks"] = json!(e.disks.iter().map(|(n, size, used)| json!({ "name": n, "size": size, "used": used })).collect::<Vec<_>>());
            }
            Ok(None) => {}
            Err(e) => { v["copy_error"] = json!(format!("{e:#}")); }
        }
    }
    if v["basis"] == "none" { return Ok(Json(v)); }
    let est = v["estimate"].as_u64().unwrap_or(0);
    let largest = v["largest"].as_u64().unwrap_or(0);
    let check = |need: u64, free: Option<u64>| json!({ "need": need, "free": free, "ok": free.map(|f| f >= need) });
    // Хранилище копий образа: первая копия — все данные (сжатие zstd обычно уменьшает), дальше — изменения.
    let repo_site = if !q.site_id.is_empty() { q.site_id.clone() } else if !q.instance_id.is_empty() { s.instance(&q.instance_id).map(|i| i.site_id).unwrap_or_default() } else { String::new() };
    if !repo_site.is_empty() {
        let f = site_free(&s, &repo_site);
        let mut c = check(est, f.as_ref().map(|x| x.0));
        c["site"] = json!(f.map(|x| x.1).unwrap_or(repo_site));
        v["repo"] = c;
    }
    if !q.dir.is_empty() {
        let keep = q.keep.unwrap_or(3).max(1);
        let f = dir_free(&q.dir);
        let tmp = if q.format.as_deref() == Some("raw") { 0 } else { largest };
        let mut c = check(est * keep + tmp + (est * keep + tmp) / 20, f.as_ref().map(|x| x.0));
        c["dir"] = json!(q.dir); c["keep"] = json!(keep);
        c["system"] = json!(f.as_ref().map(|x| x.1).unwrap_or(false));
        c["at"] = json!(f.map(|x| x.2));
        v["local"] = c;
    }
    let keep_r = q.keep_remote.unwrap_or(2).max(1);
    v["remote"] = json!(q.sites.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|id| {
        let f = site_free(&s, id);
        let name = s.store.read(|st| st.sites.iter().find(|x| x.id == id).map(|x| x.name.clone())).unwrap_or_else(|| id.to_string());
        // На время приёма рядом с прошлым набором лежит новый — на один набор больше.
        let mut c = check(est * (keep_r + 1), f.map(|x| x.0));
        c["site"] = json!(name); c["keep"] = json!(keep_r);
        c
    }).collect::<Vec<_>>());
    Ok(Json(v))
}

/// Подключённые диски сервера управления (для выбора локальной папки образов).
pub async fn server_mounts() -> Json<Value> { Json(json!(super::imageship::mounts())) }

#[derive(Deserialize)]
pub struct EstimateQ {
    instance_id: String,
    #[serde(default)]
    format: Option<String>,
}

/// Сколько места займут образы последней копии экземпляра «Образ машины по SSH» — для формы продукта
/// «Образы qcow2» (до создания) и подсказок.
pub async fn image_estimate(State(s): S, Query(q): Query<EstimateQ>) -> R<Json<Value>> {
    let i = s.instance(&q.instance_id)?;
    if i.plugin != "machine" { return Err(bad("источник — экземпляр продукта «Образ машины по SSH»")); }
    let site = source_site(&s, &i, None)?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let repo = Arc::new(crate::store::Repo::open(be, &site.password).await?);
    let snaps = crate::plugins::appkit::snapshots_of(&repo, &i.id).await?;
    let Some(last) = snaps.last() else { return Ok(Json(json!({ "snapshot": null }))) };
    let (_, m) = repo.manifest(&last.id).await?;
    let est = crate::plugins::image::formats::estimate(&repo, &m.images).await?;
    let format = q.format.unwrap_or_else(|| "qcow2".into());
    Ok(Json(json!({
        "snapshot": last.id, "time": last.time, "format": format,
        "disks": est.disks.iter().map(|(n, size, used)| json!({ "name": n, "size": size, "used": used })).collect::<Vec<_>>(),
        "total": est.total, "largest": est.largest, "need": est.need(&format),
    })))
}

/// Подготовить скачивание: оценить размер и выдать одноразовую ссылку (только admin, запись в аудит).
pub async fn image_download(State(s): S, Extension(w): Extension<Who>, Json(q): Json<ImageDlReq>) -> R<Json<Value>> {
    let i = s.instance(&q.instance_id)?;
    let site = source_site(&s, &i, Some(&q.site_id))?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let repo = Arc::new(crate::store::Repo::open(be, &site.password).await?);
    let (_, m) = repo.manifest(&q.snapshot_id).await?;
    let img = m.images.iter().find(|x| x.name == q.disk).ok_or_else(|| bad("нет такого диска в снапшоте"))?.clone();
    // Размер до скачивания: ненулевые блоки × размер блока (столько примерно займёт файл образа).
    let ids = repo.read_image_map(&img).await?;
    let est = ids.iter().filter(|id| **id != crate::store::repo::ZERO_ID).count() as u64 * img.block as u64;
    let format = q.format.filter(|f| ["qcow2", "vmdk", "vhd", "raw"].contains(&f.as_str())).unwrap_or_else(|| "qcow2".into());
    let name = format!("{}-{}.{}", slug(&i.name), img.name, crate::plugins::image::formats::ext(&format));
    let token = crate::util::token();
    s.image_dl.lock().insert(token.clone(), ImageGrant {
        instance_id: q.instance_id, site_id: q.site_id, snapshot_id: q.snapshot_id.clone(), disk: q.disk.clone(),
        format: format.clone(), size: est, name: name.clone(), expires: Utc::now() + chrono::Duration::hours(2), user: w.user.clone(),
    });
    s.audit(&w.user, "image_download_prepare", format!("{} · {} · {} ({}, ~{})", i.name, q.snapshot_id, q.disk, format, crate::util::human_bytes(est)));
    Ok(Json(json!({ "token": token, "size": est, "name": name, "url": format!("/api/image/dl/{token}") })))
}

/// Отдать собранный файл образа по одноразовой ссылке (admin; запись в аудит). Ссылка действует один раз.
pub async fn image_dl(State(s): S, Path(token): Path<String>) -> R<Response> {
    // Путь открыт в middleware: авторизует сам одноразовый токен (выдан администратором через POST).
    let g = s.image_dl.lock().remove(&token).ok_or_else(|| bad("ссылка недействительна или уже использована"))?;
    if g.expires < Utc::now() { return Err(bad("ссылка просрочена — подготовьте скачивание заново")); }
    let i = s.instance(&g.instance_id)?;
    let site = source_site(&s, &i, Some(&g.site_id))?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let repo = Arc::new(crate::store::Repo::open(be, &site.password).await?);
    let (_, m) = repo.manifest(&g.snapshot_id).await?;
    let img = m.images.iter().find(|x| x.name == g.disk).ok_or_else(|| not_found("диск не найден в снапшоте"))?.clone();
    let dir = s.dir.join("downloads");
    std::fs::create_dir_all(&dir).ok();
    // Файл собирается на диске сервера: сначала — хватит ли места (для qcow2/vmdk/vhd — ещё временный raw).
    let need = (if g.format == "raw" { g.size } else { g.size * 2 }) + g.size / 20;
    if let Some(free) = crate::plugins::image::formats::free_bytes(&dir.to_string_lossy()) {
        if free < need {
            s.audit(&g.user, "image_download_failed", format!("{} · {}: на сервере свободно {}, нужно ~{}", i.name, g.disk, crate::util::human_bytes(free), crate::util::human_bytes(need)));
            return Err(bad(format!("на сервере управления недостаточно места для сборки образа: нужно ~{}, свободно {} в {}", crate::util::human_bytes(need), crate::util::human_bytes(free), dir.display())));
        }
    }
    let path = dir.join(format!("{token}.{}", crate::plugins::image::formats::ext(&g.format)));
    let bytes = match crate::plugins::image::formats::build(&repo, &img, &path, &g.format).await {
        Ok(b) => b,
        Err(e) => { let _ = std::fs::remove_file(&path); return Err(bad(format!("сборка образа: {e:#}"))); }
    };
    s.audit(&g.user, "image_download", format!("{} · {} · {} ({}, {})", i.name, g.snapshot_id, g.disk, g.format, crate::util::human_bytes(bytes)));
    let file = tokio::fs::File::open(&path).await.map_err(|e| bad(format!("файл образа: {e}")))?;
    // Снять имя сразу: данные живут, пока открыт дескриптор, и файл убирается после отдачи.
    let _ = std::fs::remove_file(&path);
    let stream = tokio_util::io::ReaderStream::new(file);
    Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-disposition", format!("attachment; filename=\"{}\"", g.name))
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| bad(format!("ответ: {e}")))
}

// ---------------------------------------------------------------- задания, аудит, пользователи

#[derive(Deserialize)]
pub struct JobsQ {
    instance_id: Option<String>,
    limit: Option<usize>,
}

pub async fn jobs(State(s): S, Query(q): Query<JobsQ>) -> Json<Vec<super::jobs::Job>> {
    Json(s.jobs.list(q.instance_id.as_deref().filter(|x| !x.is_empty()), q.limit.unwrap_or(200)))
}

/// Незащищённое: что агенты нашли на своих машинах, а экземпляра для этого нет (и не просили не предлагать).
fn suggestions_of(st: &super::state::State) -> Vec<Value> {
    let mut out = Vec::new();
    for a in &st.agents {
        for r in &a.found {
            let id = r["id"].as_str().unwrap_or_default();
            let cat = r["catalog_id"].as_str().unwrap_or_default();
            if id.is_empty() || cat.is_empty() || a.found_dismissed.iter().any(|d| d == id) { continue; }
            // Уже защищено: экземпляр того же продукта на этом агенте (для СУБД — на том же порту).
            let port = r["config"].get("port").cloned();
            if st.instances.iter().any(|i| i.agent_id == a.id && i.catalog_id == cat && (port.is_none() || i.config.get("port").is_none() || i.config.get("port") == port.as_ref())) { continue; }
            out.push(json!({ "agent_id": a.id, "agent_name": a.name, "id": id, "title": r["title"], "reason": r["reason"], "catalog_id": cat, "details": r["details"], "priority": r["priority"] }));
        }
    }
    out
}

pub async fn suggestions(State(s): S) -> Json<Value> {
    Json(json!(s.store.read(|st| suggestions_of(st))))
}

#[derive(Deserialize)]
pub struct SuggestReq {
    agent_id: String,
    id: String,
    #[serde(default)]
    site_id: String,
}

/// Защитить найденное: экземпляр с параметрами и расписанием из рекомендации.
pub async fn suggestion_apply(State(s): S, Extension(w): Extension<Who>, Json(q): Json<SuggestReq>) -> R<Json<Value>> {
    let (a, r) = s.store.read(|st| {
        let a = st.agents.iter().find(|a| a.id == q.agent_id).cloned();
        let r = a.as_ref().and_then(|a| a.found.iter().find(|r| r["id"] == q.id.as_str()).cloned());
        (a, r)
    });
    let a = a.ok_or_else(|| not_found("агент не найден"))?;
    let r = r.ok_or_else(|| bad("агент больше не видит этот продукт — обновите страницу"))?;
    if !s.store.read(|st| suggestions_of(st)).iter().any(|x| x["agent_id"] == a.id.as_str() && x["id"] == q.id.as_str()) {
        return Err(bad("этот продукт на сервере уже защищён"));
    }
    let site = if q.site_id.is_empty() { s.store.read(|st| if st.sites.len() == 1 { st.sites[0].id.clone() } else { String::new() }) } else { q.site_id.clone() };
    if site.is_empty() { return Err(bad("выберите площадку")); }
    let title = r["title"].as_str().unwrap_or("Продукт").to_string();
    let schedule = r["schedule"].as_str().filter(|x| !x.is_empty()).unwrap_or("0 1 * * *").to_string();
    let req = InstanceReq {
        name: format!("{} · {}", title.split(" (").next().unwrap_or(&title), a.name),
        catalog_id: r["catalog_id"].as_str().unwrap_or_default().to_string(),
        agent_id: a.id.clone(),
        site_id: site,
        config: r["config"].clone(),
        schedule: Some(super::state::Schedule { enabled: true, full: Some(schedule), dump: None, incr: None }),
        retention: Some(super::state::Retention { keep_last: r["keep_last"].as_u64().unwrap_or(7).max(1) as usize }),
        rpo_target_min: Some(r["rpo_min"].as_u64().filter(|x| *x > 0).unwrap_or(1440)),
        retry_mode: None,
        retry: None,
    };
    let inst = create_instance(&s, &w.user, req)?;
    Ok(Json(json!({ "ok": true, "id": inst.id, "name": inst.name })))
}

/// Не предлагать найденное (например, копируется другим способом).
pub async fn suggestion_dismiss(State(s): S, Extension(w): Extension<Who>, Json(q): Json<SuggestReq>) -> R<Json<Value>> {
    let name = s.store.update(|st| {
        let a = st.agents.iter_mut().find(|a| a.id == q.agent_id).ok_or_else(|| anyhow!("агент не найден"))?;
        if !a.found_dismissed.contains(&q.id) { a.found_dismissed.push(q.id.clone()); }
        Ok(a.name.clone())
    })?;
    s.audit(&w.user, "suggestion_dismiss", format!("{name}: {}", q.id));
    Ok(Json(json!({ "ok": true })))
}

/// Ошибка неактуальна: позже такое же задание того же экземпляра (вид и режим) завершилось успешно.
fn superseded(j: &super::jobs::Job, all: &[super::jobs::Job]) -> bool {
    let Some(inst) = j.instance_id.as_deref() else { return false };
    all.iter().any(|x| x.status == "ok" && x.created > j.created && x.instance_id.as_deref() == Some(inst) && x.kind == j.kind && x.mode == j.mode)
}

#[derive(Deserialize)]
pub struct AckReq {
    ids: Vec<String>,
}

/// Скрыть ошибки заданий с «Обзора» (задания и журналы остаются в разделе «Задания»).
pub async fn jobs_ack(State(s): S, Extension(w): Extension<Who>, Json(q): Json<AckReq>) -> R<Json<Value>> {
    let mut n = 0;
    for id in q.ids.iter().take(500) {
        let ok = s.jobs.get(id).map(|j| j.status == "failed" && j.ack.is_none()).unwrap_or(false);
        if ok && s.jobs.update(id, |j| j.ack = Some(super::jobs::JobAck { user: w.user.clone(), at: Utc::now() })).is_some() { n += 1; }
    }
    if n > 0 { s.audit(&w.user, "jobs_ack", format!("скрыто ошибок на обзоре: {n}")); }
    Ok(Json(json!({ "ok": true, "hidden": n })))
}

pub async fn job_get(State(s): S, Path(id): Path<String>) -> R<Json<super::jobs::Job>> {
    s.jobs.get(&id).map(Json).ok_or_else(|| not_found("задание не найдено"))
}

#[derive(Deserialize)]
pub struct LimitQ {
    limit: Option<usize>,
}

pub async fn audit(State(s): S, Query(q): Query<LimitQ>) -> Json<Vec<Value>> { Json(s.read_audit(q.limit.unwrap_or(500))) }

#[derive(Deserialize)]
pub struct AuditQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    format: String,
}

/// Страница журнала аудита: `{items, total, offset, limit}` (новые первыми; фильтр — на сервере, по всему журналу).
pub async fn audit_page(State(s): S, Query(q): Query<AuditQ>) -> Json<Value> {
    let all = s.audit_filtered(&q.q, &q.from, &q.to);
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let total = all.len();
    let items: Vec<Value> = all.into_iter().skip(q.offset).take(limit).collect();
    Json(json!({ "items": items, "total": total, "offset": q.offset, "limit": limit }))
}

fn csv_field(v: &str) -> String {
    // Формулы в ячейках (=, +, -, @) не исполняются в Excel — экранируются апострофом.
    let v = if v.starts_with(['=', '+', '-', '@']) { format!("'{v}") } else { v.to_string() };
    if v.contains([';', '"', '\n', '\r']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v }
}

/// Выгрузка журнала аудита (с тем же фильтром) файлом: CSV для Excel (UTF-8 с BOM, «;») или JSON.
pub async fn audit_export(State(s): S, Extension(w): Extension<Who>, Query(q): Query<AuditQ>) -> Response {
    let all = s.audit_filtered(&q.q, &q.from, &q.to);
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let host = crate::util::hostname().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>();
    s.audit(&w.user, "audit_export", format!("выгрузка журнала аудита ({}): записей {}{}", if q.format == "json" { "JSON" } else { "CSV" }, all.len(), if q.q.trim().is_empty() { String::new() } else { format!(", фильтр «{}»", q.q.trim()) }));
    if q.format == "json" {
        let body = serde_json::to_vec_pretty(&all).unwrap_or_default();
        return ([("content-type", "application/json; charset=utf-8".to_string()), ("content-disposition", format!("attachment; filename=\"audit-{host}-{stamp}.json\""))], body).into_response();
    }
    let mut out = String::from("\u{feff}Время;Пользователь;Действие;Подробности\r\n");
    for e in &all {
        let t = e["time"].as_str().and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok()).map(|d| d.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M:%S").to_string()).unwrap_or_default();
        out += &[t.as_str(), e["user"].as_str().unwrap_or(""), e["action"].as_str().unwrap_or(""), e["detail"].as_str().unwrap_or("")].iter().map(|x| csv_field(x)).collect::<Vec<_>>().join(";");
        out += "\r\n";
    }
    ([("content-type", "text/csv; charset=utf-8".to_string()), ("content-disposition", format!("attachment; filename=\"audit-{host}-{stamp}.csv\""))], out).into_response()
}

pub async fn settings(State(s): S) -> Json<Value> {
    Json(s.store.read(|st| json!({ "settings": st.settings, "default_excludes_builtin": crate::exclude::defaults() })))
}

#[derive(Deserialize)]
pub struct SettingsReq {
    #[serde(default)]
    default_excludes: Option<Vec<String>>,
    #[serde(default)]
    retry: Option<crate::server::state::RetryPolicy>,
    #[serde(default)]
    catch_up: Option<bool>,
    #[serde(default)]
    catch_up_hours: Option<u32>,
}

/// Изменить настройки: только переданные поля.
pub async fn settings_update(State(s): S, Extension(w): Extension<Who>, Json(q): Json<SettingsReq>) -> R<Json<Value>> {
    let ex: Option<Vec<String>> = q.default_excludes.as_ref().map(|v| v.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
    if let Some(ex) = &ex { for x in ex { if x.chars().any(|c| c.is_control()) { return Err(bad("недопустимые символы в исключениях")); } } }
    if let Some(r) = &q.retry { r.validate().map_err(bad)?; }
    if let Some(h) = q.catch_up_hours { if !(1..=168).contains(&h) { return Err(bad("догонять пропуски не старше 1–168 часов")); } }
    let mut what = Vec::new();
    s.store.update(|st| {
        if let Some(ex) = &ex { st.settings.default_excludes = ex.clone(); }
        if let Some(r) = &q.retry { st.settings.retry = r.clone(); }
        if let Some(c) = q.catch_up { st.settings.catch_up = c; }
        if let Some(h) = q.catch_up_hours { st.settings.catch_up_hours = h; }
        Ok(())
    })?;
    if let Some(ex) = &ex { what.push(format!("общие исключения: {}", ex.len())); }
    if let Some(r) = &q.retry { what.push(if r.enabled { format!("повторы: {} через {} мин", r.attempts, r.interval_min) } else { "повторы выключены".into() }); }
    if let Some(c) = q.catch_up { what.push(format!("догон пропусков: {}", if c { "да" } else { "нет" })); }
    s.audit(&w.user, "settings", what.join("; "));
    Ok(Json(json!({ "ok": true })))
}

pub async fn users(State(s): S) -> Json<Vec<Value>> {
    Json(s.store.read(|st| st.users.iter().map(|u| json!({ "name": u.name, "role": u.role, "email": u.email, "created": u.created })).collect()))
}

#[derive(Deserialize)]
pub struct UserReq {
    name: String,
    password: String,
    role: String,
    #[serde(default)]
    email: String,
}

fn check_email(e: &str) -> R<String> {
    let e = e.trim().to_string();
    if !e.is_empty() && !super::mail::valid_address(&e) { return Err(bad("неверный адрес почты")); }
    Ok(e)
}

pub async fn user_create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<UserReq>) -> R<Json<Value>> {
    if !["admin", "operator", "restore", "auditor"].contains(&q.role.as_str()) { return Err(bad("неизвестная роль")); }
    if q.name.trim().is_empty() || q.password.len() < 8 { return Err(bad("имя и пароль не короче 8 символов")); }
    if q.name.trim().len() > 64 || q.name.chars().any(|c| c.is_control() || c == '|') { return Err(bad("недопустимое имя пользователя")); }
    let email = check_email(&q.email)?;
    s.store.update(|st| {
        if st.users.iter().any(|u| u.name.eq_ignore_ascii_case(q.name.trim())) { return Err(anyhow!("пользователь уже есть")); }
        st.users.push(User { name: q.name.trim().into(), pw_hash: auth::hash_password(&q.password), role: q.role.clone(), email: email.clone(), session_gen: 0, created: Utc::now() });
        Ok(())
    })?;
    s.audit(&w.user, "user_create", format!("{} ({})", q.name, q.role));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PwReq {
    password: String,
}

/// Смена пароля закрывает открытые сеансы пользователя; себе — сразу новый токен сеанса.
pub async fn user_password(State(s): S, Extension(w): Extension<Who>, Path(name): Path<String>, Json(q): Json<PwReq>) -> R<Json<Value>> {
    if q.password.len() < 8 { return Err(bad("пароль не короче 8 символов")); }
    let gen = s.store.update(|st| {
        let u = st.users.iter_mut().find(|u| u.name == name).ok_or_else(|| anyhow!("пользователь не найден"))?;
        u.pw_hash = auth::hash_password(&q.password);
        u.session_gen += 1;
        Ok(u.session_gen)
    })?;
    s.audit(&w.user, "user_password", format!("{name}: пароль изменён, открытые сеансы закрыты"));
    let token = (name == w.user).then(|| auth::issue(&s.store.read(|st| st.session_secret.clone()), &name, gen));
    Ok(Json(json!({ "ok": true, "token": token })))
}

#[derive(Deserialize)]
pub struct UserUpdReq {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

pub async fn user_update(State(s): S, Extension(w): Extension<Who>, Path(name): Path<String>, Json(q): Json<UserUpdReq>) -> R<Json<Value>> {
    if let Some(r) = &q.role { if !["admin", "operator", "restore", "auditor"].contains(&r.as_str()) { return Err(bad("неизвестная роль")); } }
    let email = match &q.email { Some(e) => Some(check_email(e)?), None => None };
    s.store.update(|st| {
        let u = st.users.iter_mut().find(|u| u.name == name).ok_or_else(|| anyhow!("пользователь не найден"))?;
        if let Some(r) = &q.role {
            if *r != u.role && name == w.user { return Err(anyhow!("свою роль менять нельзя")); }
            u.role = r.clone();
        }
        if let Some(e) = &email { u.email = e.clone(); }
        if !st.users.iter().any(|u| u.role == "admin") { return Err(anyhow!("должен остаться хотя бы один администратор")); }
        Ok(())
    })?;
    s.audit(&w.user, "user_update", format!("{name}: {}{}", q.role.as_deref().map(|r| format!("роль {r}")).unwrap_or_default(), email.map(|e| format!(" почта «{e}»")).unwrap_or_default()));
    Ok(Json(json!({ "ok": true })))
}

pub async fn user_delete(State(s): S, Extension(w): Extension<Who>, Path(name): Path<String>) -> R<Json<Value>> {
    if name == w.user { return Err(bad("нельзя удалить себя")); }
    s.store.update(|st| {
        st.users.retain(|u| u.name != name);
        if !st.users.iter().any(|u| u.role == "admin") { return Err(anyhow!("должен остаться хотя бы один администратор")); }
        Ok(())
    })?;
    s.audit(&w.user, "user_delete", name);
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_nested() {
        let v = json!({ "url": "https://x", "token_secret": "abc", "nested": { "password": "p", "list": [{ "secret_key": "k", "x": 1 }] }, "empty_psk": "", "psk": "" });
        let m = mask(&v);
        assert_eq!(m["token_secret"], MASK);
        assert_eq!(m["nested"]["password"], MASK);
        assert_eq!(m["nested"]["list"][0]["secret_key"], MASK);
        assert_eq!(m["nested"]["list"][0]["x"], 1);
        assert_eq!(m["psk"], "");
        let mut back = m.clone();
        unmask(&mut back, &v);
        assert_eq!(back["token_secret"], "abc");
        assert_eq!(back["nested"]["password"], "p");
    }
}
