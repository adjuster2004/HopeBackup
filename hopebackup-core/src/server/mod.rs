//! Сервер управления: панель и REST API, агенты, задания, расписания, восстановление по SSH.

pub mod cloudauth;
pub mod disk;
pub mod agent_api;
pub mod api;
pub mod auth;
pub mod cluster;
pub mod imageship;
pub mod instrepl;
pub mod jobs;
pub mod machines;
pub mod migration;
pub mod updates;
pub mod mail;
pub mod notify;
pub mod recovery;
pub mod retry;
pub mod replication;
mod repl_bench;
pub mod ssh_restore;
pub mod state;
pub mod vm;

use anyhow::{anyhow, Context, Result};
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::connectors::TunnelManager;
use crate::proto::{ConnectorSpec, InstanceRuntime, JobSpec, SiteAccess, WalStatus};
use crate::store::Remote;
use jobs::Jobs;
use state::{Instance, Store};

pub struct Server {
    pub dir: PathBuf,
    pub store: Store,
    pub jobs: Jobs,
    pub tunnels: TunnelManager,
    pub started: DateTime<Utc>,
    pub fingerprint: String,
    pub listen_port: u16,
    /// Состояние отправки WAL по отчётам агентов.
    pub wal: Mutex<HashMap<String, WalStatus>>,
    /// Одноразовые ссылки на скачивание образа диска (в памяти; только admin).
    pub image_dl: Mutex<HashMap<String, api::ImageGrant>>,
    audit_lock: Mutex<()>,
    /// Роль узла в кластере управления (локально, не синхронизируется).
    pub cluster: parking_lot::RwLock<cluster::Local>,
    /// Итог последней синхронизации с каждым узлом.
    pub peer_status: Mutex<HashMap<String, Value>>,
    /// Состояние репликации пар площадок «исходная>принимающая».
    pub repl: Mutex<HashMap<String, Value>>,
    /// Место на диске узлов площадок (последнее известное): обновляется при проверке и после репликации.
    pub site_disk: Mutex<HashMap<String, Value>>,
    repl_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// По парам площадок: pack-файлы, которые не нужно передавать (их блоки уже на принимающей).
    repl_known: Mutex<HashMap<String, Arc<replication::Known>>>,
    /// Площадки, для которых нужна ещё одна репликация после текущей.
    pub repl_pending: Mutex<std::collections::HashSet<String>>,
    pub sync_now: Arc<tokio::sync::Notify>,
    /// Встроенные узлы площадок «диск сервера».
    pub disk: parking_lot::RwLock<HashMap<String, axum::Router>>,
    /// Незавершённые входы в облачные диски.
    pub oauth: Mutex<HashMap<String, cloudauth::Flow>>,
    /// Связь с агентами в памяти: время последнего опроса и «опрос идёт сейчас» (для уведомлений «не на связи»).
    pub agent_seen: Mutex<HashMap<String, (DateTime<Utc>, bool)>>,
}

pub fn server_dir() -> PathBuf { crate::agent::data_dir().join("server") }

impl Server {
    pub fn repl_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> { self.repl_locks.lock().entry(key.to_string()).or_default().clone() }
    pub fn repl_known(&self, key: &str) -> Arc<replication::Known> { self.repl_known.lock().entry(key.to_string()).or_default().clone() }

    pub fn audit(&self, user: &str, action: &str, detail: impl Into<String>) {
        let _g = self.audit_lock.lock();
        let line = json!({ "time": Utc::now(), "user": user, "action": action, "detail": detail.into() });
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("audit.log")) {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn read_audit(&self, limit: usize) -> Vec<Value> {
        let s = std::fs::read_to_string(self.dir.join("audit.log")).unwrap_or_default();
        s.lines().rev().take(limit).filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// Весь журнал аудита, новые первыми, с фильтром: текст (без учёта регистра — в пользователе, действии,
    /// подробностях) и период (RFC 3339, включительно).
    pub fn audit_filtered(&self, text: &str, from: &str, to: &str) -> Vec<Value> {
        let s = std::fs::read_to_string(self.dir.join("audit.log")).unwrap_or_default();
        let t = text.trim().to_lowercase();
        let parse = |x: &str| chrono::DateTime::parse_from_rfc3339(x.trim()).ok().map(|d| d.with_timezone(&Utc));
        let (from, to) = (parse(from), parse(to));
        s.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|e| {
            if !t.is_empty() {
                let hay = format!("{} {} {}", e["user"].as_str().unwrap_or(""), e["action"].as_str().unwrap_or(""), e["detail"].as_str().unwrap_or("")).to_lowercase();
                if !hay.contains(&t) { return false; }
            }
            if from.is_some() || to.is_some() {
                let Some(tm) = e["time"].as_str().and_then(parse) else { return false };
                if from.map(|f| tm < f).unwrap_or(false) || to.map(|x| tm > x).unwrap_or(false) { return false; }
            }
            true
        }).collect()
    }

    /// Доступ к площадке с секретами и коннекторами по порядку (права агента: чтение и запись).
    pub fn site_access(&self, site_id: &str) -> Result<SiteAccess> { self.site_access_as(site_id, false)?.ok_or_else(|| anyhow!("площадка не найдена")) }

    /// Доступ с правом очистки (токен администратора узла или ключ S3 с удалением); None — не задан.
    pub fn site_access_admin(&self, site_id: &str) -> Result<Option<SiteAccess>> { self.site_access_as(site_id, true) }

    fn site_access_as(&self, site_id: &str, admin: bool) -> Result<Option<SiteAccess>> {
        self.store.read(|st| {
            let s = st.sites.iter().find(|s| s.id == site_id).ok_or_else(|| anyhow!("площадка не найдена"))?;
            let connectors = s
                .connector_ids
                .iter()
                .filter_map(|cid| st.connectors.iter().find(|c| &c.id == cid))
                .map(|c| ConnectorSpec { id: c.id.clone(), name: c.name.clone(), kind: c.kind.clone(), params: c.params.clone() })
                .collect();
            let ov = self.cluster.read().site_overrides.get(&s.id).cloned();
            let connectors: Vec<ConnectorSpec> = if ov.as_ref().map(|o| o.direct).unwrap_or(false) { vec![] } else { connectors };
            let remote = if s.kind == "cloud" {
                let c = s.cloud.as_ref().ok_or_else(|| anyhow!("у облачной площадки нет параметров"))?;
                let mut r = Remote::node(&format!("cloud://{}", c.provider), &s.repo, "", "");
                r.cloud = Some(c.clone());
                r
            } else if s.kind == "s3" {
                let x = s.s3.as_ref().ok_or_else(|| anyhow!("у площадки S3 нет параметров бакета"))?;
                if admin && x.admin_access_key.is_empty() { return Ok(None); }
                let mut r = Remote::node(&format!("s3://{}/{}", x.bucket, x.prefix), &s.repo, "", "");
                r.s3 = Some(x.access(admin));
                r
            } else {
                if admin && s.prune_token.is_empty() { return Ok(None); }
                let url = ov.as_ref().map(|o| o.url.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| s.url.clone());
                Remote::node(&url, &s.repo, if admin { &s.prune_token } else { &s.token }, &s.fingerprint)
            };
            Ok(Some(SiteAccess { id: s.id.clone(), name: s.name.clone(), remote, password: s.password.clone(), connectors, compression: s.compression }))
        })
    }

    pub fn instance(&self, id: &str) -> Result<Instance> {
        self.store.read(|st| st.instances.iter().find(|i| i.id == id).cloned()).ok_or_else(|| anyhow!("экземпляр не найден"))
    }

    /// Экземпляр для исполнителя: конфигурация + общие исключения сервера.
    pub fn runtime(&self, i: &Instance) -> InstanceRuntime {
        let mut config = i.config.clone();
        if let Some(o) = config.as_object_mut() {
            let ex = self.store.read(|st| st.settings.default_excludes.clone());
            o.insert("global_exclude".into(), json!(ex));
            // Продукт «Образ машины по SSH» берёт доступ из раздела «Серверы и ВМ» (секреты — не маской).
            if i.plugin == "machine" {
                if let Some(mid) = o.get("machine_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from) {
                    if let Some(m) = self.store.read(|st| st.machines.iter().find(|m| m.id == mid).cloned()) {
                        o.insert("host".into(), json!(m.host));
                        o.insert("port".into(), json!(m.port));
                        o.insert("user".into(), json!(m.user));
                        o.insert("auth".into(), json!(m.auth));
                        o.insert("password".into(), json!(m.password));
                        o.insert("private_key".into(), json!(m.private_key));
                        o.insert("key_passphrase".into(), json!(m.key_passphrase));
                        o.insert("sudo_password".into(), json!(m.sudo_password));
                        o.insert("host_key".into(), json!(m.host_key));
                    }
                }
            }
        }
        InstanceRuntime { id: i.id.clone(), name: i.name.clone(), plugin: i.plugin.clone(), config }
    }

    /// Поставить копию экземпляра в очередь его агента.
    pub fn enqueue_backup(&self, inst: &Instance, kind: &str, user: &str) -> Result<String> {
        // «Образы qcow2» берут копию там, где сейчас копии источника (даже если его перевели на другую площадку).
        let site_id = if api::FOLLOWS_SOURCE.contains(&inst.plugin.as_str()) {
            self.store.read(|st| api::source_site_of(st, &inst.config)).unwrap_or_else(|_| inst.site_id.clone())
        } else { inst.site_id.clone() };
        let site = self.site_access(&site_id)?;
        let spec = JobSpec { id: String::new(), kind: "backup".into(), instance: Some(self.runtime(inst)), site: Some(site), snapshot_id: None, mode: kind.to_string(), params: Value::Null };
        let title = format!("{} — {}", inst.name, match kind { "full" => "полная копия", "dump" => "логический дамп", "files" => "копия файлов", "incr" => "журнал / инкремент", k => k });
        Ok(self.jobs.enqueue(spec, title, &inst.agent_id, user))
    }

    /// Выполнить задание на самом сервере (создание репозитория, восстановление по SSH или на новую ВМ).
    pub fn spawn_local(self: &Arc<Self>, id: String, ssh: Option<(state::Target, String)>) {
        let s = self.clone();
        tokio::spawn(async move {
            let Some(spec) = s.jobs.start_local(&id) else { return };
            let s2 = s.clone();
            let jid = id.clone();
            let log: crate::plugins::LogFn = Arc::new(move |m: &str| s2.jobs.log(&jid, vec![m.to_string()]));
            let r = match ssh {
                Some((target, via)) if vm::is_vm_kind(&target.kind) => vm::restore(&s, spec, &target, &via, log.clone()).await,
                Some((target, via)) => ssh_restore::run(&s, spec, &target, &via, log.clone()).await,
                None => crate::exec::run_job(&spec, &s.tunnels, log.clone()).await,
            };
            match r {
                Ok(v) => {
                    if let Some(j) = s.jobs.finish(&id, true, v, None) {
                        // Копию выполнил сам сервер (продукты без агента): как после отчёта агента — очистка
                        // по хранению и репликация на принимающие площадки.
                        if j.kind == "backup" && j.status == "ok" { if let Some(inst) = &j.instance_id { s.after_backup(inst); } }
                        // Образы (qcow2 и образ машины), которые никуда не реплицируются, — напомнить в журнале задания,
                        // где репликация включается.
                        if j.kind == "backup" && j.status == "ok" {
                            if let Some(inst) = j.instance_id.as_ref().and_then(|x| s.instance(x).ok()) {
                                if let Some(m) = image_repl_note(&s, &inst) { s.jobs.log(&j.id, vec![m]); }
                            }
                        }
                        migration::on_job_finished(&s, &j);
                    }
                }
                Err(e) => { log(&format!("ошибка: {e:#}")); if let Some(j) = s.jobs.finish(&id, false, Value::Null, Some(format!("{e:#}"))) { migration::on_job_finished(&s, &j); } }
            }
        });
    }

    /// После успешной копии — очистка площадки по политикам хранения (если задан токен очистки)
    /// и репликация на принимающие площадки.
    pub fn after_backup(self: &Arc<Self>, instance_id: &str) {
        let Ok(inst) = self.instance(instance_id) else { return };
        self.enqueue_replication(&inst.site_id, "после копии");
        // Закладка «Репликация»: на площадки с «сразу после выполнения задания» — зашифрованные копии
        // экземпляра или (у «Образов qcow2») готовые qcow2.
        if inst.plugin != "replica" { instrepl::after_job(self, &inst); }
        // Реплика переезда — цепочка изменений от первой синхронизации: старые звенья не удаляются.
        if inst.plugin == "replica" { return; }
        let (per, keep) = self.store.read(|st| {
            // Политики всех экземпляров каталога: на площадке бывают копии и других (после перевода).
            let per: HashMap<String, usize> = st.instances.iter().map(|i| (i.id.clone(), i.retention.keep_last.max(1))).collect();
            (per, inst.retention.keep_last.max(1))
        });
        let Ok(Some(access)) = self.site_access_admin(&inst.site_id) else { return };
        let s = self.clone();
        let site_id = inst.site_id.clone();
        tokio::spawn(async move {
            let r: Result<crate::store::types::PruneReport> = async {
                let remote = s.tunnels.connect(&access, &|_| {}).await?;
                let be = crate::store::backend_for(&remote)?;
                // Агентский токен (ключ) не может удалять — очистку выполняет сервер с правами администратора.
                be.prune(&crate::store::types::PrunePolicy { keep_last: keep.max(per.values().copied().max().unwrap_or(1)), keep_days: None, dry_run: false, per_instance: per, keep_unknown: true }).await
            }
            .await;
            match r {
                Ok(rep) if !rep.snapshots_removed.is_empty() || rep.wal_removed > 0 => {
                    s.audit("system", "prune", format!("площадка {site_id}: снапшотов {}, pack-файлов {}, освобождено {}, WAL {}", rep.snapshots_removed.len(), rep.packs_removed, crate::util::human_bytes(rep.bytes_freed), rep.wal_removed));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("очистка площадки {site_id}: {e:#}"),
            }
        });
    }
}

/// Запуск заданий, назначенных самому серверу управления (агент «server»): безагентные продукты вроде
/// «Образ машины по SSH» сервер выполняет сам, подключаясь к машине по SSH.
async fn local_jobs_loop(s: Arc<Server>) {
    loop {
        if s.is_active() { for id in s.jobs.pending_local() { s.spawn_local(id, None); } }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Следующий запуск по cron (5 полей, местное время сервера).
pub fn next_run(expr: &str) -> Option<DateTime<Utc>> {
    let c = croner::Cron::new(expr).parse().ok()?;
    c.find_next_occurrence(&chrono::Local::now(), false).ok().map(|t| t.with_timezone(&Utc))
}

async fn scheduler(s: Arc<Server>) {
    let mut fired: HashMap<String, String> = HashMap::new();
    loop {
        if !s.is_active() { tokio::time::sleep(std::time::Duration::from_secs(20)).await; continue; }
        let now = chrono::Local::now();
        let minute = now.format("%Y%m%d%H%M").to_string();
        let insts = s.store.read(|st| st.instances.clone());
        for i in insts {
            if !i.schedule.enabled { continue; }
            let main_kind = if i.plugin == "files" { "files" } else { "full" };
            for (expr, kind) in [(i.schedule.full.clone(), main_kind), (i.schedule.dump.clone(), "dump"), (i.schedule.incr.clone(), "incr")] {
                let Some(expr) = expr.filter(|e| !e.trim().is_empty()) else { continue };
                let Ok(c) = croner::Cron::new(&expr).parse() else { continue };
                if c.is_time_matching(&now).unwrap_or(false) {
                    let key = format!("{}|{kind}", i.id);
                    if fired.get(&key) == Some(&minute) { continue; }
                    fired.insert(key, minute.clone());
                    match s.enqueue_backup(&i, kind, "расписание") {
                        Ok(id) => {
                            s.jobs.update(&id, |j| j.origin = "schedule".into());
                            tracing::info!("по расписанию: {} ({kind}) → задание {id}", i.name)
                        }
                        Err(e) => tracing::warn!("расписание {}: {e:#}", i.name),
                    }
                }
            }
        }
        // Репликация экземпляров по своему расписанию (закладка «Репликация» → «по расписанию»).
        instrepl::on_minute(&s, &now, &mut fired, &minute);
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    }
}

#[derive(rust_embed::RustEmbed)]
#[folder = "../admin/dist"]
struct Ui;

async fn static_file(uri: axum::http::Uri) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (file, name) = match Ui::get(path) {
        Some(f) => (f, path.to_string()),
        None if !path.contains('.') || path.ends_with(".html") => match Ui::get("index.html") {
            Some(f) => (f, "index.html".into()),
            None => return (axum::http::StatusCode::NOT_FOUND, "панель не собрана").into_response(),
        },
        None => return (axum::http::StatusCode::NOT_FOUND, "не найдено").into_response(),
    };
    let mime = mime_guess::from_path(&name).first_or_octet_stream();
    let cache = if name == "index.html" { "no-cache" } else { "public, max-age=31536000, immutable" };
    ([(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string()), (axum::http::header::CACHE_CONTROL, cache.to_string())], file.data.into_owned()).into_response()
}

pub fn router(s: Arc<Server>) -> Router {
    Router::new()
        .route("/api/health", get(|| async { ([("x-hopebackup-version", crate::VERSION)], "ok") }))
        .route("/api/auth/login", post(api::login))
        .route("/api/auth/recover-request", post(recovery::request))
        .route("/api/auth/recover-check", get(recovery::check))
        .route("/api/auth/recover-reset", post(recovery::reset))
        .route("/api/auth/recovery-request", get(recovery::emergency_request))
        .route("/api/auth/recover-emergency", post(recovery::emergency))
        .route("/api/mail", get(mail::get))
        .route("/api/mail/smtp", put(mail::put_smtp))
        .route("/api/mail/notify", put(mail::put_notify))
        .route("/api/mail/test", post(mail::test))
        .route("/api/mail/test-notify", post(mail::test_notify))
        .route("/api/auth/me", get(api::me))
        .route("/api/status", get(api::status))
        .route("/api/catalog", get(api::catalog))
        .route("/api/instances", get(api::instances).post(api::instance_create))
        .route("/api/instances/:id", get(api::instance_get).put(api::instance_update).delete(api::instance_delete))
        .route("/api/instances/:id/backup", post(api::instance_backup))
        .route("/api/instances/:id/ship", post(api::instance_ship))
        .route("/api/instances/:id/replication", get(api::replication_get).put(api::replication_put))
        .route("/api/instances/:id/replicate", post(api::replicate_now))
        .route("/api/instances/:id/discover", post(api::instance_discover))
        .route("/api/instances/:id/action", post(api::instance_action))
        .route("/api/instances/:id/points", get(api::instance_points))
        .route("/api/sites", get(api::sites).post(api::site_create))
        .route("/api/sites/probe", post(api::site_probe))
        .route("/api/sites/oauth/start", post(api::oauth_start))
        .route("/api/sites/oauth/:flow", get(api::oauth_status))
        .route("/api/sites/:id", put(api::site_update).delete(api::site_delete))
        .route("/api/sites/:id/test", post(api::site_test))
        .route("/api/sites/:id/init", post(api::site_init))
        .route("/api/connectors", get(api::connectors).post(api::connector_create))
        .route("/api/connectors/wg-keygen", post(api::wg_keygen))
        .route("/api/connectors/requirements", get(api::connector_requirements))
        .route("/api/connectors/install", post(api::connector_install))
        .route("/api/connectors/:id", put(api::connector_update).delete(api::connector_delete))
        .route("/api/connectors/:id/test", post(api::connector_test))
        .route("/api/agents", get(api::agents))
        .route("/api/agents/enroll-token", post(api::agent_enroll_token))
        .route("/api/agents/:id", delete(api::agent_delete))
        .route("/api/targets", get(api::targets).post(api::target_create))
        .route("/api/targets/:id", put(api::target_update).delete(api::target_delete))
        .route("/api/targets/:id/test", post(api::target_test))
        .route("/api/restore", post(api::restore))
        .route("/api/image/disks", get(api::image_disks))
        .route("/api/image/download", post(api::image_download))
        .route("/api/image/estimate", get(api::image_estimate))
        .route("/api/server/mounts", get(api::server_mounts))
        .route("/api/image/requirements", get(api::image_requirements))
        .route("/api/image/dl/:token", get(api::image_dl))
        .route("/api/updates", get(updates::overview))
        .route("/api/updates/check", post(updates::do_check))
        .route("/api/updates/config", put(updates::config))
        .route("/api/updates/upload", post(updates::upload).layer(axum::extract::DefaultBodyLimit::max(crate::updater::MAX_PACKAGE as usize + 1024)))
        .route("/api/updates/upload/start", post(updates::upload_start))
        .route("/api/updates/upload/:id", axum::routing::put(updates::upload_chunk))
        .route("/api/updates/upload/:id/finish", post(updates::upload_finish))
        .route("/api/updates/apply", post(updates::do_apply))
        .route("/api/updates/discard", post(updates::discard))
        .route("/api/updates/dismiss", post(updates::dismiss))
        .route("/api/updates/agents", post(updates::update_agents))
        .route("/api/agent/package/:target", get(updates::agent_package))
        .route("/api/migrations", get(migration::list).post(migration::create))
        .route("/api/migrations/:id", get(migration::get_one).put(migration::update).delete(migration::delete))
        .route("/api/migrations/:id/:op", post(migration::step))
        .route("/api/jobs", get(api::jobs))
        .route("/api/jobs/ack", post(api::jobs_ack))
        .route("/api/suggestions", get(api::suggestions))
        .route("/api/suggestions/apply", post(api::suggestion_apply))
        .route("/api/suggestions/dismiss", post(api::suggestion_dismiss))
        .route("/api/jobs/:id", get(api::job_get))
        .route("/api/audit", get(api::audit))
        .route("/api/audit/page", get(api::audit_page))
        .route("/api/audit/export", get(api::audit_export))
        .route("/api/settings", get(api::settings).put(api::settings_update))
        .route("/api/machines", get(machines::list).post(machines::create))
        .route("/api/machines/:id", put(machines::update).delete(machines::delete))
        .route("/api/machines/:id/test", post(machines::test))
        .route("/api/machines/:id/reset-key", post(machines::reset_key))
        .route("/api/machines/:id/inventory", get(machines::inventory_get).post(machines::inventory_start))
        .route("/api/machines/:id/install-agent", post(machines::install_agent))
        .route("/api/machines/:id/apply", post(machines::apply))
        .route("/api/users", get(api::users).post(api::user_create))
        .route("/api/users/:name", put(api::user_update).delete(api::user_delete))
        .route("/api/users/:name/password", put(api::user_password))
        .route("/api/cluster", get(cluster::status))
        .route("/api/cluster/invite", post(cluster::invite))
        .route("/api/cluster/join", post(cluster::join))
        .route("/api/cluster/takeover", post(cluster::takeover))
        .route("/api/cluster/overrides/:id", put(cluster::override_site))
        .route("/api/cluster/peers/:id", delete(cluster::peer_delete))
        .route("/api/cluster/peer/join", post(cluster::peer_join))
        .route("/api/cluster/peer/sync", post(cluster::peer_sync))
        .route("/api/cluster/peer/status", get(cluster::peer_status))
        .route("/api/cluster/offer", post(cluster::offer))
        .route("/api/cluster/adopt", post(cluster::adopt))
        .route("/api/cluster/peers/:id/update", post(cluster::peer_update))
        .route("/api/cluster/peer/adopt", post(cluster::peer_adopt))
        .route("/api/cluster/peer/update", post(cluster::peer_update_recv).layer(axum::extract::DefaultBodyLimit::max(crate::updater::MAX_PACKAGE as usize + 1024)))
        .route("/api/sites/:id/replicate", post(api::site_replicate))
        .route("/api/sites/:id/failover", post(api::site_failover))
        .route("/api/agent/enroll", post(agent_api::enroll))
        .route("/api/agent/poll", post(agent_api::poll))
        .route("/api/agent/jobs/:id/log", post(agent_api::job_log))
        .route("/api/agent/jobs/:id/finish", post(agent_api::job_finish))
        .nest_service("/node", disk::service(s.clone()))
        .fallback(static_file)
        .layer(axum::middleware::from_fn_with_state(s.clone(), auth::middleware))
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(s)
}

/// Запустить сервер управления.
pub async fn run(listen: &str) -> Result<()> {
    let dir = server_dir();
    std::fs::create_dir_all(&dir)?;
    let store = Store::open(&dir)?;
    // Первый запуск: ключ сеансов и администратор с паролем в файле (как в HopeDFS).
    store.update(|st| {
        if st.session_secret.is_empty() { st.session_secret = crate::util::token(); }
        if st.users.is_empty() {
            let pw = crate::util::token()[..16].to_string();
            st.users.push(state::User { name: "Administrator".into(), pw_hash: auth::hash_password(&pw), role: "admin".into(), email: String::new(), session_gen: 0, created: Utc::now() });
            let p = dir.join("initial_admin_password.txt");
            crate::util::write_secret(&p, format!("{pw}\n").as_bytes())?;
            tracing::info!("создан пользователь Administrator, пароль — в {}", p.display());
        }
        Ok(())
    })?;
    let (cert, key) = crate::tlsutil::ensure_cert(&dir, &[])?;
    let fingerprint = crate::tlsutil::fingerprint_pem(&cert)?;
    // Несколько адресов через запятую: первый — основной (его порт сообщается агентам).
    let addrs: Vec<std::net::SocketAddr> = listen.split(',').map(|a| a.trim()).filter(|a| !a.is_empty()).map(|a| a.parse().with_context(|| format!("адрес {a}"))).collect::<Result<_>>()?;
    let addr = *addrs.first().ok_or_else(|| anyhow!("не задан адрес сервера"))?;
    let local = cluster::Local::load(&dir);
    let sync_now = store.changed.clone();
    let s = Arc::new(Server {
        jobs: Jobs::open(dir.join("jobs")),
        cluster: parking_lot::RwLock::new(local),
        peer_status: Mutex::new(HashMap::new()),
        repl: Mutex::new(HashMap::new()),
        site_disk: Mutex::new(HashMap::new()),
        repl_locks: Mutex::new(HashMap::new()),
        repl_known: Mutex::new(HashMap::new()),
        repl_pending: Mutex::new(Default::default()),
        sync_now,
        disk: parking_lot::RwLock::new(HashMap::new()),
        oauth: Mutex::new(HashMap::new()),
        agent_seen: Mutex::new(HashMap::new()),
        dir,
        store,
        tunnels: TunnelManager::default(),
        started: Utc::now(),
        fingerprint: fingerprint.clone(),
        listen_port: addr.port(),
        wal: Mutex::new(HashMap::new()),
        image_dl: Mutex::new(HashMap::new()),
        audit_lock: Mutex::new(()),
    });
    s.save_cluster();
    tokio::spawn(scheduler(s.clone()));
    // С 0.9.1 «Образы qcow2» собирает и конвертирует только сервер управления: экземпляры, назначенные агенту
    // в 0.9.0, переводятся на сервер.
    let _ = s.store.update(|st| {
        for i in st.instances.iter_mut().filter(|i| api::SERVER_ONLY.contains(&i.plugin.as_str()) && i.agent_id != "server") {
            tracing::info!("«{}»: сборку образов выполняет сервер управления (было: агент {})", i.name, i.agent_id);
            i.agent_id = "server".into();
        }
        // С 0.9.3 площадки для готовых qcow2 — на закладке «Репликация» (было в параметрах: copy_sites, keep_remote).
        for i in st.instances.iter_mut().filter(|i| i.plugin == "qcow2images") {
            let sites = crate::connectors::net::p_list(&i.config, "copy_sites");
            if !sites.is_empty() && i.replicate.is_empty() {
                let keep = i.config.get("keep_remote").and_then(|x| x.as_u64()).unwrap_or(2) as usize;
                // Перенесённые площадки — выключенными: отправка готовых qcow2 только после явного включения.
                i.replicate = sites.into_iter().map(|site_id| state::ReplTarget { site_id, enabled: false, when: "after_job".into(), cron: String::new(), keep }).collect();
                tracing::info!("«{}»: площадки для qcow2 перенесены на закладку «Репликация» (выключены)", i.name);
            }
            if let Some(o) = i.config.as_object_mut() { o.remove("copy_sites"); o.remove("keep_remote"); }
            // С 0.9.5 образы не реплицируются автоматически: включённые не администратором площадки — выключить.
            if !i.repl_set && i.replicate.iter().any(|t| t.enabled) {
                for t in i.replicate.iter_mut() { t.enabled = false; }
                tracing::warn!("«{}»: репликация образов qcow2 на другие площадки остановлена — включается на закладке «Репликация»", i.name);
            }
        }
        // С 0.9.2 площадка «Образов qcow2» — всегда площадка источника.
        let fix: Vec<(String, String)> = st.instances.iter().filter(|i| api::FOLLOWS_SOURCE.contains(&i.plugin.as_str()))
            .filter_map(|i| api::source_site_of(st, &i.config).ok().filter(|s| *s != i.site_id).map(|s| (i.id.clone(), s))).collect();
        for (id, site) in fix {
            if let Some(i) = st.instances.iter_mut().find(|i| i.id == id) {
                tracing::info!("«{}»: площадка как у источника — {site} (было {})", i.name, i.site_id);
                i.site_id = site;
            }
        }
        Ok(())
    });
    tokio::spawn(local_jobs_loop(s.clone()));
    tokio::spawn(cluster::sync_loop(s.clone()));
    s.tunnels.spawn_watchdog();
    tokio::spawn(replication::replication_loop(s.clone()));
    tokio::spawn(cloudauth::token_loop(s.clone()));
    notify::start(s.clone());
    tokio::spawn(retry::retry_loop(s.clone()));
    updates::start(s.clone());
    { let c = s.cluster.read(); tracing::info!("узел управления {} ({}): роль {}, эпоха {}", c.name, c.node_id, if c.role == "active" { "активный" } else { "принимающий" }, c.epoch); }
    tracing::info!("HopeBackup {}: панель https://{addr}/ , отпечаток сертификата {fingerprint}", crate::VERSION);
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await?;
    let app = router(s);
    for extra in addrs.iter().skip(1) {
        tracing::info!("дополнительный адрес https://{extra}/ (прежний порт: агенты переходят на {})", addr.port());
        let (tls, app, extra) = (tls.clone(), app.clone(), *extra);
        tokio::spawn(async move { if let Err(e) = axum_server::bind_rustls(extra, tls).serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await { tracing::error!("адрес {extra}: {e:#}"); } });
    }
    axum_server::bind_rustls(addr, tls).serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}

/// Напоминание для журнала задания: копии образа никуда не реплицируются (репликация экземпляра выключена, из
/// репликации площадки продукт исключён) — и где её включить. `None` — реплицируются или это не образ.
pub(crate) fn image_repl_note(s: &Server, inst: &state::Instance) -> Option<String> {
    if !instrepl::targets(inst).is_empty() { return None; }
    match inst.plugin.as_str() {
        "qcow2images" => Some(if inst.replicate.is_empty() {
            "Готовые qcow2 остались на площадке источника; на другие площадки они не отправляются — включается на закладке «Репликация» продукта".into()
        } else {
            "Репликация образов на другие площадки остановлена — готовые qcow2 не отправляются; включается на закладке «Репликация» продукта".into()
        }),
        "machine" if inst.site_repl_excluded() => {
            let replicas = s.store.read(|st| st.sites.iter().find(|x| x.id == inst.site_id).map(|x| x.replicas.len()).unwrap_or(0));
            (replicas > 0).then(|| "Репликация образа на другие площадки остановлена: при репликации площадки этот продукт не передаётся — включается на закладке «Репликация» продукта".into())
        }
        _ => None,
    }
}
