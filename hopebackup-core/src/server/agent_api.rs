//! Протокол агентов: подключение, длинный опрос, журнал и итог заданий.

use anyhow::anyhow;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::api::{ApiError, R};
use super::state::Agent;
use super::Server;
use crate::proto::{AgentReport, PollResp, WalShip};

type S = State<Arc<Server>>;

pub(crate) fn agent_of(s: &Server, h: &HeaderMap) -> R<Agent> {
    let tok = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let hash = crate::util::sha256_hex(tok);
    s.store.read(|st| st.agents.iter().find(|a| crate::util::ct_eq(&a.token_hash, &hash)).cloned()).ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "неизвестный агент".into()))
}

#[derive(Deserialize)]
pub struct EnrollReq {
    token: String,
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    version: String,
}

pub async fn enroll(State(s): S, Json(q): Json<EnrollReq>) -> R<Json<Value>> {
    let hash = crate::util::sha256_hex(&q.token);
    let agent_token = crate::util::token();
    let agent = s
        .store
        .update(|st| {
            let pos = st.enroll_tokens.iter().position(|e| crate::util::ct_eq(&e.token_hash, &hash) && e.expires > Utc::now()).ok_or_else(|| anyhow!("токен подключения неверный или истёк"))?;
            let et = st.enroll_tokens.remove(pos);
            let a = Agent {
                id: crate::util::short_id(),
                name: q.name.trim().to_string(),
                host: q.host.clone(),
                os: q.os.clone(),
                version: q.version.clone(),
                token_hash: crate::util::sha256_hex(&agent_token),
                enrolled: Utc::now(),
                last_seen: Some(Utc::now()),
                build: String::new(),
                arch: String::new(),
                update: Value::Null,
                caps: vec![],
                found: vec![],
                found_dismissed: vec![],
            };
            st.agents.push(a.clone());
            if !et.machine_id.is_empty() {
                if let Some(m) = st.machines.iter_mut().find(|m| m.id == et.machine_id) { m.agent_id = a.id.clone(); }
            }
            Ok(a)
        })
        .map_err(|e| ApiError(StatusCode::FORBIDDEN, e.to_string()))?;
    s.audit("system", "agent_enroll", format!("{} ({})", agent.name, agent.host));
    Ok(Json(json!({ "agent_id": agent.id, "agent_token": agent_token })))
}

fn wal_list(s: &Server, agent_id: &str) -> Vec<WalShip> {
    let insts = s.store.read(|st| st.instances.iter().filter(|i| i.agent_id == agent_id && i.plugin == "postgresql" && i.config.get("wal").and_then(|v| v.as_bool()).unwrap_or(true)).cloned().collect::<Vec<_>>());
    insts.into_iter().filter_map(|i| s.site_access(&i.site_id).ok().map(|site| WalShip { instance_id: i.id.clone(), site })).collect()
}

pub async fn poll(State(s): S, h: HeaderMap, Json(rep): Json<AgentReport>) -> R<Json<PollResp>> {
    let a = agent_of(&s, &h)?;
    if !s.is_active() {
        let c = s.cluster.read().clone();
        return Err(ApiError(StatusCode::CONFLICT, serde_json::to_string(&json!({ "standby": true, "active_url": c.active_url, "active_fp": c.active_fp, "epoch": c.epoch })).unwrap_or_default()));
    }
    // Связь в памяти: опрос идёт (длинный опрос до 25 с); по завершении или обрыву — время конца.
    struct Seen(Arc<Server>, String);
    impl Drop for Seen { fn drop(&mut self) { self.0.agent_seen.lock().insert(self.1.clone(), (Utc::now(), false)); } }
    s.agent_seen.lock().insert(a.id.clone(), (Utc::now(), true));
    let _seen = Seen(s.clone(), a.id.clone());
    // Отметка «в сети» без записи файла состояния на каждый опрос: пишем, только если прошло больше минуты.
    let stale = a.last_seen.map(|t| (Utc::now() - t).num_seconds() > 60).unwrap_or(true) || a.version != rep.version || a.host != rep.host
        || a.build != rep.build || a.update != rep.update.clone().unwrap_or(Value::Null) || a.caps != rep.caps || rep.found.as_ref().map(|f| *f != a.found).unwrap_or(false);
    if stale {
        let _ = s.store.update(|st| {
            if let Some(x) = st.agents.iter_mut().find(|x| x.id == a.id) {
                x.last_seen = Some(Utc::now());
                x.version = rep.version.clone();
                x.host = rep.host.clone();
                x.os = rep.os.clone();
                x.build = rep.build.clone();
                x.arch = rep.build.split('-').next().unwrap_or("").to_string();
                x.update = rep.update.clone().unwrap_or(Value::Null);
                x.caps = rep.caps.clone();
                if let Some(f) = &rep.found { x.found = f.clone(); }
            }
            Ok(())
        });
    }
    {
        let mut w = s.wal.lock();
        for st in &rep.wal { w.insert(st.instance_id.clone(), st.clone()); }
    }
    s.jobs.fail_stale(&a.id, &rep.running, &rep.pending, rep.started);
    let mut jobs = s.jobs.take_for(&a.id);
    if jobs.is_empty() {
        let n = s.jobs.notifier(&a.id);
        let _ = tokio::time::timeout(Duration::from_secs(25), n.notified()).await;
        jobs = s.jobs.take_for(&a.id);
    }
    // last_seen пишется на диск не чаще раза в минуту, «в сети» считается с запасом 90 с.
    let connectors = Some(s.store.read(|st| st.connectors.iter().map(|c| c.id.clone()).collect()));
    let epoch = s.cluster.read().epoch;
    Ok(Json(PollResp { jobs, wal: wal_list(&s, &a.id), connectors, epoch, servers: super::cluster::servers_for_agents(&s), server_port: s.listen_port }))
}

#[derive(Deserialize)]
pub struct LogReq {
    lines: Vec<String>,
}

fn own_job(s: &Server, a: &Agent, id: &str) -> R<()> {
    let j = s.jobs.get(id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "нет задания".into()))?;
    if j.agent_id != a.id { return Err(ApiError(StatusCode::FORBIDDEN, "чужое задание".into())); }
    Ok(())
}

pub async fn job_log(State(s): S, h: HeaderMap, Path(id): Path<String>, Json(q): Json<LogReq>) -> R<Json<Value>> {
    let a = agent_of(&s, &h)?;
    own_job(&s, &a, &id)?;
    s.jobs.log(&id, q.lines);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct FinishReq {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
    /// Строки журнала, не отправленные, пока сервер был недоступен.
    #[serde(default)]
    log: Vec<String>,
}

pub async fn job_finish(State(s): S, h: HeaderMap, Path(id): Path<String>, Json(q): Json<FinishReq>) -> R<Json<Value>> {
    let a = agent_of(&s, &h)?;
    own_job(&s, &a, &id)?;
    if !q.log.is_empty() { s.jobs.log(&id, q.log.into_iter().take(5000).collect()); }
    if let Some(j) = s.jobs.finish(&id, q.ok, q.result, q.error) {
        if j.kind == "backup" && j.status == "ok" {
            if let Some(inst) = &j.instance_id { s.after_backup(inst); }
        }
        super::migration::on_job_finished(&s, &j);
    }
    Ok(Json(json!({ "ok": true })))
}
