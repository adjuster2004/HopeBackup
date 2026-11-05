//! Задания: очередь для агентов, журнал выполнения, история.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::proto::JobSpec;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    pub t: DateTime<Utc>,
    pub m: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Агент-исполнитель или `server`.
    pub agent_id: String,
    pub status: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub started: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished: Option<DateTime<Utc>>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub log: Vec<LogLine>,
    #[serde(default)]
    pub user: String,
    /// Вид копии (full, dump, files…) — для повтора того же запуска.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// Откуда задание: schedule | retry | catchup | manual (пусто — вручную или служебное).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    /// Номер попытки (1 — первая).
    #[serde(default = "d_attempt")]
    pub attempt: u32,
    /// Итог не дошёл от исполнителя (перезапуск сервера, обрыв связи с агентом): поздний отчёт агента
    /// ещё будет принят.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    /// Повтор: когда запланирован и каким заданием выполнен (`skipped` — не понадобился).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_job: Option<String>,
    /// Решение о повторе уже принято (повтор назначен или попытки исчерпаны).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_decided: bool,
    /// Сервер перезапустился, пока задание выполнялось у агента: ждём отчёта агента до этого времени.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_until: Option<DateTime<Utc>>,
    /// Ошибку скрыли с «Обзора» (кто и когда); само задание и журнал остаются.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<JobAck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobAck {
    pub user: String,
    pub at: DateTime<Utc>,
}
fn d_attempt() -> u32 { 1 }

/// Задания копирования, которые повторяются по политике (вручную запущенные — нет: их итог видит оператор).
pub fn retryable(j: &Job) -> bool { j.kind == "backup" && matches!(j.origin.as_str(), "schedule" | "retry" | "catchup" | "manual_retry") }

struct Entry {
    job: Job,
    /// Спецификация с секретами — только в памяти, пока задание не выдано исполнителю.
    spec: Option<JobSpec>,
    /// Журнал завершённого задания — только в файле (в памяти не держим).
    offloaded: bool,
}

/// Сколько заданий держать в памяти; старые завершённые читаются из файлов по запросу.
const MEM_JOBS: usize = 2000;

fn done(j: &Job) -> bool { j.status == "ok" || j.status == "failed" }

pub struct Jobs {
    dir: PathBuf,
    map: Mutex<HashMap<String, Entry>>,
    /// Будить длинный опрос агента, когда для него появилось задание.
    notify: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
}

impl Jobs {
    pub fn open(dir: PathBuf) -> Jobs {
        let _ = std::fs::create_dir_all(&dir);
        let mut map = HashMap::new();
        let mut files: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        // Последние 1000 заданий — в памяти.
        let skip = files.len().saturating_sub(1000);
        for p in files.into_iter().skip(skip) {
            if let Ok(b) = std::fs::read(&p) {
                if let Ok(mut j) = serde_json::from_slice::<Job>(&b) {
                    if j.status == "running" && j.agent_id != "server" {
                        // Агент, скорее всего, продолжает: ждём его отчёта (он хранит итог, пока сервер не примет).
                        j.await_until = Some(Utc::now() + chrono::Duration::minutes(15));
                    } else if j.status == "running" || j.status == "queued" {
                        let was_queued = j.status == "queued";
                        j.status = "failed".into();
                        j.interrupted = true;
                        j.finished = Some(Utc::now());
                        j.error = Some(if was_queued { "не выдано исполнителю: сервер перезапустился".into() } else { "прервано перезапуском сервера".into() });
                    }
                    let off = done(&j) && !j.log.is_empty();
                    if off { j.log = Vec::new(); }
                    map.insert(j.id.clone(), Entry { job: j, spec: None, offloaded: off });
                }
            }
        }
        Jobs { dir, map: Mutex::new(map), notify: Mutex::new(HashMap::new()) }
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        (!id.is_empty() && id.len() < 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| self.dir.join(format!("{id}.json")))
    }

    fn read_file(&self, id: &str) -> Option<Job> { serde_json::from_slice(&std::fs::read(self.path(id)?).ok()?).ok() }

    /// Вернуть журнал из файла в память перед изменением задания.
    fn ensure_log(&self, e: &mut Entry) {
        if e.offloaded {
            e.job.log = self.read_file(&e.job.id).map(|j| j.log).unwrap_or_default();
            e.offloaded = false;
        }
    }

    /// После сохранения: журнал завершённого задания остаётся только в файле.
    fn offload(e: &mut Entry) {
        if done(&e.job) && !e.job.log.is_empty() {
            e.job.log = Vec::new();
            e.offloaded = true;
        }
    }

    /// Не держать в памяти больше `MEM_JOBS` заданий: убрать самые старые завершённые (файлы остаются).
    fn trim(m: &mut HashMap<String, Entry>) {
        if m.len() <= MEM_JOBS { return; }
        let mut old: Vec<(DateTime<Utc>, String)> = m.values().filter(|e| done(&e.job) && e.spec.is_none()).map(|e| (e.job.created, e.job.id.clone())).collect();
        old.sort();
        let n = m.len() - MEM_JOBS;
        for (_, id) in old.into_iter().take(n) { m.remove(&id); }
    }

    pub fn notifier(&self, agent: &str) -> Arc<tokio::sync::Notify> {
        self.notify.lock().entry(agent.to_string()).or_insert_with(|| Arc::new(tokio::sync::Notify::new())).clone()
    }

    pub fn new_id() -> String { format!("{}-{}", Utc::now().format("%Y%m%d%H%M%S"), &crate::util::short_id()[..6]) }

    /// Поставить задание в очередь агента (или `server` — выполнит сам сервер).
    pub fn enqueue(&self, mut spec: JobSpec, title: String, agent_id: &str, user: &str) -> String {
        if spec.id.is_empty() { spec.id = Self::new_id(); }
        let id = spec.id.clone();
        let job = Job {
            id: id.clone(),
            kind: spec.kind.clone(),
            title,
            instance_id: spec.instance.as_ref().map(|i| i.id.clone()),
            agent_id: agent_id.to_string(),
            status: "queued".into(),
            created: Utc::now(),
            started: None,
            finished: None,
            error: None,
            result: Value::Null,
            log: vec![],
            user: user.to_string(),
            mode: spec.mode.clone(),
            origin: String::new(),
            attempt: 1,
            interrupted: false,
            retry_at: None,
            retry_job: None,
            retry_decided: false,
            await_until: None,
            ack: None,
        };
        {
            let mut m = self.map.lock();
            m.insert(id.clone(), Entry { job: job.clone(), spec: Some(spec), offloaded: false });
            Self::trim(&mut m);
        }
        self.persist(&job);
        self.notifier(agent_id).notify_one();
        id
    }

    /// Выдать агенту его задания из очереди.
    pub fn take_for(&self, agent: &str) -> Vec<JobSpec> {
        let mut out = Vec::new();
        let mut m = self.map.lock();
        for e in m.values_mut() {
            if e.job.agent_id == agent && e.job.status == "queued" {
                if let Some(s) = e.spec.take() {
                    e.job.status = "running".into();
                    e.job.started = Some(Utc::now());
                    out.push(s);
                }
            }
        }
        out
    }

    /// Идентификаторы заданий, которые должен выполнить сам сервер (агент «server»): копия, обнаружение,
    /// действие — их запускает фоновый цикл через `spawn_local` (восстановление и создание репозитория
    /// запускаются отдельно при постановке).
    pub fn pending_local(&self) -> Vec<String> {
        let m = self.map.lock();
        m.values().filter(|e| e.job.agent_id == "server" && e.job.status == "queued" && e.spec.is_some()
            && matches!(e.job.kind.as_str(), "backup" | "discover" | "action"))
            .map(|e| e.job.id.clone()).collect()
    }

    /// Взять спецификацию задания, которое выполняет сам сервер.
    pub fn start_local(&self, id: &str) -> Option<JobSpec> {
        let mut m = self.map.lock();
        let e = m.get_mut(id)?;
        e.job.status = "running".into();
        e.job.started = Some(Utc::now());
        e.spec.take()
    }

    pub fn log(&self, id: &str, lines: Vec<String>) {
        let mut m = self.map.lock();
        if let Some(e) = m.get_mut(id) {
            self.ensure_log(e);
            let now = Utc::now();
            for l in lines { e.job.log.push(LogLine { t: now, m: l }); }
            if e.job.log.len() > 20_000 { let n = e.job.log.len() - 20_000; e.job.log.drain(..n); }
            // Запись в журнал завершённого задания (решение о повторе и т. п.) — сразу в файл.
            if done(&e.job) {
                self.persist(&e.job);
                Self::offload(e);
            }
        }
    }

    pub fn finish(&self, id: &str, ok: bool, result: Value, error: Option<String>) -> Option<Job> {
        let job = {
            let mut m = self.map.lock();
            let e = m.get_mut(id)?;
            self.ensure_log(e);
            // Поздний отчёт по заданию, которое считалось прерванным, принимается.
            if (e.job.status == "ok" || e.job.status == "failed") && !e.job.interrupted { let j = e.job.clone(); Self::offload(e); return Some(j); }
            if e.job.interrupted { e.job.log.push(LogLine { t: Utc::now(), m: "итог получен от исполнителя после перерыва связи".into() }); }
            e.job.interrupted = false;
            e.job.await_until = None;
            e.job.status = if ok { "ok".into() } else { "failed".into() };
            e.job.finished = Some(Utc::now());
            e.job.result = result;
            e.job.error = error;
            let j = e.job.clone();
            self.persist(&j);
            Self::offload(e);
            j
        };
        super::notify::job_finished(&job);
        crate::util::trim_memory();
        Some(job)
    }

    /// Агент вышел на связь без задания в работе и без неотправленного итога по нему: задание прервано
    /// (агент перезапустился). `pending` — задания, итог которых агент ещё отправит.
    /// `agent_started` — когда запущен процесс агента: задание, выданное до этого, он уже не выполняет.
    pub fn fail_stale(&self, agent: &str, running: &[String], pending: &[String], agent_started: Option<DateTime<Utc>>) {
        let restarted = |s: Option<DateTime<Utc>>| matches!((agent_started, s), (Some(a), Some(s)) if a > s);
        let ids: Vec<String> = self.map.lock().values().filter(|e| e.job.agent_id == agent && e.job.status == "running" && !running.contains(&e.job.id) && !pending.contains(&e.job.id) && (e.job.await_until.is_some() || restarted(e.job.started) || e.job.started.map(|s| (Utc::now() - s).num_seconds() > 60).unwrap_or(false))).map(|e| e.job.id.clone()).collect();
        for id in ids { self.interrupt(&id, "агент перезапустился во время выполнения"); }
    }

    /// Считать задание прерванным (итог не получен); поздний отчёт ещё будет принят.
    pub fn interrupt(&self, id: &str, why: &str) {
        let job = {
            let mut m = self.map.lock();
            let Some(e) = m.get_mut(id) else { return };
            if e.job.status != "running" && e.job.status != "queued" { return; }
            e.job.status = "failed".into();
            e.job.interrupted = true;
            e.job.await_until = None;
            e.job.finished = Some(Utc::now());
            e.job.error = Some(why.to_string());
            e.spec = None;
            let j = e.job.clone();
            self.persist(&j);
            Self::offload(e);
            j
        };
        super::notify::job_finished(&job);
    }

    /// Узел стал активным: выполняющиеся у агентов задания ждут их отчёта (как после перезапуска).
    pub fn await_running(&self) {
        let until = Utc::now() + chrono::Duration::minutes(15);
        for e in self.map.lock().values_mut() {
            if e.job.status == "running" && e.job.agent_id != "server" && e.job.await_until.is_none() { e.job.await_until = Some(until); }
        }
    }

    /// Задания, итог которых так и не пришёл после перезапуска сервера (агент не вышел на связь вовремя).
    pub fn expire_awaiting(&self) {
        let now = Utc::now();
        let ids: Vec<String> = self.map.lock().values().filter(|e| e.job.status == "running" && e.job.await_until.map(|t| t < now).unwrap_or(false)).map(|e| e.job.id.clone()).collect();
        for id in ids { self.interrupt(&id, "агент не сообщил итог после перезапуска сервера"); }
    }

    /// Изменить задание и сохранить.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut Job)) -> Option<Job> {
        let job = {
            let mut m = self.map.lock();
            let e = m.get_mut(id)?;
            self.ensure_log(e);
            f(&mut e.job);
            let j = e.job.clone();
            self.persist(&j);
            Self::offload(e);
            j
        };
        Some(job)
    }

    /// Сводка заданий от активного узла (на принимающем — только для просмотра).
    pub fn import(&self, jobs: Vec<Job>) {
        let mut changed = Vec::new();
        {
            let mut m = self.map.lock();
            for j in jobs {
                let fresh = match m.get(&j.id) { Some(e) => e.job.status != j.status || e.job.finished != j.finished, None => true };
                if fresh {
                    let log = match m.get_mut(&j.id) { Some(e) => { self.ensure_log(e); std::mem::take(&mut e.job.log) } None => Vec::new() };
                    let mut j = j;
                    if j.log.is_empty() { j.log = log; }
                    self.persist(&j);
                    changed.push(j.id.clone());
                    let mut e = Entry { job: j, spec: None, offloaded: false };
                    Self::offload(&mut e);
                    m.insert(e.job.id.clone(), e);
                }
            }
            Self::trim(&mut m);
        }
        let _ = changed;
    }

    /// Задание с журналом (журнал завершённого и старые задания — из файла).
    pub fn get(&self, id: &str) -> Option<Job> {
        let (j, off) = match self.map.lock().get(id) { Some(e) => (e.job.clone(), e.offloaded), None => return self.read_file(id) };
        if !off { return Some(j); }
        let mut j = j;
        j.log = self.read_file(id).map(|x| x.log).unwrap_or_default();
        Some(j)
    }

    pub fn list(&self, instance: Option<&str>, limit: usize) -> Vec<Job> {
        let m = self.map.lock();
        let mut v: Vec<Job> = m.values().filter(|e| instance.map(|i| e.job.instance_id.as_deref() == Some(i)).unwrap_or(true)).map(|e| { let mut j = e.job.clone(); j.log.clear(); j }).collect();
        v.sort_by(|a, b| b.created.cmp(&a.created));
        v.truncate(limit);
        v
    }

    pub fn all(&self) -> Vec<Job> { self.list(None, usize::MAX) }

    fn persist(&self, j: &Job) {
        let p = self.dir.join(format!("{}.json", j.id));
        if let Err(e) = crate::util::write_json(&p, j, false) { tracing::warn!("сохранение задания {}: {e}", j.id); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JobSpec { JobSpec { id: String::new(), kind: "backup".into(), instance: None, site: None, snapshot_id: None, mode: "full".into(), params: Value::Null } }

    /// Журнал завершённого задания — в файле, не в памяти; изменения после завершения его не теряют.
    #[test]
    fn finished_logs_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(dir.path().to_path_buf());
        let id = jobs.enqueue(spec(), "копия".into(), "server", "t");
        jobs.start_local(&id);
        jobs.log(&id, vec!["строка 1".into(), "строка 2".into()]);
        let j = jobs.finish(&id, false, Value::Null, Some("ошибка".into())).unwrap();
        assert_eq!(j.log.len(), 2, "итог возвращается с журналом (для письма)");
        assert!(jobs.map.lock()[&id].job.log.is_empty() && jobs.map.lock()[&id].offloaded);
        jobs.log(&id, vec!["повтор в 02:00".into()]);
        jobs.update(&id, |x| x.retry_decided = true);
        let g = jobs.get(&id).unwrap();
        assert_eq!(g.log.iter().map(|l| l.m.as_str()).collect::<Vec<_>>(), ["строка 1", "строка 2", "повтор в 02:00"]);
        assert!(g.retry_decided);
        // После перезапуска — то же; задания сверх предела в памяти читаются из файла.
        let jobs2 = Jobs::open(dir.path().to_path_buf());
        assert_eq!(jobs2.get(&id).unwrap().log.len(), 3);
        jobs2.map.lock().clear();
        assert_eq!(jobs2.get(&id).unwrap().log.len(), 3);
    }
}
