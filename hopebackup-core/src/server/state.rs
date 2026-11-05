//! Состояние сервера управления: экземпляры, площадки, коннекторы, агенты, цели, пользователи.
//! Хранится в `<data>/server/state.json` (0600: в нём пароли репозиториев и ключи SSH).

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub pw_hash: String,
    /// `admin`, `operator`, `restore`, `auditor`.
    pub role: String,
    /// Адрес почты — для ссылки сброса пароля.
    #[serde(default)]
    pub email: String,
    /// Номер поколения сеансов: смена или сброс пароля увеличивает его и закрывает открытые сеансы.
    #[serde(default)]
    pub session_gen: u64,
    pub created: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub version: String,
    pub token_hash: String,
    pub enrolled: DateTime<Utc>,
    #[serde(default)]
    pub last_seen: Option<DateTime<Utc>>,
    /// Сборка агента (`x86_64-gnu`…) — по ней сервер выдаёт программу при обновлении.
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub arch: String,
    /// Ход последнего обновления на агенте (status.json установщика).
    #[serde(default)]
    pub update: serde_json::Value,
    /// Возможности агента (`proto::AGENT_CAPS` его версии).
    #[serde(default)]
    pub caps: Vec<String>,
    /// Что агент нашёл на машине (рекомендации облегчённой описи).
    #[serde(default)]
    pub found: Vec<serde_json::Value>,
    /// Найденное, которое администратор попросил не предлагать (id рекомендаций).
    #[serde(default)]
    pub found_dismissed: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollToken {
    pub token_hash: String,
    pub expires: DateTime<Utc>,
    #[serde(default)]
    pub note: String,
    /// Токен выпущен для установки агента на машину из «Серверы и ВМ»: подключившийся агент
    /// привязывается к ней (сервер не доверяет идентификатору, который сообщила сама машина).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_id: String,
}

/// Площадка в объектном хранилище: Yandex Object Storage, AWS S3, S3-совместимое (MinIO, Ceph).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct S3Site {
    /// `yandex`, `aws` или `s3`.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    /// Ключ агентов: чтение и запись, без удаления.
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    /// Ключ сервера для очистки по политике хранения (с правом удаления).
    #[serde(default)]
    pub admin_access_key: String,
    #[serde(default)]
    pub admin_secret_key: String,
    #[serde(default)]
    pub path_style: bool,
    #[serde(default)]
    pub lock_mode: String,
    #[serde(default)]
    pub lock_days: u32,
    #[serde(default)]
    pub storage_class: String,
    #[serde(default)]
    pub fingerprint: String,
}

impl S3Site {
    /// Адрес по умолчанию для пресетов облаков.
    pub fn normalize(&mut self) {
        match self.provider.as_str() {
            "yandex" => {
                if self.endpoint.trim().is_empty() { self.endpoint = "https://storage.yandexcloud.net".into(); }
                if self.region.trim().is_empty() { self.region = "ru-central1".into(); }
            }
            "aws" => {
                if self.region.trim().is_empty() { self.region = "eu-central-1".into(); }
                if self.endpoint.trim().is_empty() { self.endpoint = format!("https://s3.{}.amazonaws.com", self.region.trim()); }
            }
            _ => {}
        }
        self.endpoint = self.endpoint.trim().trim_end_matches('/').to_string();
        self.prefix = self.prefix.trim().trim_matches('/').to_string();
        self.lock_mode = self.lock_mode.trim().to_uppercase();
    }

    pub fn access(&self, admin: bool) -> crate::store::s3::S3Access {
        crate::store::s3::S3Access {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            access_key: if admin { self.admin_access_key.clone() } else { self.access_key.clone() },
            secret_key: if admin { self.admin_secret_key.clone() } else { self.secret_key.clone() },
            path_style: self.path_style,
            lock_mode: self.lock_mode.clone(),
            lock_days: self.lock_days,
            storage_class: self.storage_class.clone(),
            fingerprint: self.fingerprint.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub name: String,
    /// `node` — узел HopeBackup; `s3` — объектное хранилище.
    #[serde(default = "d_site_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Site>,
    /// Каталог на диске сервера управления (встроенный узел, `url` = https://сервер:4443/node/<id>).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskSite>,
    /// Облачный диск: Google Диск, Яндекс Диск, Nextcloud/WebDAV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<crate::cloud::drive::CloudAccess>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default = "d_repo")]
    pub repo: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub prune_token: String,
    #[serde(default)]
    pub connector_ids: Vec<String>,
    #[serde(default = "d_level")]
    pub compression: i32,
    /// Принимающие площадки: всё, что попадает на эту площадку, копируется на них.
    #[serde(default)]
    pub replicas: Vec<String>,
    /// Как часто догонять принимающие площадки (сегменты WAL идут непрерывно), минут.
    #[serde(default = "d_repl_min")]
    pub replication_interval_min: u64,
    pub created: DateTime<Utc>,
}
fn d_repl_min() -> u64 { 10 }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskSite {
    /// Каталог на сервере управления (например /mnt/data/hopebackup).
    pub path: String,
}
fn d_repo() -> String { "main".into() }
fn d_site_kind() -> String { "node".into() }
fn d_level() -> i32 { 3 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub params: Value,
    pub created: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Полная копия (cron, 5 полей).
    #[serde(default)]
    pub full: Option<String>,
    /// Логический дамп (cron), если заданы базы.
    #[serde(default)]
    pub dump: Option<String>,
    /// Журнал или инкремент (cron): binlog MySQL, журнал транзакций MS SQL, oplog MongoDB и т. п.
    #[serde(default)]
    pub incr: Option<String>,
}
fn d_true() -> bool { true }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default = "d_keep")]
    pub keep_last: usize,
}
fn d_keep() -> usize { 7 }
impl Default for Retention {
    fn default() -> Self { Retention { keep_last: 7 } }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub catalog_id: String,
    pub plugin: String,
    pub agent_id: String,
    pub site_id: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default = "d_rpo")]
    pub rpo_target_min: u64,
    pub created: DateTime<Utc>,
    /// Своя политика повторов (иначе — общая из настроек сервера).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// Репликация копий этого экземпляра на другие площадки (закладка «Репликация»): зашифрованные блоки как
    /// есть, а у «Образов qcow2» — готовые файлы qcow2. По умолчанию — никуда.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replicate: Vec<ReplTarget>,
    /// Репликация задана администратором явно (сохранена закладка «Репликация»). С 0.9.5 у «Образов qcow2»
    /// площадки, включённые без этого (перенесённые из параметров 0.9.2), выключаются — до явного включения.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repl_set: bool,
    /// Передавать ли копии продукта при репликации площадки целиком («Принимающие площадки»). Не задано —
    /// по умолчанию: образы машин («machine») не передаются, остальные продукты — передаются.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_repl: Option<bool>,
}

impl Instance {
    /// Продукт исключён из репликации площадки целиком.
    pub fn site_repl_excluded(&self) -> bool {
        if self.plugin == "replica" { return false; }
        match self.site_repl { Some(v) => !v, None => self.plugin == "machine" }
    }
}
fn d_rpo() -> u64 { 15 }

/// Площадка, куда реплицируются копии экземпляра.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ReplTarget {
    pub site_id: String,
    #[serde(default)]
    pub enabled: bool,
    /// `after_job` — сразу после каждого успешного задания; `cron` — по расписанию `cron`.
    #[serde(default = "d_after_job")]
    pub when: String,
    #[serde(default)]
    pub cron: String,
    /// Сколько копий (у «Образов qcow2» — наборов) держать на площадке; 0 — как у экземпляра.
    #[serde(default)]
    pub keep: usize,
}
fn d_after_job() -> String { "after_job".into() }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    /// `ssh`, `agent` или создающие ВМ: `proxmox`, `vmware`, `yandex_cloud`, `aws`.
    pub kind: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "d_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub private_key: String,
    /// Пароль (вход по паролю, где ключи не используются; он же — для sudo).
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub agent_id: String,
    /// Параметры гипервизора или облака (секреты внутри — 0600 вместе со всем состоянием).
    #[serde(default)]
    pub params: Value,
    pub created: DateTime<Utc>,
}
fn d_ssh_port() -> u16 { 22 }

/// Узел управления (активный или принимающий).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub url: String,
    pub fingerprint: String,
    pub joined: DateTime<Utc>,
    /// Коннектор, через который активный узел достаёт до этого узла (VPN только в одну сторону). Пусто — напрямую.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connector_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u64,
    /// Ключ подписи сеансов панели.
    #[serde(default)]
    pub session_secret: String,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub enroll_tokens: Vec<EnrollToken>,
    #[serde(default)]
    pub sites: Vec<Site>,
    #[serde(default)]
    pub connectors: Vec<Connector>,
    #[serde(default)]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub targets: Vec<Target>,
    /// Узлы управления кластера (включая активный).
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// Общий секрет синхронизации узлов управления.
    #[serde(default)]
    pub cluster_secret: String,
    #[serde(default)]
    pub settings: Settings,
    /// Проекты переезда машин на другие платформы.
    #[serde(default)]
    pub migrations: Vec<super::migration::Migration>,
    /// Серверы и ВМ для описи по SSH.
    #[serde(default)]
    pub machines: Vec<super::machines::Machine>,
    /// Сервер исходящей почты.
    #[serde(default)]
    pub mail: super::mail::MailSettings,
    /// Уведомления администраторам.
    #[serde(default)]
    pub notify: super::notify::NotifySettings,
    /// Ссылки сброса пароля (хэши токенов).
    #[serde(default)]
    pub password_resets: Vec<super::recovery::PasswordReset>,
    /// Использованные аварийные токены вендора (jti).
    #[serde(default)]
    pub recovery_used: Vec<String>,
}

/// Общие настройки сервера.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Исключения для всех экземпляров файлового уровня (файлы, сервер целиком), если у экземпляра
    /// не выключено «Общие исключения сервера».
    #[serde(default = "crate::exclude::defaults")]
    pub default_excludes: Vec<String>,
    /// Повторы неудачных копий по расписанию (у экземпляра можно задать свои).
    #[serde(default)]
    pub retry: RetryPolicy,
    /// Догонять запуски по расписанию, пропущенные, пока сервер был выключен.
    #[serde(default = "d_true")]
    pub catch_up: bool,
    /// Догонять, если пропуск не старше стольких часов.
    #[serde(default = "d_catch_up_hours")]
    pub catch_up_hours: u32,
    /// Источник обновлений («Обновления»).
    #[serde(default)]
    pub updates: super::updates::UpdateSettings,
}

impl Default for Settings {
    fn default() -> Self { Settings { default_excludes: crate::exclude::defaults(), retry: RetryPolicy::default(), catch_up: true, catch_up_hours: d_catch_up_hours(), updates: Default::default() } }
}

fn d_catch_up_hours() -> u32 { 24 }

/// Политика повторов: сколько ещё попыток и через сколько минут каждая.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Повторных попыток после первой неудачной (0–24).
    #[serde(default = "d_attempts")]
    pub attempts: u32,
    /// Пауза перед каждой попыткой, минут (1–1440).
    #[serde(default = "d_interval")]
    pub interval_min: u32,
}
fn d_attempts() -> u32 { 3 }
fn d_interval() -> u32 { 60 }

impl Default for RetryPolicy {
    fn default() -> Self { RetryPolicy { enabled: true, attempts: d_attempts(), interval_min: d_interval() } }
}

impl RetryPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.attempts > 24 { return Err("повторных попыток — не больше 24".into()); }
        if !(1..=1440).contains(&self.interval_min) { return Err("пауза между попытками — от 1 до 1440 минут".into()); }
        Ok(())
    }
}

pub struct Store {
    path: PathBuf,
    st: RwLock<State>,
    /// Каталог изменился — активный узел передаст его принимающим.
    pub changed: std::sync::Arc<tokio::sync::Notify>,
}

impl Store {
    pub fn open(dir: &std::path::Path) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("state.json");
        let mut st: State = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| anyhow!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        // Переименования продуктов каталога.
        for i in st.instances.iter_mut() {
            if i.catalog_id == "qbase" { i.catalog_id = "qdatabase".into(); }
        }
        Ok(Store { path, st: RwLock::new(st), changed: std::sync::Arc::new(tokio::sync::Notify::new()) })
    }

    pub fn get(&self) -> State { self.st.read().clone() }

    pub fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T { f(&self.st.read()) }

    /// Изменить и сохранить.
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut g = self.st.write();
        let mut copy = g.clone();
        let r = f(&mut copy)?;
        copy.version += 1;
        crate::util::write_json(&self.path, &copy, true)?;
        *g = copy;
        self.changed.notify_one();
        Ok(r)
    }

    /// Заменить каталог целиком (принимающий узел получает его от активного).
    pub fn replace(&self, st: State) -> Result<()> {
        let mut g = self.st.write();
        crate::util::write_json(&self.path, &st, true)?;
        *g = st;
        Ok(())
    }
}
