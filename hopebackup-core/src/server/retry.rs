//! Повторы неудачных копий по расписанию и догон пропущенных запусков.
//!
//! **Повторы.** Копия по расписанию (или догоняющая, или сама повторная) завершилась ошибкой — через
//! `interval_min` минут ставится повтор того же вида копии, всего не больше `attempts` повторов (политика
//! экземпляра или общая из «Настроек»). Повтор не нужен, если за это время уже есть успешная копия
//! экземпляра. Если итог задания не дошёл (сервер перезапускался, связь с агентом рвалась), перед повтором
//! сервер ищет на площадке снапшот этого задания (в заголовке — идентификатор задания) и, если он есть,
//! засчитывает задание успешным. Письмо «Ошибка копирования» — только когда повторов больше не будет.
//!
//! **Догон.** Если запуск по расписанию пришёлся на время, когда сервер был выключен (или узел ещё не был
//! активным), копия запускается после старта — если пропуск не старше `catch_up_hours` и с тех пор не было
//! копий этого вида.

use chrono::{DateTime, Local, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::jobs::{retryable, Job};
use super::state::{Instance, RetryPolicy};
use super::Server;

/// Пределы для стендовых проверок: только вместе с HOPEBACKUP_TESTING=1.
fn test_num(var: &str, default: i64) -> i64 {
    if std::env::var("HOPEBACKUP_TESTING").as_deref() != Ok("1") { return default; }
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Секунд в «минуте» политики (на стенде можно ускорить).
fn minute() -> i64 { test_num("HOPEBACKUP_TEST_MINUTE_SECS", 60).max(1) }

pub fn policy(s: &Server, inst: &Instance) -> RetryPolicy {
    inst.retry.clone().unwrap_or_else(|| s.store.read(|st| st.settings.retry.clone()))
}

pub async fn retry_loop(s: Arc<Server>) {
    // Сначала агенты выходят на связь и досылают итоги, затем — решения о повторах и догон.
    tokio::time::sleep(Duration::from_secs(test_num("HOPEBACKUP_TEST_RETRY_DELAY", 90).max(0) as u64)).await;
    let every = Duration::from_secs(test_num("HOPEBACKUP_TEST_RETRY_EVERY", 20).max(1) as u64);
    let catch_every = test_num("HOPEBACKUP_TEST_CATCHUP_EVERY", 300).max(1);
    let mut last_catch: Option<std::time::Instant> = None;
    let mut tried: HashMap<String, DateTime<Local>> = HashMap::new();
    loop {
        if s.is_active() {
            s.jobs.expire_awaiting();
            decide(&s);
            due(&s).await;
            if last_catch.map(|t| t.elapsed().as_secs() as i64 >= catch_every).unwrap_or(true) {
                catch_up(&s, &mut tried);
                last_catch = Some(std::time::Instant::now());
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// Решить о повторе для только что завершившихся с ошибкой копий.
fn decide(s: &Arc<Server>) {
    for j in s.jobs.all().into_iter().filter(|j| retryable(j) && j.status == "failed" && !j.retry_decided) {
        let Some(inst) = j.instance_id.as_ref().and_then(|id| s.instance(id).ok()) else {
            s.jobs.update(&j.id, |x| x.retry_decided = true);
            continue;
        };
        let p = policy(s, &inst);
        let total = if p.enabled { 1 + p.attempts } else { 1 };
        if j.attempt < total {
            let at = j.finished.unwrap_or_else(Utc::now) + chrono::Duration::seconds(p.interval_min as i64 * minute());
            let msg = format!("повтор в {} (попытка {} из {total})", at.with_timezone(&Local).format("%H:%M"), j.attempt + 1);
            s.jobs.update(&j.id, |x| { x.retry_decided = true; x.retry_at = Some(at); });
            s.jobs.log(&j.id, vec![msg]);
            tracing::info!("{}: ошибка копирования, повтор в {at}", inst.name);
        } else {
            s.jobs.update(&j.id, |x| x.retry_decided = true);
            let why = if !p.enabled { "повторы выключены".to_string() } else { format!("повторы исчерпаны ({} из {})", p.attempts, p.attempts) };
            s.jobs.log(&j.id, vec![format!("повторов больше не будет: {why}")]);
            // С журналом (в списке заданий его нет) — в письме будет конец журнала.
            let full = s.jobs.get(&j.id).unwrap_or_else(|| j.clone());
            super::notify::backup_failed_final(&full, j.attempt, &why);
        }
    }
}

fn busy(jobs: &[Job], inst: &str) -> bool {
    jobs.iter().any(|j| j.kind == "backup" && j.instance_id.as_deref() == Some(inst) && (j.status == "queued" || j.status == "running"))
}

/// Снапшот задания на площадке экземпляра (итог дошёл до площадки, но не до сервера).
pub async fn snapshot_of_job(s: &Arc<Server>, inst: &Instance, job_id: &str) -> anyhow::Result<Option<crate::store::types::SnapshotHeader>> {
    let site = s.site_access(&inst.site_id)?;
    let remote = s.tunnels.connect(&site, &|_| {}).await?;
    let be = crate::store::backend_for(&remote)?;
    let snaps = be.list_snapshots(Some(&inst.id)).await?;
    Ok(snaps.into_iter().find(|h| h.extra.get("job").and_then(|v| v.as_str()) == Some(job_id)))
}

/// Поставить повторы, время которых пришло.
async fn due(s: &Arc<Server>) {
    let now = Utc::now();
    let all = s.jobs.all();
    for j in all.iter().filter(|j| retryable(j) && j.retry_decided && j.retry_job.is_none() && j.retry_at.map(|t| t <= now).unwrap_or(false)) {
        let Some(inst) = j.instance_id.as_ref().and_then(|id| s.instance(id).ok()) else {
            s.jobs.update(&j.id, |x| x.retry_job = Some("skipped".into()));
            continue;
        };
        // Успешная копия уже есть (вручную или по расписанию) — повтор не нужен.
        if let Some(ok) = all.iter().find(|x| x.kind == "backup" && x.instance_id == j.instance_id && x.status == "ok" && x.created > j.created) {
            s.jobs.update(&j.id, |x| x.retry_job = Some("skipped".into()));
            s.jobs.log(&j.id, vec![format!("повтор не нужен: копия уже снята заданием {}", ok.id)]);
            continue;
        }
        // Итог не дошёл: может быть, снапшот на площадке есть.
        if j.interrupted {
            match snapshot_of_job(s, &inst, &j.id).await {
                Ok(Some(h)) => {
                    s.jobs.update(&j.id, |x| x.retry_job = Some("skipped".into()));
                    s.jobs.log(&j.id, vec![format!("на площадке найден снапшот этого задания {} — копия засчитана", h.id)]);
                    s.jobs.finish(&j.id, true, json!({ "snapshot": h, "reconciled": true }), None);
                    s.after_backup(&inst.id);
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("сверка задания {} с площадкой: {e:#}", j.id),
            }
        }
        if busy(&all, &inst.id) { continue; }
        let mode = if j.mode.is_empty() { if inst.plugin == "files" { "files".to_string() } else { "full".to_string() } } else { j.mode.clone() };
        match s.enqueue_backup(&inst, &mode, "повтор") {
            Ok(id) => {
                let attempt = j.attempt + 1;
                s.jobs.update(&id, |x| { x.origin = "retry".into(); x.attempt = attempt; });
                s.jobs.log(&id, vec![format!("повтор задания {} (попытка {attempt})", j.id)]);
                s.jobs.update(&j.id, |x| x.retry_job = Some(id.clone()));
                tracing::info!("{}: повтор копии (попытка {attempt}) → {id}", inst.name);
            }
            Err(e) => {
                // Площадка или агент не заданы — повтор не выполнить: считаем попытку неудачной.
                s.jobs.update(&j.id, |x| x.retry_job = Some("skipped".into()));
                s.jobs.log(&j.id, vec![format!("повтор не поставлен: {e:#}")]);
                let full = s.jobs.get(&j.id).unwrap_or_else(|| j.clone());
                super::notify::backup_failed_final(&full, j.attempt, &format!("повтор не поставлен: {e:#}"));
            }
        }
    }
}

/// Последнее время запуска по расписанию, которое должно было случиться после `anchor`, если оно уже прошло.
pub fn missed_since(expr: &str, anchor: DateTime<Local>, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let c = croner::Cron::new(expr).parse().ok()?;
    let next = c.find_next_occurrence(&anchor, false).ok()?;
    (next < now).then_some(next)
}

/// Догнать запуски по расписанию, пропущенные, пока сервер не работал.
fn catch_up(s: &Arc<Server>, tried: &mut HashMap<String, DateTime<Local>>) {
    let (on, hours) = s.store.read(|st| (st.settings.catch_up, st.settings.catch_up_hours));
    if !on { return; }
    let now = Local::now();
    let all = s.jobs.all();
    for inst in s.store.read(|st| st.instances.clone()) {
        if !inst.schedule.enabled { continue; }
        let main_kind = if inst.plugin == "files" { "files" } else { "full" };
        for (expr, kind) in [(inst.schedule.full.clone(), main_kind), (inst.schedule.dump.clone(), "dump"), (inst.schedule.incr.clone(), "incr")] {
            let Some(expr) = expr.filter(|e| !e.trim().is_empty()) else { continue };
            // Опора — последнее задание этого вида (любой итог) или создание экземпляра.
            let last = all.iter().filter(|j| j.kind == "backup" && j.instance_id.as_deref() == Some(inst.id.as_str()) && (j.mode.is_empty() || j.mode == kind)).map(|j| j.created).max();
            let anchor = last.unwrap_or(inst.created).max(inst.created).with_timezone(&Local);
            let Some(missed) = missed_since(&expr, anchor, now - chrono::Duration::minutes(2)) else { continue };
            if (now - missed).num_hours() >= hours as i64 { continue; }
            let key = format!("{}|{kind}", inst.id);
            if tried.get(&key) == Some(&missed) { continue; }
            if busy(&all, &inst.id) { continue; }
            tried.insert(key, missed);
            match s.enqueue_backup(&inst, kind, "догон расписания") {
                Ok(id) => {
                    s.jobs.update(&id, |x| x.origin = "catchup".into());
                    s.jobs.log(&id, vec![format!("запуск по расписанию в {} пропущен (сервер управления не работал) — копия догоняет", missed.format("%d.%m %H:%M"))]);
                    tracing::info!("догон расписания: {} ({kind}, пропуск {missed}) → {id}", inst.name);
                }
                Err(e) => tracing::warn!("догон расписания {}: {e:#}", inst.name),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    #[test]
    fn missed() {
        let at = |h: u32, m: u32| Local.with_ymd_and_hms(2026, 10, 25, h, m, 0).unwrap();
        // Последняя копия вчера в 01:00 (опора 24-го), сейчас 25-го 03:00: запуск 25-го 01:00 пропущен.
        let anchor = Local.with_ymd_and_hms(2026, 10, 24, 1, 0, 30).unwrap();
        assert_eq!(missed_since("0 1 * * *", anchor, at(3, 0)), Some(at(1, 0)));
        // Сейчас 00:30 — следующий запуск ещё впереди.
        assert_eq!(missed_since("0 1 * * *", anchor, at(0, 30)), None);
        // Копия уже была сегодня в 01:00.
        assert_eq!(missed_since("0 1 * * *", at(1, 0), at(3, 0)), None);
    }
}
