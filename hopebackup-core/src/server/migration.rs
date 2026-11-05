//! «Переезд»: проект переноса машины на другую платформу.
//!
//! Источник — экземпляр плагина «replica» на агенте машины: синхронизации по расписанию (только изменённые
//! блоки). Цель — агент на узле платформы (Proxmox) или рядом с ней (oVirt/zVirt): применяет синхронизации
//! к томам ВМ, которую создаёт при первом применении, и управляет ей (тест без сети, запуск, откат).
//!
//! Переключение: последняя синхронизация с остановкой служб и заморозкой ФС → применение → выключение
//! источника → запуск ВМ на цели (со снимком для отката, где хранилище умеет снимки).
//!
//! Шаги — обычные задания агентов; что делать дальше, сервер решает по итогу задания (`on_job_finished`).

use anyhow::{anyhow, bail, Result};
use axum::extract::{Path, State};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use super::api::{bad, mask, not_found, unmask, InstanceReq, R};
use super::auth::Who;
use super::jobs::Job;
use super::state::Schedule;
use super::Server;
use crate::proto::JobSpec;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Migration {
    pub id: String,
    pub name: String,
    /// Экземпляр «replica» на агенте источника.
    pub instance_id: String,
    /// Агент, применяющий изменения: на узле Proxmox или с доступом к Engine oVirt.
    pub target_agent: String,
    /// proxmox | ovirt
    pub platform: String,
    /// Параметры цели: хранилище, кластер, сети, имя ВМ, ресурсы; для oVirt — адрес и учётная запись.
    #[serde(default)]
    pub params: Value,
    /// Состояние цели от последнего шага (ВМ, тома, применённая синхронизация).
    #[serde(default)]
    pub state: Value,
    /// syncing | testing | cutover | running | done
    #[serde(default = "d_phase")]
    pub phase: String,
    /// Применять к цели каждую новую синхронизацию.
    #[serde(default = "d_true")]
    pub auto_apply: bool,
    /// Выключить источник перед запуском ВМ на цели (те же MAC и адреса).
    #[serde(default = "d_true")]
    pub shutdown_source: bool,
    /// Последняя синхронизация: {id, time, cutover}.
    #[serde(default)]
    pub last_sync: Value,
    /// Шаг, который выполняется сейчас: {job, step}.
    #[serde(default)]
    pub busy: Value,
    #[serde(default)]
    pub pending_apply: bool,
    #[serde(default)]
    pub events: Vec<Event>,
    pub created: DateTime<Utc>,
}
fn d_phase() -> String { "syncing".into() }
fn d_true() -> bool { true }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub t: DateTime<Utc>,
    pub what: String,
    #[serde(default)]
    pub job: String,
    #[serde(default = "d_true")]
    pub ok: bool,
}

const PLATFORMS: &[&str] = &["proxmox", "ovirt"];

fn view(s: &Server, m: &Migration) -> Value {
    let inst = s.instance(&m.instance_id).ok();
    let (src_agent, schedule, config) = inst.as_ref().map(|i| (i.agent_id.clone(), json!(i.schedule), mask(&i.config))).unwrap_or_default();
    let names = s.store.read(|st| st.agents.iter().map(|a| (a.id.clone(), a.name.clone())).collect::<std::collections::HashMap<_, _>>());
    let mut v = serde_json::to_value(m).unwrap_or_default();
    v["params"] = mask(&m.params);
    v["source_agent"] = json!(src_agent);
    v["source_agent_name"] = json!(names.get(&src_agent).cloned().unwrap_or_default());
    v["target_agent_name"] = json!(names.get(&m.target_agent).cloned().unwrap_or_default());
    v["schedule"] = schedule;
    v["source_config"] = config;
    v["site_id"] = json!(inst.as_ref().map(|i| i.site_id.clone()).unwrap_or_default());
    v["events"] = json!(m.events.iter().rev().take(50).collect::<Vec<_>>());
    v
}

fn get(s: &Server, id: &str) -> R<Migration> {
    s.store.read(|st| st.migrations.iter().find(|m| m.id == id).cloned()).ok_or_else(|| not_found("переезд не найден"))
}

fn save(s: &Server, id: &str, f: impl FnOnce(&mut Migration)) -> Option<Migration> {
    s.store.update(|st| {
        let m = st.migrations.iter_mut().find(|m| m.id == id).ok_or_else(|| anyhow!("переезд не найден"))?;
        f(m);
        Ok(m.clone())
    }).ok()
}

fn event(m: &mut Migration, what: impl Into<String>, job: &str, ok: bool) {
    m.events.push(Event { t: Utc::now(), what: what.into(), job: job.to_string(), ok });
    if m.events.len() > 500 { let n = m.events.len() - 500; m.events.drain(..n); }
}

pub async fn list(State(s): State<Arc<Server>>) -> Json<Vec<Value>> {
    let ms = s.store.read(|st| st.migrations.clone());
    Json(ms.iter().map(|m| view(&s, m)).collect())
}

pub async fn get_one(State(s): State<Arc<Server>>, Path(id): Path<String>) -> R<Json<Value>> { Ok(Json(view(&s, &get(&s, &id)?))) }

#[derive(Deserialize)]
pub struct CreateReq {
    name: String,
    source_agent: String,
    site_id: String,
    /// Параметры источника: disks, stop_services, final_freeze, freeze_timeout.
    #[serde(default)]
    source: Value,
    /// Расписание синхронизаций (cron); пусто — только вручную.
    #[serde(default)]
    sync_cron: Option<String>,
    target_agent: String,
    platform: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    auto_apply: Option<bool>,
    #[serde(default)]
    shutdown_source: Option<bool>,
}

pub async fn create(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Json(q): Json<CreateReq>) -> R<Json<Value>> {
    if !PLATFORMS.contains(&q.platform.as_str()) { return Err(bad("платформа цели: proxmox или ovirt (zVirt, РОСА)")); }
    if q.name.trim().is_empty() { return Err(bad("укажите имя переезда")); }
    if !s.store.read(|st| st.agents.iter().any(|a| a.id == q.target_agent)) { return Err(bad("выберите агента на стороне цели")); }
    check_params(&q.platform, &q.params)?;
    let cron = q.sync_cron.clone().filter(|c| !c.trim().is_empty());
    let inst = super::api::create_instance(&s, &w.user, InstanceReq {
        name: q.name.trim().to_string(), catalog_id: "replica".into(), agent_id: q.source_agent.clone(), site_id: q.site_id.clone(),
        config: if q.source.is_object() { q.source.clone() } else { json!({}) },
        schedule: Some(Schedule { enabled: cron.is_some(), full: cron, dump: None, incr: None }),
        retention: None, rpo_target_min: None, retry_mode: None, retry: None,
    })?;
    let m = Migration {
        id: inst.id.clone(), name: q.name.trim().to_string(), instance_id: inst.id.clone(), target_agent: q.target_agent, platform: q.platform,
        params: if q.params.is_object() { q.params } else { json!({}) }, state: Value::Null, phase: d_phase(),
        auto_apply: q.auto_apply.unwrap_or(true), shutdown_source: q.shutdown_source.unwrap_or(true),
        last_sync: Value::Null, busy: Value::Null, pending_apply: false, events: vec![], created: Utc::now(),
    };
    let m = s.store.update(|st| { st.migrations.push(m.clone()); Ok(m) })?;
    s.audit(&w.user, "migration_create", format!("{} → {}", m.name, m.platform));
    Ok(Json(view(&s, &m)))
}

fn check_params(platform: &str, p: &Value) -> R<()> {
    let has = |k: &str| p[k].as_str().is_some_and(|x| !x.trim().is_empty());
    if !has("storage") { return Err(bad(if platform == "proxmox" { "укажите хранилище Proxmox (например, local-lvm)" } else { "укажите домен хранения" })); }
    if platform == "ovirt" && (!has("url") || !has("cluster")) { return Err(bad("укажите адрес API Engine и кластер")); }
    Ok(())
}

#[derive(Deserialize)]
pub struct UpdateReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    auto_apply: Option<bool>,
    #[serde(default)]
    shutdown_source: Option<bool>,
    #[serde(default)]
    target_agent: Option<String>,
    /// Параметры источника (секреты — как в ответе: «********» — не менять).
    #[serde(default)]
    source: Option<Value>,
}

pub async fn update(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<UpdateReq>) -> R<Json<Value>> {
    let old = get(&s, &id)?;
    let mut params = q.params.clone().unwrap_or(old.params.clone());
    unmask(&mut params, &old.params);
    check_params(&old.platform, &params)?;
    // ВМ уже создана — платформу и место не меняем (состояние цели относится к ним).
    if !old.state.is_null() && q.target_agent.as_ref().is_some_and(|a| *a != old.target_agent) { return Err(bad("ВМ на цели уже создана — агент цели не меняется")); }
    let m = save(&s, &id, |m| {
        if let Some(n) = q.name.filter(|n| !n.trim().is_empty()) { m.name = n.trim().to_string(); }
        m.params = params;
        if let Some(a) = q.auto_apply { m.auto_apply = a; }
        if let Some(a) = q.shutdown_source { m.shutdown_source = a; }
        if let Some(a) = q.target_agent { m.target_agent = a; }
    }).ok_or_else(|| not_found("переезд не найден"))?;
    if let Some(mut src) = q.source.filter(|v| v.is_object()) {
        s.store.update(|st| {
            let i = st.instances.iter_mut().find(|i| i.id == m.instance_id).ok_or_else(|| anyhow!("экземпляр источника не найден"))?;
            unmask(&mut src, &i.config);
            if let (Some(c), Some(n)) = (i.config.as_object_mut(), src.as_object()) { for (k, v) in n { c.insert(k.clone(), v.clone()); } }
            Ok(())
        })?;
    }
    s.audit(&w.user, "migration_update", m.name.clone());
    Ok(Json(view(&s, &m)))
}

pub async fn delete(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    let m = get(&s, &id)?;
    if m.phase == "running" { return Err(bad("ВМ на цели работает после переключения — завершите переезд или откатите его")); }
    s.store.update(|st| { st.migrations.retain(|x| x.id != id); st.instances.retain(|i| i.id != m.instance_id); Ok(()) })?;
    s.audit(&w.user, "migration_delete", m.name);
    Ok(Json(json!({ "ok": true, "note": "ВМ на цели и копии в репозитории не удалялись" })))
}

/// Шаг переезда по запросу из панели.
pub async fn step(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Path((id, op)): Path<(String, String)>) -> R<Json<Value>> {
    let m = get(&s, &id)?;
    let busy_err = || bad(format!("выполняется шаг «{}» — дождитесь его окончания", m.busy["step"].as_str().unwrap_or("")));
    let job = match op.as_str() {
        "sync" => {
            if matches!(m.phase.as_str(), "cutover" | "running" | "done") { return Err(bad("переключение уже выполнено")); }
            let i = s.instance(&m.instance_id)?;
            let j = s.enqueue_backup(&i, "full", &w.user)?;
            s.jobs.update(&j, |x| x.origin = "manual".into());
            j
        }
        "prepare" => {
            let i = s.instance(&m.instance_id)?;
            let spec = JobSpec { id: String::new(), kind: "action".into(), instance: Some(s.runtime(&i)), site: None, snapshot_id: None, mode: "prepare-kvm".into(), params: Value::Null };
            s.jobs.enqueue(spec, format!("{} — подготовка к KVM", m.name), &i.agent_id, &w.user)
        }
        "apply" => {
            if !m.busy.is_null() { return Err(busy_err()); }
            if m.phase == "testing" { return Err(bad("идёт тестовый запуск — сначала завершите его")); }
            if matches!(m.phase.as_str(), "running" | "done") { return Err(bad("ВМ на цели уже работает")); }
            apply(&s, &m, "apply", &w.user)?
        }
        "test-start" | "test-stop" | "rollback" | "finish" => {
            if !m.busy.is_null() { return Err(busy_err()); }
            if m.state.is_null() { return Err(bad("ВМ на цели ещё не создана — сначала примените синхронизацию")); }
            match op.as_str() {
                "test-start" if m.phase != "syncing" => return Err(bad("тестовый запуск — до переключения")),
                "test-stop" if m.phase != "testing" => return Err(bad("тестовый запуск не идёт")),
                "rollback" | "finish" if m.phase != "running" => return Err(bad("откат и завершение — после переключения")),
                _ => {}
            }
            target_action(&s, &m, &op, json!({}), &w.user)?
        }
        "cutover" => {
            if !m.busy.is_null() { return Err(busy_err()); }
            if m.phase != "syncing" { return Err(bad(if m.phase == "testing" { "идёт тестовый запуск — сначала завершите его" } else { "переключение уже выполнено или идёт" })); }
            if m.state.is_null() { return Err(bad("сначала примените хотя бы одну синхронизацию (и проверьте ВМ тестовым запуском)")); }
            let i = s.instance(&m.instance_id)?;
            let j = s.enqueue_backup(&i, "cutover", &w.user)?;
            s.jobs.update(&j, |x| { x.origin = "migration".into(); x.title = format!("{} — переключение: последняя синхронизация", m.name); });
            // Синхронизации по расписанию больше не нужны.
            let _ = s.store.update(|st| { if let Some(i) = st.instances.iter_mut().find(|i| i.id == m.instance_id) { i.schedule.enabled = false; } Ok(()) });
            save(&s, &id, |x| { x.phase = "cutover".into(); x.busy = json!({ "job": j, "step": "cutover-sync" }); event(x, "переключение: последняя синхронизация (службы остановлены, ФС заморожены)", &j, true); });
            j
        }
        _ => return Err(bad("шаг: sync, prepare, apply, test-start, test-stop, cutover, rollback, finish")),
    };
    s.audit(&w.user, "migration_step", format!("{}: {op}", m.name));
    Ok(Json(json!({ "job_id": job })))
}

/// Параметры шага на стороне цели: параметры переезда, платформа и состояние.
fn target_params(m: &Migration, extra: Value) -> Value {
    let mut p = if m.params.is_object() { m.params.clone() } else { json!({}) };
    p["platform"] = json!(m.platform);
    if !m.state.is_null() { p["state"] = m.state.clone(); }
    if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) { for (k, v) in e { o.insert(k.clone(), v.clone()); } }
    p
}

/// Применить последнюю синхронизацию к цели.
fn apply(s: &Arc<Server>, m: &Migration, step: &str, user: &str) -> Result<String> {
    let snap = m.last_sync["id"].as_str().ok_or_else(|| anyhow!("синхронизаций ещё не было"))?.to_string();
    if m.state["applied"].as_str() == Some(snap.as_str()) && m.state["dirty"].as_bool() != Some(true) { bail!("синхронизация {snap} уже применена"); }
    let i = s.instance(&m.instance_id)?;
    let site = s.site_access(&i.site_id)?;
    let spec = JobSpec { id: String::new(), kind: "restore".into(), instance: Some(s.runtime(&i)), site: Some(site), snapshot_id: Some(snap.clone()), mode: String::new(), params: target_params(m, json!({})) };
    let j = s.jobs.enqueue(spec, format!("{} — применение синхронизации на цели", m.name), &m.target_agent, user);
    s.jobs.update(&j, |x| x.origin = "migration".into());
    let id = m.id.clone();
    save(s, &id, |x| { x.busy = json!({ "job": j, "step": step }); x.pending_apply = false; });
    Ok(j)
}

/// Операция с ВМ цели (`target-<op>`) на агенте цели.
fn target_action(s: &Arc<Server>, m: &Migration, op: &str, extra: Value, user: &str) -> Result<String> {
    let i = s.instance(&m.instance_id)?;
    let spec = JobSpec { id: String::new(), kind: "action".into(), instance: Some(s.runtime(&i)), site: None, snapshot_id: None, mode: format!("target-{op}"), params: target_params(m, extra) };
    let title = match op { "test-start" => "тестовый запуск ВМ без сети", "test-stop" => "окончание теста", "start" => "запуск ВМ на цели", "rollback" => "откат: остановка ВМ на цели", "finish" => "завершение переезда", o => o };
    let j = s.jobs.enqueue(spec, format!("{} — {title}", m.name), &m.target_agent, user);
    s.jobs.update(&j, |x| x.origin = "migration".into());
    let id = m.id.clone();
    save(s, &id, |x| x.busy = json!({ "job": j, "step": op }));
    Ok(j)
}

/// Источник — ВМ vSphere без агента (выключается при переключении сама, включается через vCenter).
fn agentless(s: &Server, m: &Migration) -> bool {
    s.instance(&m.instance_id).map(|i| i.config["source"] == "vsphere").unwrap_or(false)
}

/// Вернуть источник в работу: запустить остановленные службы или включить ВМ vSphere.
fn resume_source(s: &Arc<Server>, m: &Migration, user: &str) -> Result<()> {
    if agentless(s, m) || !m.shutdown_source { source_action(s, m, "resume", Value::Null, user)?; }
    Ok(())
}

fn source_action(s: &Arc<Server>, m: &Migration, name: &str, params: Value, user: &str) -> Result<String> {
    let i = s.instance(&m.instance_id)?;
    let spec = JobSpec { id: String::new(), kind: "action".into(), instance: Some(s.runtime(&i)), site: None, snapshot_id: None, mode: name.into(), params };
    let title = if name == "poweroff" { "выключение источника" } else { "возобновление работы источника" };
    let j = s.jobs.enqueue(spec, format!("{} — {title}", m.name), &i.agent_id, user);
    s.jobs.update(&j, |x| x.origin = "migration".into());
    Ok(j)
}

/// Итог задания: продвинуть переезд.
pub fn on_job_finished(s: &Arc<Server>, j: &Job) {
    let Some(m) = s.store.read(|st| st.migrations.iter().find(|m| m.busy["job"] == j.id.as_str() || (j.kind == "backup" && j.instance_id.as_deref() == Some(m.instance_id.as_str()))).cloned()) else { return };
    let ok = j.status == "ok";
    let step = if m.busy["job"] == j.id.as_str() { m.busy["step"].as_str().unwrap_or("").to_string() } else { String::new() };
    let err = j.error.clone().unwrap_or_default();
    let user = "migration";
    let r: Result<()> = (|| {
        match (j.kind.as_str(), step.as_str()) {
            ("backup", st) => {
                let cut = st == "cutover-sync";
                let snap = j.result["snapshot"]["id"].as_str().unwrap_or("").to_string();
                let m2 = save(s, &m.id, |x| {
                    if ok { x.last_sync = json!({ "id": snap, "time": Utc::now(), "cutover": cut }); }
                    event(x, if ok { format!("синхронизация {snap}{}", if cut { " (последняя, при остановленных службах)" } else { "" }) } else { format!("синхронизация не удалась: {err}") }, &j.id, ok);
                    if cut { x.busy = Value::Null; if !ok { x.phase = "syncing".into(); } }
                }).ok_or_else(|| anyhow!("переезд удалён"))?;
                if cut {
                    if !ok {
                        let _ = s.store.update(|st| { if let Some(i) = st.instances.iter_mut().find(|i| i.id == m.instance_id) { i.schedule.enabled = i.schedule.full.is_some(); } Ok(()) });
                        resume_source(s, &m2, user)?;
                        return Ok(());
                    }
                    apply(s, &m2, "cutover-apply", user)?;
                } else if ok && m2.auto_apply && m2.phase == "syncing" && !m2.state["testing"].is_object() {
                    if m2.busy.is_null() { apply(s, &m2, "apply", user)?; } else { save(s, &m.id, |x| x.pending_apply = true); }
                } else if ok && m2.auto_apply && (m2.phase == "testing" || !m2.busy.is_null()) {
                    save(s, &m.id, |x| x.pending_apply = true);
                }
            }
            ("restore", st) => {
                let applied = j.result["applied"].as_str().unwrap_or("").to_string();
                let m2 = save(s, &m.id, |x| {
                    x.busy = Value::Null;
                    if ok && j.result["state"].is_object() { x.state = j.result["state"].clone(); }
                    let blocks = j.result["blocks"].as_u64().unwrap_or(0);
                    event(x, if ok { format!("применена {applied} (синхронизаций {}, блоков {blocks})", j.result["syncs"]) } else { format!("применение не удалось: {err}") }, &j.id, ok);
                    if st == "cutover-apply" && !ok { x.phase = "syncing".into(); }
                }).ok_or_else(|| anyhow!("переезд удалён"))?;
                if st == "cutover-apply" {
                    if !ok {
                        let _ = s.store.update(|stt| { if let Some(i) = stt.instances.iter_mut().find(|i| i.id == m.instance_id) { i.schedule.enabled = i.schedule.full.is_some(); } Ok(()) });
                        resume_source(s, &m2, user)?;
                        return Ok(());
                    }
                    if m2.shutdown_source && !agentless(s, &m2) {
                        let delay = 20u64;
                        let pj = source_action(s, &m2, "poweroff", json!({ "delay": delay }), user)?;
                        save(s, &m.id, |x| { x.busy = json!({ "job": pj, "step": "poweroff" }); event(x, format!("источник выключается (через {delay} с)"), &pj, true); });
                    } else {
                        target_action(s, &m2, "start", json!({}), user)?;
                    }
                } else if ok && m2.pending_apply && m2.phase == "syncing" {
                    apply(s, &m2, "apply", user)?;
                }
            }
            ("action", "poweroff") => {
                // Выключение запланировано (или не удалось — тогда ВМ на цели не запускаем: конфликт адресов).
                let m2 = save(s, &m.id, |x| { x.busy = Value::Null; if !ok { event(x, format!("источник не выключен: {err} — выключите его и запустите ВМ на цели вручную (шаг «Запуск»)"), &j.id, false); } }).ok_or_else(|| anyhow!("переезд удалён"))?;
                if ok { target_action(s, &m2, "start", json!({ "start_delay": m2.params["start_delay"].as_u64().unwrap_or(30) }), user)?; }
            }
            ("action", op) if !op.is_empty() => {
                let back = if agentless(s, &m) { "ВМ источника включается через vCenter" } else if m.shutdown_source { "включите источник" } else { "службы источника запускаются снова" };
                let m2 = save(s, &m.id, |x| {
                    x.busy = Value::Null;
                    if ok && j.result["state"].is_object() { x.state = j.result["state"].clone(); }
                    if ok {
                        x.phase = match op { "test-start" => "testing", "test-stop" => "syncing", "start" => "running", "rollback" => "syncing", "finish" => "done", _ => x.phase.as_str() }.to_string();
                    }
                    let what = match op {
                        "test-start" => "тестовый запуск: ВМ на цели работает без сети",
                        "test-stop" => "тест окончен: тома цели возвращены к применённой синхронизации",
                        "start" => "ВМ на цели запущена — переезд выполнен; проверьте службы, затем «Завершить» (или «Откат»)",
                        "rollback" => if back.is_empty() { "" } else { "откат: ВМ на цели остановлена" },
                        "finish" => "переезд завершён",
                        o => o,
                    };
                    let mut what = if ok && x.state["dirty"] == true && (op == "test-stop" || op == "rollback") {
                        format!("{} (хранилище без снимков: тома будут переписаны при следующем применении)", what.split(':').next().unwrap_or(what))
                    } else { what.to_string() };
                    if op == "rollback" { what = format!("{what}; {back} — синхронизации продолжатся"); }
                    event(x, if ok { what } else { format!("{op}: {err}") }, &j.id, ok);
                }).ok_or_else(|| anyhow!("переезд удалён"))?;
                if ok && op == "rollback" {
                    let _ = s.store.update(|st| { if let Some(i) = st.instances.iter_mut().find(|i| i.id == m.instance_id) { i.schedule.enabled = i.schedule.full.is_some(); } Ok(()) });
                    resume_source(s, &m2, user)?;
                }
                if ok && op == "test-stop" && m2.pending_apply { apply(s, &m2, "apply", user)?; }
            }
            _ => {}
        }
        Ok(())
    })();
    if let Err(e) = r {
        let msg = format!("{e:#}");
        if !msg.contains("уже применена") { save(s, &m.id, |x| event(x, format!("следующий шаг не начат: {msg}"), &j.id, false)); }
    }
}
