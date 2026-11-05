//! Агент: работает на хосте с продуктом. Опрашивает сервер управления, выполняет задания
//! (копия, восстановление, обнаружение), держит туннели к площадкам и непрерывно отправляет
//! сегменты WAL из локального спула.

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::connectors::TunnelManager;
use crate::plugins::LogFn;
use crate::proto::*;
use crate::store::{backend_for, Repo};

/// Каталог данных агента/сервера (`HOPEBACKUP_DATA`, по умолчанию `/var/lib/hopebackup`).
pub fn data_dir() -> PathBuf {
    std::env::var("HOPEBACKUP_DATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/var/lib/hopebackup"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub server: String,
    pub fingerprint: String,
    pub agent_id: String,
    pub token: String,
    pub name: String,
    /// Все узлы управления кластера (приходят с опросом): при недоступности текущего агент переключается.
    #[serde(default)]
    pub servers: Vec<ServerRef>,
    /// Наибольшая виденная эпоха: задания узла с меньшей эпохой (прежний активный) не выполняются.
    #[serde(default)]
    pub epoch: u64,
}

impl AgentConfig {
    /// Следующий узел управления после текущего (по кругу).
    fn next_server(&self, prefer: Option<&str>) -> Option<ServerRef> {
        let mut list: Vec<ServerRef> = self.servers.clone();
        if !list.iter().any(|x| x.url == self.server) { list.insert(0, ServerRef { url: self.server.clone(), fingerprint: self.fingerprint.clone() }); }
        if let Some(p) = prefer.filter(|p| !p.is_empty() && *p != self.server) {
            if let Some(x) = list.iter().find(|x| x.url == p) { return Some(x.clone()); }
        }
        if list.len() < 2 { return None; }
        let i = list.iter().position(|x| x.url == self.server).unwrap_or(0);
        Some(list[(i + 1) % list.len()].clone())
    }
}

/// Тот же адрес с другим портом: https://h:7543 → https://h:4443.
fn with_port(url: &str, port: u16) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (hostport, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "") };
    let host = if hostport.starts_with('[') { &hostport[..hostport.find(']')? + 1] } else { hostport.rsplit_once(':').map(|x| x.0).unwrap_or(hostport) };
    let cur: u16 = hostport.rsplit_once(':').and_then(|x| x.1.parse().ok()).unwrap_or(if scheme == "https" { 443 } else { 80 });
    if cur == port { return Some(url.to_string()); }
    Some(format!("{scheme}://{host}:{port}{path}"))
}

async fn health_ok(url: &str, fp: &str) -> bool {
    let Ok(c) = crate::tlsutil::client(if fp.is_empty() { None } else { Some(fp) }) else { return false };
    c.get(format!("{}/api/health", url.trim_end_matches('/'))).timeout(Duration::from_secs(10)).send().await.map(|r| r.status().is_success()).unwrap_or(false)
}

fn config_path() -> PathBuf { data_dir().join("agent.json") }

pub fn load_config() -> Result<AgentConfig> {
    let b = std::fs::read(config_path()).with_context(|| format!("агент не подключён к серверу ({} нет) — выполните `hopebackup agent enroll`", config_path().display()))?;
    Ok(serde_json::from_slice(&b)?)
}

/// Подключить агента к серверу управления по одноразовому токену.
pub async fn enroll(server: &str, token: &str, fingerprint: Option<&str>, name: Option<&str>) -> Result<AgentConfig> {
    let server = server.trim_end_matches('/').to_string();
    let fp = match fingerprint.map(|s| s.to_string()).filter(|s| !s.is_empty()) {
        Some(f) => f,
        None => {
            let f = crate::tlsutil::probe_fingerprint(&format!("{server}/api/health")).await?;
            println!("Отпечаток сертификата сервера: {f}");
            println!("Сверьте его с панелью (Агенты → Подключить агент). Чтобы не спрашивать, передайте --fingerprint.");
            f
        }
    };
    let client = crate::tlsutil::client(Some(&fp))?;
    let name = name.map(|s| s.to_string()).unwrap_or_else(crate::util::hostname);
    let resp = client
        .post(format!("{server}/api/agent/enroll"))
        .json(&serde_json::json!({ "token": token, "name": name, "host": crate::util::hostname(), "os": os_name(), "version": crate::VERSION }))
        .send()
        .await?;
    if !resp.status().is_success() { bail!("сервер отказал: {}", resp.text().await.unwrap_or_default()); }
    let v: serde_json::Value = resp.json().await?;
    let cfg = AgentConfig {
        server,
        fingerprint: fp,
        agent_id: v["agent_id"].as_str().unwrap_or_default().to_string(),
        token: v["agent_token"].as_str().unwrap_or_default().to_string(),
        name,
        servers: vec![],
        epoch: 0,
    };
    if cfg.agent_id.is_empty() || cfg.token.is_empty() { bail!("неверный ответ сервера"); }
    crate::util::write_json(&config_path(), &cfg, true)?;
    Ok(cfg)
}

#[derive(Default)]
struct Shared {
    wal: RwLock<Vec<WalShip>>,
    wal_status: Mutex<HashMap<String, WalStatus>>,
    running: Mutex<HashSet<String>>,
    /// Проверенное обновление: передать установщику после отправки итога задания.
    handoff: Mutex<Option<crate::updater::Request>>,
    /// Найденные на машине продукты (рекомендации облегчённой описи); None — ещё не смотрели.
    found: Mutex<Option<Vec<serde_json::Value>>>,
}

/// Что на машине стоит защищать: продукты из облегчённой описи → рекомендации «создать экземпляр»
/// (без «сервера целиком» и дисков — это решается описью в «Серверах и ВМ»).
fn look_around() -> Vec<serde_json::Value> {
    let inv = crate::plugins::host::inventory::light();
    crate::plugins::host::analysis::analyze(&inv, true).recommendations.into_iter()
        // Второстепенное (priority low: nginx, haproxy, openvpn — конфигурация и так в копии сервера) не предлагаем.
        .filter(|r| r.action == "instance" && r.available && r.priority != "low" && r.catalog_id != "host" && !r.catalog_id.is_empty() && !r.id.starts_with("mount"))
        .map(|r| serde_json::json!({ "id": r.id, "title": r.title, "reason": r.reason, "catalog_id": r.catalog_id, "config": r.config,
            "schedule": r.schedule, "rpo_min": r.rpo_min, "keep_last": r.keep_last, "details": r.details, "priority": r.priority }))
        .collect()
}

/// Раз в 6 часов (первый раз — через минуту после запуска) посмотреть, что работает на машине.
async fn looker(shared: Arc<Shared>) {
    // HOPEBACKUP_LOOK_DELAY — для проверок (по умолчанию минута после запуска).
    let first = std::env::var("HOPEBACKUP_LOOK_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    tokio::time::sleep(Duration::from_secs(first)).await;
    loop {
        match tokio::task::spawn_blocking(look_around).await {
            Ok(v) => { *shared.found.lock() = Some(v); }
            Err(e) => tracing::warn!("опись продуктов на машине: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}

pub async fn run(cfg: AgentConfig) -> Result<()> {
    let mut cfg = cfg;
    crate::util::PATIENT.store(true, std::sync::atomic::Ordering::Relaxed);
    let agent_started = chrono::Utc::now();
    let mut client = crate::tlsutil::client(Some(&cfg.fingerprint))?;
    let tunnels = TunnelManager::default();
    let shared = Arc::new(Shared::default());
    tokio::spawn(wal_shipper(shared.clone(), tunnels.clone()));
    tokio::spawn(looker(shared.clone()));
    tracing::info!("агент «{}» ({}): сервер {}", cfg.name, cfg.agent_id, cfg.server);
    // Снимки ФС, оставшиеся от прерванных копий (агент перезапускался посреди копии).
    let _ = tokio::task::spawn_blocking(|| crate::fssnap::cleanup_stale(&(std::sync::Arc::new(|m: &str| tracing::warn!("{m}")) as LogFn))).await;
    let n = outbox_ids().len();
    if n > 0 { tracing::info!("неотправленных итогов заданий: {n} — отправлю, когда сервер управления ответит"); }
    let mut backoff = 2u64;
    let mut fails = 0u32;
    let mut port_retry: Option<std::time::Instant> = None;
    // Новая версия после обновления на связи с сервером (TLS с закреплённым отпечатком) — знак установщику,
    // что она работает. Опрос — длинный (до 25 с), поэтому сначала быстрая проверка /api/health.
    let mut alive_marked = false;
    if client.get(format!("{}/api/health", cfg.server)).timeout(Duration::from_secs(10)).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
        alive_marked = true;
        mark_alive(agent_started);
    }
    // Переключиться на другой узел управления (и запомнить его).
    let switch = |cfg: &mut AgentConfig, client: &mut reqwest::Client, to: ServerRef, why: &str| {
        tracing::warn!("переключение на узел управления {} ({why})", to.url);
        cfg.server = to.url;
        cfg.fingerprint = to.fingerprint;
        if let Ok(c) = crate::tlsutil::client(Some(&cfg.fingerprint)) { *client = c; }
        let _ = crate::util::write_json(&config_path(), cfg, true);
    };
    loop {
        // Сначала — итоги заданий, сохранённые, пока сервер был недоступен.
        deliver_outbox(&client, &cfg).await;
        let report = AgentReport {
            version: crate::VERSION.into(),
            host: crate::util::hostname(),
            os: os_name(),
            wal: shared.wal_status.lock().values().cloned().collect(),
            running: shared.running.lock().iter().cloned().collect(),
            pending: outbox_ids(),
            started: Some(agent_started),
            build: crate::dist::this_target(),
            update: crate::updater::load::<serde_json::Value>(&update_dir().join("status.json")),
            caps: crate::proto::AGENT_CAPS.iter().map(|c| c.to_string()).collect(),
            found: shared.found.lock().clone(),
        };
        let r = client.post(format!("{}/api/agent/poll", cfg.server)).bearer_auth(&cfg.token).json(&report).timeout(Duration::from_secs(45)).send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                backoff = 2;
                fails = 0;
                if !alive_marked { alive_marked = true; mark_alive(agent_started); }
                let p: PollResp = match resp.json().await { Ok(p) => p, Err(e) => { tracing::warn!("ответ сервера: {e}"); continue; } };
                if p.epoch > 0 && p.epoch < cfg.epoch {
                    // Прежний активный узел вернулся: его задания не выполняем.
                    tracing::warn!("узел {} отвечает с эпохой {} < {} — задания не выполняются", cfg.server, p.epoch, cfg.epoch);
                    match cfg.next_server(None) { Some(n) => switch(&mut cfg, &mut client, n, "устаревшая эпоха"), None => tokio::time::sleep(Duration::from_secs(10)).await }
                    continue;
                }
                if p.epoch > cfg.epoch || (!p.servers.is_empty() && p.servers != cfg.servers) {
                    cfg.epoch = cfg.epoch.max(p.epoch);
                    if !p.servers.is_empty() { cfg.servers = p.servers.clone(); }
                    let _ = crate::util::write_json(&config_path(), &cfg, true);
                }
                *shared.wal.write() = p.wal;
                if let Some(keep) = p.connectors { tunnels.retain(&keep).await; }
                for job in p.jobs {
                    let (client, cfg, tunnels, shared) = (client.clone(), cfg.clone(), tunnels.clone(), shared.clone());
                    tokio::spawn(async move { execute(client, cfg, tunnels, shared, job).await });
                }
                // Сервер переехал на другой порт (7543 → 4443): сначала задания этого ответа (они уже отмечены
                // на сервере как выполняемые), затем проверить новый адрес и перейти на него. Неудачная
                // проверка — повтор не раньше чем через 10 минут.
                if p.server_port > 0 && port_retry.map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(true) {
                    if let Some(new_url) = with_port(&cfg.server, p.server_port) {
                        if new_url != cfg.server {
                            if health_ok(&new_url, &cfg.fingerprint).await {
                                tracing::info!("сервер управления перешёл на порт {}: {} → {new_url}", p.server_port, cfg.server);
                                let fp = cfg.fingerprint.clone();
                                switch(&mut cfg, &mut client, ServerRef { url: new_url, fingerprint: fp }, "новый порт сервера");
                                let _ = crate::util::write_json(&config_path(), &cfg, true);
                                port_retry = None;
                            } else {
                                tracing::warn!("сервер сообщает порт {}, но {new_url} недоступен — остаёмся на {}", p.server_port, cfg.server);
                                port_retry = Some(std::time::Instant::now());
                            }
                        }
                    }
                }
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::CONFLICT => {
                // Это принимающий узел: он подсказывает активный.
                let v: serde_json::Value = resp.json().await.unwrap_or_default();
                let info: serde_json::Value = v["error"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
                let active = info["active_url"].as_str().unwrap_or("").to_string();
                let afp = info["active_fp"].as_str().unwrap_or("").to_string();
                let to = cfg.next_server(Some(&active)).or_else(|| (!active.is_empty() && !afp.is_empty()).then(|| ServerRef { url: active.clone(), fingerprint: afp.clone() }));
                match to {
                    Some(n) if n.url != cfg.server => switch(&mut cfg, &mut client, n, "узел принимающий"),
                    _ => tokio::time::sleep(Duration::from_secs(15)).await,
                }
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                tracing::error!("сервер не принимает токен агента (агент удалён?) — повтор через минуту");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok(resp) => {
                tracing::warn!("сервер ответил {}", resp.status());
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
            Err(e) => {
                fails += 1;
                tracing::warn!("нет связи с сервером {}: {e}", cfg.server);
                if fails >= 3 {
                    if let Some(n) = cfg.next_server(None) { fails = 0; backoff = 2; switch(&mut cfg, &mut client, n, "текущий недоступен"); continue; }
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

async fn execute(client: reqwest::Client, cfg: AgentConfig, tunnels: TunnelManager, shared: Arc<Shared>, job: JobSpec) {
    shared.running.lock().insert(job.id.clone());
    let buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let b2 = buf.clone();
    let jid = job.id.clone();
    let log: LogFn = Arc::new(move |s: &str| {
        tracing::info!("[{jid}] {s}");
        let mut b = b2.lock();
        b.push(s.to_string());
        // Сервер долго недоступен: храним последние строки.
        if b.len() > 5000 { let n = b.len() - 5000; b.drain(..n); }
    });
    let url = format!("{}/api/agent/jobs/{}", cfg.server, job.id);
    let flusher = {
        let (client, buf, url, token) = (client.clone(), buf.clone(), url.clone(), cfg.token.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                flush(&client, &buf, &url, &token).await;
            }
        })
    };
    let r = if job.kind == "self_update" { self_update(&client, &cfg, &shared, &job, &log).await }
        else if job.kind == "install_pkgs" { let l = log.clone(); crate::connectors::net::install_for(&job.mode, &move |m: &str| l(m)).await }
        else { crate::exec::run_job(&job, &tunnels, log.clone()).await };
    flusher.abort();
    if let Err(e) = &r { log(&format!("ошибка: {e:#}")); }
    flush(&client, &buf, &url, &cfg.token).await;
    // Строки журнала, которые не ушли (сервер недоступен), — вместе с итогом.
    let rest: Vec<String> = std::mem::take(&mut *buf.lock());
    let mut body = match &r {
        Ok(v) => serde_json::json!({ "ok": true, "result": v }),
        Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
    };
    if !rest.is_empty() { body["log"] = serde_json::json!(rest); }
    // Итог сначала на диск: если сервер недоступен (или агент перезапустится), он уйдёт позже.
    let saved = save_outbox(&job.id, &body);
    let mut delivered = false;
    for _ in 0..6 {
        if send_finish(&client, &cfg, &job.id, &body).await { delivered = true; break; }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    if delivered { if saved { remove_outbox(&job.id); } } else { tracing::warn!("[{}] сервер управления недоступен — итог сохранён и будет отправлен, когда он ответит", job.id); }
    shared.running.lock().remove(&job.id);
    // Обновление: итог уже у сервера — теперь установщик заменит программу и перезапустит службу.
    let pending = shared.handoff.lock().take();
    if let Some(req) = pending {
        if let Err(e) = crate::updater::handoff(&update_dir(), &req) {
            tracing::warn!("обновление агента не передано установщику: {e:#}");
            crate::updater::update_job(&req.status, |j| { j.phase = "failed".into(); j.running = false; j.finished = true; j.failed = true; j.message = format!("{e:#}"); });
        }
    }
    crate::util::trim_memory();
}

// ---------------------------------------------------------------- неотправленные итоги заданий

fn outbox_dir() -> std::path::PathBuf { data_dir().join("outbox") }

fn valid_job_id(id: &str) -> bool { !id.is_empty() && id.len() < 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') }

fn save_outbox(id: &str, body: &serde_json::Value) -> bool {
    if !valid_job_id(id) { return false; }
    let _ = std::fs::create_dir_all(outbox_dir());
    match crate::util::write_json(&outbox_dir().join(format!("{id}.json")), body, true) {
        Ok(()) => true,
        Err(e) => { tracing::warn!("итог задания {id} не сохранён на диск: {e:#}"); false }
    }
}

fn remove_outbox(id: &str) { let _ = std::fs::remove_file(outbox_dir().join(format!("{id}.json"))); }

/// Задания, итог которых ещё не принят сервером.
pub fn outbox_ids() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(outbox_dir()).map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(|x| x.to_string())).filter(|x| valid_job_id(x)).collect()).unwrap_or_default();
    v.sort();
    v
}

/// Отправить итог; true — сервер принял (или задание ему неизвестно — тогда хранить незачем).
async fn send_finish(client: &reqwest::Client, cfg: &AgentConfig, id: &str, body: &serde_json::Value) -> bool {
    match client.post(format!("{}/api/agent/jobs/{id}/finish", cfg.server)).bearer_auth(&cfg.token).json(body).timeout(Duration::from_secs(30)).send().await {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND || resp.status() == reqwest::StatusCode::FORBIDDEN => true,
        _ => false,
    }
}

async fn deliver_outbox(client: &reqwest::Client, cfg: &AgentConfig) {
    for id in outbox_ids() {
        let p = outbox_dir().join(format!("{id}.json"));
        let Ok(b) = std::fs::read(&p) else { continue };
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&b) else { let _ = std::fs::remove_file(&p); continue };
        if send_finish(client, cfg, &id, &body).await {
            tracing::info!("[{id}] итог задания доставлен серверу управления");
            remove_outbox(&id);
        } else {
            break; // сервер недоступен — попробую в следующем цикле
        }
    }
}

async fn flush(client: &reqwest::Client, buf: &Mutex<Vec<String>>, url: &str, token: &str) {
    let lines = std::mem::take(&mut *buf.lock());
    if lines.is_empty() { return; }
    if client.post(format!("{url}/log")).bearer_auth(token).json(&serde_json::json!({ "lines": lines })).send().await.is_err() {
        // Не потерять строки при обрыве: вернуть в начало буфера.
        let mut b = buf.lock();
        let mut l = lines;
        l.append(&mut b);
        *b = l;
    }
}

/// Непрерывная отправка сегментов WAL из спула на площадку.
async fn wal_shipper(shared: Arc<Shared>, tunnels: TunnelManager) {
    let mut repos: HashMap<String, (String, Arc<Repo>)> = HashMap::new();
    loop {
        let ships = shared.wal.read().clone();
        for ship in ships {
            let dir = crate::plugins::postgres::spool_dir(&ship.instance_id);
            let mut files: Vec<String> = match std::fs::read_dir(&dir) {
                Ok(rd) => rd.flatten().filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect(),
                Err(_) => continue,
            };
            files.sort();
            {
                let mut st = shared.wal_status.lock();
                let s = st.entry(ship.instance_id.clone()).or_insert_with(|| WalStatus { instance_id: ship.instance_id.clone(), ..Default::default() });
                s.queued = files.len() as u64;
            }
            if files.is_empty() { continue; }
            let sig = format!("{}|{}|{}", ship.site.remote.url, ship.site.remote.repo, crate::util::sha256_hex(&ship.site.password));
            let repo = match repos.get(&ship.instance_id).filter(|(s, _)| *s == sig) {
                Some((_, r)) => r.clone(),
                None => {
                    let r: Result<Arc<Repo>> = async {
                        let remote = tunnels.connect(&ship.site, &|m: &str| tracing::info!("WAL {}: {m}", ship.instance_id)).await?;
                        Ok(Arc::new(Repo::open(backend_for(&remote)?, &ship.site.password).await?.with_level(ship.site.compression.max(3))))
                    }
                    .await;
                    match r {
                        Ok(r) => { repos.insert(ship.instance_id.clone(), (sig, r.clone())); r }
                        Err(e) => {
                            shared.wal_status.lock().entry(ship.instance_id.clone()).and_modify(|s| s.last_error = Some(format!("{e:#}")));
                            continue;
                        }
                    }
                }
            };
            let ns = format!("wal/{}", ship.instance_id);
            for name in files {
                let path = dir.join(&name);
                let res: Result<()> = async {
                    let data = std::fs::read(&path)?;
                    repo.put_object(&ns, &name, data).await?;
                    std::fs::remove_file(&path)?;
                    Ok(())
                }
                .await;
                let mut st = shared.wal_status.lock();
                let s = st.get_mut(&ship.instance_id).unwrap();
                match res {
                    Ok(()) => {
                        s.shipped += 1;
                        s.queued = s.queued.saturating_sub(1);
                        s.last_shipped = Some(name.clone());
                        s.last_time = Some(chrono::Utc::now());
                        s.last_error = None;
                    }
                    Err(e) => {
                        tracing::warn!("WAL {} {name}: {e:#}", ship.instance_id);
                        s.last_error = Some(format!("{name}: {e:#}"));
                        drop(st);
                        repos.remove(&ship.instance_id);
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// `restore-exec`: задание из stdin, события построчно в stdout (вызывается сервером по SSH).
pub async fn restore_exec() -> Result<()> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    let spec: JobSpec = serde_json::from_str(&s).map_err(|e| anyhow!("задание: {e}"))?;
    let emit = |e: &ExecEvent| {
        println!("{}", serde_json::to_string(e).unwrap_or_default());
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };
    let log: LogFn = Arc::new(move |m: &str| emit(&ExecEvent::Log(m.to_string())));
    let tunnels = TunnelManager::default();
    match crate::exec::run_job(&spec, &tunnels, log).await {
        Ok(v) => emit(&ExecEvent::Result(v)),
        Err(e) => { emit(&ExecEvent::Error(format!("{e:#}"))); std::process::exit(1); }
    }
    Ok(())
}

// ---------------------------------------------------------------- обновление агента

fn update_dir() -> std::path::PathBuf { data_dir().join("updates") }

/// Отметка «новая версия на связи с сервером» — её ждёт установщик (иначе откат).
fn mark_alive(started: chrono::DateTime<chrono::Utc>) {
    let _ = crate::updater::ensure_dir(&update_dir());
    crate::updater::save(&update_dir().join("alive.json"), &serde_json::json!({ "version": crate::VERSION, "at": crate::updater::now(), "started": started, "pid": std::process::id() }));
}

/// Задание `self_update`: скачать свою сборку с сервера, сверить, проверить запуском и подготовить установку.
async fn self_update(client: &reqwest::Client, cfg: &AgentConfig, shared: &Shared, job: &JobSpec, log: &LogFn) -> Result<serde_json::Value> {
    use crate::updater as up;
    let want = job.params["version"].as_str().unwrap_or("").to_string();
    let sha = job.params["sha256"].as_str().unwrap_or("").to_string();
    let target = crate::dist::this_target();
    if job.params["target"].as_str().is_some_and(|t| t != target) { log(&format!("сборка этой машины — {target}")); }
    if shared.running.lock().len() > 1 { bail!("на агенте выполняются другие задания — обновите позже"); }
    up::installer_ready()?;
    if up::cmp_ver(&want, crate::VERSION) == std::cmp::Ordering::Equal { log(&format!("версия {want} уже установлена — переустановка")); }
    let d = update_dir();
    up::ensure_dir(&d)?;
    log(&format!("загрузка {target} {want} с сервера управления"));
    let r = client.get(format!("{}/api/agent/package/{target}", cfg.server)).bearer_auth(&cfg.token).timeout(Duration::from_secs(900)).send().await.context("загрузка программы")?;
    if !r.status().is_success() { bail!("сервер не выдал программу: {} {}", r.status(), r.text().await.unwrap_or_default().chars().take(300).collect::<String>()); }
    let data = r.bytes().await.context("загрузка программы")?;
    let pkg = d.join("package.bin");
    std::fs::write(&pkg, &data)?;
    drop(data);
    let got = up::sha256_file(&pkg)?;
    if !sha.is_empty() && got != sha { let _ = std::fs::remove_file(&pkg); bail!("контрольная сумма программы не совпадает — загрузка повреждена"); }
    up::check_elf(&pkg, std::env::consts::ARCH)?;
    let found = up::probe(&pkg).await?;
    if !want.is_empty() && found != want { bail!("сервер выдал версию {found}, ожидалась {want}"); }
    log(&format!("программа {found} проверена (SHA-256 {got}); установка — после отправки итога"));
    let status = d.join("status.json");
    up::save(&status, &up::Job { phase: "handoff".into(), progress: 75, running: true, target_version: found.clone(), from_version: crate::VERSION.into(), started_at: up::now(), updated_at: up::now(), ..Default::default() });
    *shared.handoff.lock() = Some(up::Request {
        package: pkg.clone(), sha256: got.clone(), target_version: found.clone(), from_version: crate::VERSION.into(), install: up::install_path(),
        service: up::service_name("hopebackup-agent"), health: up::Health::Marker { path: d.join("alive.json") }, status,
        dist_from: None, dist_dir: None, cleanup: vec![pkg], wait_secs: 120,
    });
    Ok(serde_json::json!({ "staged": found, "from": crate::VERSION, "sha256": got }))
}
