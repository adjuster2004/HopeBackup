//! Уведомления администраторам по почте (как в HopeDFS).
//!
//! События, которые администратор выбирает галочками в разделе «Почта и уведомления» (`EVENTS`), уходят
//! списку получателей через сервер почты из настроек. Источники вызывают `emit()` — письмо отправляется в
//! фоне и не задерживает работу. Состояния (агенты на связи, RPO, отправка WAL, место на дисках, узлы
//! управления) проверяет `monitor` раз в минуту; письмо — при переходе через порог и при возврате в норму,
//! повторно не шлётся (отметки — в `<server>/notify_state.json`). Журнал отправок — `<server>/notify_log.json`
//! (последние 200). Не больше 20 писем в час по одному событию.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

use super::Server;

/// Событие: идентификатор, группа, название, когда отправляется.
pub struct Event {
    pub id: &'static str,
    pub group: &'static str,
    pub title: &'static str,
    pub text: &'static str,
}

pub const EVENTS: &[Event] = &[
    Event { id: "backup_failed", group: "Резервное копирование", title: "Ошибка копирования", text: "Задание копирования экземпляра завершилось ошибкой: что, на каком агенте, текст ошибки и конец журнала." },
    Event { id: "backup_ok", group: "Резервное копирование", title: "Успешное копирование", text: "Каждое успешное копирование (объём, новые данные). По умолчанию выключено — писем будет много." },
    Event { id: "rpo", group: "Резервное копирование", title: "Нарушена цель RPO", text: "У экземпляра нет свежей копии дольше цели RPO (с запасом); и когда копии снова в норме." },
    Event { id: "wal", group: "Резервное копирование", title: "Архив WAL PostgreSQL", text: "Отправка WAL с ошибкой или очередь на агенте растёт; и когда отправка восстановилась." },
    Event { id: "restore", group: "Восстановление", title: "Восстановление", text: "Восстановление (на агента, сервер по SSH или новую ВМ) завершено успешно или с ошибкой." },
    Event { id: "agent_offline", group: "Агенты и машины", title: "Агент не на связи", text: "Агент не выходит на связь больше 10 минут; и когда снова на связи." },
    Event { id: "machine", group: "Агенты и машины", title: "Серверы и ВМ", text: "Опись или установка агента по SSH не удались; изменился ключ хоста машины (возможна подмена сервера)." },
    Event { id: "job_failed", group: "Агенты и машины", title: "Другие задания с ошибкой", text: "Обнаружение, подготовка репозитория, репликация площадок и прочие задания завершились ошибкой." },
    Event { id: "space", group: "Хранилище", title: "Мало места", text: "На диске сервера управления или площадки «диск сервера» свободно меньше 10%; и когда место освободилось." },
    Event { id: "cluster", group: "Узлы управления", title: "Узлы управления", text: "Узел управления не на связи больше 5 минут или снова на связи; управление перешло на другой узел." },
    Event { id: "updates", group: "Обновления", title: "Обновления", text: "Доступна новая версия (автопроверка); обновление сервера управления или агента установлено или не установлено (откат)." },
    Event { id: "password_reset", group: "Безопасность", title: "Восстановление доступа к панели", text: "Запрошена ссылка сброса пароля; пароль сменён по ссылке или аварийным токеном вендора." },
    Event { id: "login_failed", group: "Безопасность", title: "Подбор пароля", text: "5 и больше неудачных входов в панель за 15 минут для одной учётной записи или с одного адреса." },
];

/// Настройки уведомлений.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotifySettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default = "default_events")]
    pub events: Vec<String>,
}

impl Default for NotifySettings {
    fn default() -> Self { NotifySettings { enabled: false, recipients: vec![], events: default_events() } }
}

/// События, включённые по умолчанию.
pub fn default_events() -> Vec<String> { EVENTS.iter().filter(|e| e.id != "backup_ok").map(|e| e.id.to_string()).collect() }

pub fn title(id: &str) -> &'static str { EVENTS.iter().find(|e| e.id == id).map(|e| e.title).unwrap_or("Уведомление") }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: DateTime<Utc>,
    pub event: String,
    pub subject: String,
    pub to: Vec<String>,
    pub ok: bool,
    #[serde(default)]
    pub error: String,
}

struct Note {
    event: String,
    subject: String,
    body: String,
}

static TX: OnceLock<mpsc::UnboundedSender<Note>> = OnceLock::new();
/// Настройки уведомлений изменились: монитор забывает отметки и заново сообщает о текущих отклонениях
/// (иначе о проблеме, начавшейся до включения уведомлений, письма бы не было).
static RESET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn settings_changed() { RESET.store(true, std::sync::atomic::Ordering::SeqCst); }
static LOG: Mutex<Option<VecDeque<LogEntry>>> = parking_lot::const_mutex(None);
/// Отправок по событию за последний час (защита от лавины писем).
static RATE: Mutex<Option<HashMap<String, Vec<i64>>>> = parking_lot::const_mutex(None);
const RATE_PER_HOUR: usize = 20;
const LOG_MAX: usize = 200;

/// Отправить уведомление о событии (если оно включено). Не блокирует.
pub fn emit(event: &str, subject: impl Into<String>, body: impl Into<String>) {
    if let Some(tx) = TX.get() { let _ = tx.send(Note { event: event.to_string(), subject: subject.into(), body: body.into() }); }
}

fn log_path(s: &Server) -> std::path::PathBuf { s.dir.join("notify_log.json") }

pub fn push_log(s: &Server, e: LogEntry) {
    let mut g = LOG.lock();
    let l = g.get_or_insert_with(|| load_log(s));
    l.push_front(e);
    while l.len() > LOG_MAX { l.pop_back(); }
    let v: Vec<&LogEntry> = l.iter().collect();
    let _ = crate::util::write_json(&log_path(s), &v, true);
}

fn load_log(s: &Server) -> VecDeque<LogEntry> {
    std::fs::read(log_path(s)).ok().and_then(|b| serde_json::from_slice::<Vec<LogEntry>>(&b).ok()).map(VecDeque::from).unwrap_or_default()
}

pub fn log(s: &Server) -> Vec<LogEntry> {
    let mut g = LOG.lock();
    g.get_or_insert_with(|| load_log(s)).iter().cloned().collect()
}

fn rate_ok(event: &str) -> bool {
    let now = Utc::now().timestamp();
    let mut g = RATE.lock();
    let m = g.get_or_insert_with(HashMap::new);
    let v = m.entry(event.to_string()).or_default();
    v.retain(|t| now - *t < 3600);
    if v.len() >= RATE_PER_HOUR { return false; }
    v.push(now);
    true
}

/// Пределы для стендовых проверок: только вместе с HOPEBACKUP_TESTING=1.
fn test_secs(var: &str, default: i64) -> i64 {
    if std::env::var("HOPEBACKUP_TESTING").as_deref() != Ok("1") { return default; }
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Запуск: очередь отправки и монитор состояний.
pub fn start(s: Arc<Server>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Note>();
    if TX.set(tx).is_err() { return; }
    let a = s.clone();
    tokio::spawn(async move {
        while let Some(n) = rx.recv().await {
            let (mail, cfg) = a.store.read(|st| (st.mail.clone(), st.notify.clone()));
            if !cfg.enabled || !cfg.events.iter().any(|e| e == &n.event) || cfg.recipients.is_empty() { continue; }
            let subject = format!("[HopeBackup {}] {}", crate::util::hostname(), n.subject);
            if !rate_ok(&n.event) {
                tracing::warn!("уведомление «{}» не отправлено: больше {RATE_PER_HOUR} писем за час по событию «{}»", n.subject, title(&n.event));
                continue;
            }
            if !super::mail::configured(&mail) {
                push_log(&a, LogEntry { at: Utc::now(), event: n.event.clone(), subject, to: cfg.recipients.clone(), ok: false, error: "не настроен сервер исходящей почты".into() });
                continue;
            }
            let body = format!("{}{}", resolve_agents(&a, &n.body), super::mail::footer(&a));
            let subject = resolve_agents(&a, &subject);
            let r = super::mail::send_with(&mail, &cfg.recipients, &subject, &body).await;
            match &r {
                Ok(()) => tracing::info!("уведомление отправлено ({}): {subject}", cfg.recipients.join(", ")),
                Err(e) => tracing::warn!("уведомление «{subject}» не отправлено: {e}"),
            }
            push_log(&a, LogEntry { at: Utc::now(), event: n.event, subject, to: cfg.recipients.clone(), ok: r.is_ok(), error: r.err().unwrap_or_default() });
        }
    });
    tokio::spawn(monitor(s));
}

// ---------------------------------------------------------------- задания

/// Метки \u{1}agent:<id>\u{1} → «имя (id)».
fn resolve_agents(s: &Server, text: &str) -> String {
    if !text.contains('\u{1}') { return text.to_string(); }
    let agents = s.store.read(|st| st.agents.iter().map(|a| (a.id.clone(), a.name.clone())).collect::<Vec<_>>());
    let mut out = String::new();
    let mut parts = text.split('\u{1}');
    if let Some(first) = parts.next() { out.push_str(first); }
    for (i, p) in parts.enumerate() {
        if i % 2 == 0 {
            match p.strip_prefix("agent:") {
                Some(id) => match agents.iter().find(|(x, _)| x == id) { Some((_, n)) => out.push_str(&format!("«{n}» ({id})")), None => out.push_str(id) },
                None => out.push_str(p),
            }
        } else { out.push_str(p); }
    }
    out
}

fn tail(j: &super::jobs::Job, n: usize) -> String {
    let l: Vec<String> = j.log.iter().rev().take(n).map(|x| x.m.clone()).collect::<Vec<_>>().into_iter().rev().collect();
    if l.is_empty() { String::new() } else { format!("\n\nКонец журнала:\n{}", l.iter().map(|x| format!("  {x}")).collect::<Vec<_>>().join("\n")) }
}

/// Задание завершилось (вызывается из `Jobs::finish` на активном узле).
pub fn job_finished(j: &super::jobs::Job) {
    let ok = j.status == "ok";
    // Имя агента подставит отправитель (у него есть каталог): метка \u{1}agent:<id>\u{1}.
    let who = if j.agent_id == "server" { "сервер управления".to_string() } else { format!("агент \u{1}agent:{}\u{1}", j.agent_id) };
    let dur = match (j.started, j.finished) { (Some(a), Some(b)) => format!("{} с", (b - a).num_seconds().max(0)), _ => "—".into() };
    let err = j.error.clone().unwrap_or_default();
    let link = format!("Задание {} · {who} · длительность {dur}", j.id);
    match j.kind.as_str() {
        "backup" if ok => {
            let snap = &j.result["snapshot"];
            let size = snap["size"].as_u64().map(crate::util::human_bytes).unwrap_or_else(|| "—".into());
            let new = snap["new_bytes"].as_u64().map(crate::util::human_bytes).unwrap_or_else(|| "—".into());
            emit("backup_ok", format!("Копия готова: {}", j.title), format!("{}\n\nКопия {} создана. Объём {size}, новых данных {new}.\n{link}", j.title, snap["id"].as_str().unwrap_or("")));
        }
        // Копии по расписанию повторяются по политике: письмо об ошибке — когда повторов больше не будет
        // (его отправляет планировщик повторов, `backup_failed_final`).
        "backup" if super::jobs::retryable(j) => {}
        "backup" => emit("backup_failed", format!("Ошибка копирования: {}", j.title), format!("{}\n\nКопирование завершилось ошибкой:\n{err}\n\n{link}{}", j.title, tail(j, 12))),
        "restore" => emit("restore", format!("{}: {}", if ok { "Восстановление завершено" } else { "Ошибка восстановления" }, j.title),
            format!("{}\n\n{}\n\n{link}{}", j.title, if ok { "Восстановление завершено успешно.".to_string() } else { format!("Восстановление завершилось ошибкой:\n{err}") }, tail(j, 12))),
        k if k.starts_with("machine_") && !ok => emit("machine", format!("Ошибка: {}", j.title), format!("{}\n\n{err}\n\n{link}{}", j.title, tail(j, 8))),
        _ if !ok => emit("job_failed", format!("Ошибка задания: {}", j.title), format!("{}\n\n{err}\n\n{link}{}", j.title, tail(j, 8))),
        _ => {}
    }
}

/// Копия по расписанию не удалась и повторов больше не будет (попытки исчерпаны или повторы выключены).
pub fn backup_failed_final(j: &super::jobs::Job, attempts: u32, why: &str) {
    let who = if j.agent_id == "server" { "сервер управления".to_string() } else { format!("агент \u{1}agent:{}\u{1}", j.agent_id) };
    let err = j.error.clone().unwrap_or_default();
    emit("backup_failed", format!("Ошибка копирования: {}", j.title), format!("{}\n\nКопирование завершилось ошибкой (попыток: {attempts}; {why}):\n{err}\n\nЗадание {} · {who}{}", j.title, j.id, tail(j, 12)));
}

// ---------------------------------------------------------------- неудачные входы

static LOGIN_FAILS: Mutex<Option<HashMap<String, Vec<i64>>>> = parking_lot::const_mutex(None);

/// Неудачный вход: 5 за 15 минут по учётке или адресу — уведомление (одно на окно).
pub fn login_failed(user: &str, ip: &str) {
    let now = Utc::now().timestamp();
    let mut g = LOGIN_FAILS.lock();
    let m = g.get_or_insert_with(HashMap::new);
    for (key, what) in [(format!("u:{}", user.to_lowercase()), format!("учётная запись «{user}»")), (format!("ip:{ip}"), format!("адрес {ip}"))] {
        if key.ends_with(':') { continue; }
        let v = m.entry(key).or_default();
        v.retain(|t| now - *t < 900);
        v.push(now);
        if v.len() == 5 {
            emit("login_failed", format!("Подбор пароля: {what}"), format!("За последние 15 минут — 5 неудачных входов в панель ({what}; последняя попытка: учётная запись «{user}», адрес {ip}).\n\nЕсли это не ваши коллеги — проверьте журнал аудита и ограничьте доступ к порту панели."));
        }
    }
    if m.len() > 10_000 { m.retain(|_, v| v.iter().any(|t| now - *t < 900)); }
}

// ---------------------------------------------------------------- монитор состояний

fn state_path(s: &Server) -> std::path::PathBuf { s.dir.join("notify_state.json") }

fn load_flags(s: &Server) -> BTreeMap<String, i64> { std::fs::read(state_path(s)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default() }

/// Свободно на томе: (свободно, всего), байт.
fn volume_space(p: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 { return None; }
    Some((st.f_bavail as u64 * st.f_frsize as u64, st.f_blocks as u64 * st.f_frsize as u64))
}

fn hb(n: u64) -> String { crate::util::human_bytes(n) }

fn mins(sec: i64) -> String {
    if sec >= 2 * 86400 { format!("{} дн.", sec / 86400) } else if sec >= 7200 { format!("{} ч", sec / 3600) } else { format!("{} мин", (sec / 60).max(1)) }
}

/// Одна проверка; возвращает true, если отметки изменились.
fn check_once(s: &Server, flags: &mut BTreeMap<String, i64>) -> bool {
    let now = Utc::now();
    let ts = now.timestamp();
    let before = flags.clone();
    let st = s.store.get();
    if s.is_active() {
        // Агенты.
        let offline_after = test_secs("HOPEBACKUP_TEST_OFFLINE_SECS", 600);
        let live = s.agent_seen.lock().clone();
        for a in &st.agents {
            // Точнее всего — связь в памяти (идёт ли опрос сейчас); после перезапуска сервера — last_seen.
            let (seen, gone) = match (live.get(&a.id), a.last_seen) {
                (Some((_, true)), _) => (now, 0),
                (Some((t, false)), _) => (*t, (now - *t).num_seconds()),
                (None, Some(t)) => (t, (now - t).num_seconds()),
                (None, None) => continue,
            };
            let key = format!("agent:{}", a.id);
            if gone > offline_after && !flags.contains_key(&key) {
                flags.insert(key, ts);
                let insts: Vec<&str> = st.instances.iter().filter(|i| i.agent_id == a.id).map(|i| i.name.as_str()).collect();
                emit("agent_offline", format!("Агент не на связи: {}", a.name), format!("Агент «{}» ({}, {}) не выходит на связь {} (последний раз — {} UTC).\n\nКопии его экземпляров не создаются{}.\n\nПроверьте, что машина включена, служба hopebackup-agent запущена и сервер управления доступен с неё по порту {}.",
                    a.name, a.host, a.id, mins(gone), seen.format("%d.%m.%Y %H:%M"), if insts.is_empty() { String::new() } else { format!(": {}", insts.join(", ")) }, s.listen_port));
            } else if gone <= (offline_after / 2).min(60) && flags.remove(&key).is_some() {
                emit("agent_offline", format!("Агент снова на связи: {}", a.name), format!("Агент «{}» ({}) снова выходит на связь.", a.name, a.host));
            }
        }
        flags.retain(|k, _| !k.starts_with("agent:") || st.agents.iter().any(|a| format!("agent:{}", a.id) == *k));
        // RPO и WAL экземпляров.
        let jobs = s.jobs.all();
        for i in &st.instances {
            let rt = super::api::instance_view(s, i, &jobs)["runtime"].clone();
            let target = (i.rpo_target_min as i64 * 60).max(60);
            let rpo = rt["rpo_seconds"].as_i64();
            let key = format!("rpo:{}", i.id);
            // Запас: четверть цели, но не меньше 15 минут — чтобы не писать за минуту до очередной копии.
            let limit = target + (target / 4).max(900);
            let never_age = (now - i.created).num_seconds();
            let bad = match rpo { Some(r) => r > limit, None => i.schedule.enabled && never_age > limit };
            if bad && !flags.contains_key(&key) {
                flags.insert(key, ts);
                let why = match rpo { Some(r) => format!("последняя копия — {} назад", mins(r)), None => "успешных копий ещё нет".into() };
                let last_err = rt["last_job"]["error"].as_str().map(|e| format!("\nПоследнее задание: {e}")).unwrap_or_default();
                emit("rpo", format!("Нарушена цель RPO: {}", i.name), format!("Экземпляр «{}»: {why}, цель RPO — {}.{last_err}\n\nПроверьте расписание, агента и площадку экземпляра.", i.name, mins(target)));
            } else if !bad && rpo.map(|r| r <= target).unwrap_or(false) && flags.remove(&key).is_some() {
                emit("rpo", format!("RPO снова в норме: {}", i.name), format!("Экземпляр «{}»: свежая копия есть, RPO {} при цели {}.", i.name, mins(rpo.unwrap_or(0)), mins(target)));
            }
            if i.plugin == "postgresql" {
                let w = &rt["wal"];
                let key = format!("wal:{}", i.id);
                let err = w["last_error"].as_str().unwrap_or("").to_string();
                let queued = w["queued"].as_u64().unwrap_or(0);
                if (!err.is_empty() || queued > 50) && !flags.contains_key(&key) {
                    flags.insert(key, ts);
                    emit("wal", format!("Отправка WAL: {}", i.name), format!("Экземпляр «{}»: {}в очереди на агенте {queued} файлов WAL.\n\nПока WAL не уходит на площадку, восстановление на любой момент времени ограничено последним отправленным файлом, а каталог pg_wal на сервере СУБД растёт.",
                        i.name, if err.is_empty() { String::new() } else { format!("ошибка отправки: {err}; ") }));
                } else if err.is_empty() && queued <= 2 && !w.is_null() && flags.remove(&key).is_some() {
                    emit("wal", format!("Отправка WAL восстановлена: {}", i.name), format!("Экземпляр «{}»: WAL снова уходит на площадку, очередь {queued}.", i.name));
                }
            }
        }
        flags.retain(|k, _| !(k.starts_with("rpo:") || k.starts_with("wal:")) || st.instances.iter().any(|i| k.ends_with(&format!(":{}", i.id))));
        // Место: каталог сервера управления и площадки «диск сервера».
        let mut paths: Vec<(String, std::path::PathBuf)> = vec![("сервер управления".into(), s.dir.clone())];
        for site in &st.sites { if let Some(d) = &site.disk { paths.push((format!("площадка «{}»", site.name), std::path::PathBuf::from(&d.path))); } }
        let mut seen = std::collections::HashSet::new();
        for (what, p) in paths {
            use std::os::unix::fs::MetadataExt;
            let Ok(md) = std::fs::metadata(&p) else { continue };
            if !seen.insert(md.dev()) { continue; }
            let Some((free, total)) = volume_space(&p) else { continue };
            if total == 0 { continue; }
            let pct = free as f64 * 100.0 / total as f64;
            let key = format!("space:{}", md.dev());
            let low = test_secs("HOPEBACKUP_TEST_SPACE_PCT", 10) as f64;
            if pct < low && !flags.contains_key(&key) {
                flags.insert(key, ts);
                emit("space", format!("Мало места: {what}"), format!("{what}: на томе {} свободно {} из {} ({pct:.1}%).\n\nКогда место закончится, копии перестанут создаваться. Освободите место, уменьшите срок хранения копий или расширьте том.", p.display(), hb(free), hb(total)));
            } else if pct >= low + 5.0 && flags.remove(&key).is_some() {
                emit("space", format!("Место освободилось: {what}"), format!("{what}: на томе {} снова свободно {} из {} ({pct:.1}%).", p.display(), hb(free), hb(total)));
            }
        }
        // Узлы управления (принимающие) — по итогам синхронизации.
        let ps = s.peer_status.lock().clone();
        for p in &st.peers {
            let Some(v) = ps.get(&p.id) else { continue };
            let (pend, key) = (format!("peerfail:{}", p.id), format!("peer:{}", p.id));
            if v["ok"].as_bool() == Some(false) {
                let since = *flags.entry(pend).or_insert(ts);
                if ts - since > 300 && !flags.contains_key(&key) {
                    flags.insert(key, ts);
                    emit("cluster", format!("Узел управления не на связи: {}", p.name), format!("Принимающий узел управления «{}» ({}) не принимает каталог больше 5 минут.\nОшибка: {}\n\nПока узел недоступен, он не сможет принять управление при отказе этого узла.", p.name, p.url, v["error"].as_str().unwrap_or("")));
                }
            } else {
                flags.remove(&pend);
                if flags.remove(&key).is_some() {
                    emit("cluster", format!("Узел управления снова на связи: {}", p.name), format!("Принимающий узел управления «{}» ({}) снова принимает каталог.", p.name, p.url));
                }
            }
        }
    } else {
        // Принимающий узел: давно нет каталога от активного — он, вероятно, недоступен.
        let c = s.cluster.read().clone();
        let stale = c.last_sync.map(|t| (now - t).num_seconds() > 600).unwrap_or(false);
        if stale && !flags.contains_key("active") {
            flags.insert("active".into(), ts);
            emit("cluster", "Активный узел управления не на связи".to_string(), format!("Узел {} не получает каталог от активного узла {} больше 10 минут (последний раз — {} UTC).\n\nЕсли активный узел вышел из строя, примите управление на этом узле: раздел «Узлы управления» → «Принять управление».",
                crate::util::hostname(), c.active_url, c.last_sync.map(|t| t.format("%d.%m.%Y %H:%M").to_string()).unwrap_or_default()));
        } else if !stale && flags.remove("active").is_some() {
            emit("cluster", "Активный узел управления снова на связи".to_string(), format!("Узел {} снова получает каталог от активного узла {}.", crate::util::hostname(), c.active_url));
        }
    }
    *flags != before
}

async fn monitor(s: Arc<Server>) {
    tokio::time::sleep(std::time::Duration::from_secs(test_secs("HOPEBACKUP_TEST_NOTIFY_DELAY", 120) as u64)).await;
    let mut flags = load_flags(&s);
    let every = test_secs("HOPEBACKUP_TEST_NOTIFY_EVERY", 60).max(1) as u64;
    loop {
        if RESET.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // Отметки «вернулось в норму» не нужны: остаются только текущие отклонения, о них — заново.
            flags.clear();
        }
        let a = s.clone();
        let f0 = flags.clone();
        let res = tokio::task::spawn_blocking(move || { let mut f = f0; let c = check_once(&a, &mut f); (f, c) }).await;
        if let Ok((f, changed)) = res {
            flags = f;
            if changed { let _ = crate::util::write_json(&state_path(&s), &flags, true); }
        }
        tokio::time::sleep(std::time::Duration::from_secs(every)).await;
    }
}
