//! Общие типы протокола сервер ↔ агент ↔ исполнитель восстановления.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::Remote;

/// Коннектор до площадки с секретами (передаётся агенту только на время работы).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorSpec {
    pub id: String,
    pub name: String,
    /// `direct` или `ssh_tunnel`.
    pub kind: String,
    #[serde(default)]
    pub params: Value,
}

/// Доступ к площадке: адрес узла, репозиторий, токен, пароль репозитория, коннекторы по приоритету.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SiteAccess {
    pub id: String,
    pub name: String,
    pub remote: Remote,
    pub password: String,
    #[serde(default)]
    pub connectors: Vec<ConnectorSpec>,
    #[serde(default = "d_level")]
    pub compression: i32,
}
fn d_level() -> i32 { 3 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceRuntime {
    pub id: String,
    pub name: String,
    pub plugin: String,
    #[serde(default)]
    pub config: Value,
}

/// Задание для исполнителя.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: String,
    /// `backup`, `restore`, `discover`, `action`, `init_repo`.
    pub kind: String,
    #[serde(default)]
    pub instance: Option<InstanceRuntime>,
    #[serde(default)]
    pub site: Option<SiteAccess>,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// Вид копии (`full`, `dump`, `files`) или имя действия.
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub params: Value,
}

/// Экземпляр, сегменты WAL которого агент отправляет на площадку непрерывно.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalShip {
    pub instance_id: String,
    pub site: SiteAccess,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WalStatus {
    pub instance_id: String,
    pub queued: u64,
    #[serde(default)]
    pub last_shipped: Option<String>,
    #[serde(default)]
    pub last_time: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub shipped: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentReport {
    pub version: String,
    pub host: String,
    pub os: String,
    #[serde(default)]
    pub wal: Vec<WalStatus>,
    #[serde(default)]
    pub running: Vec<String>,
    /// Задания, итог которых агент сохранил и ещё отправит (сервер был недоступен).
    #[serde(default)]
    pub pending: Vec<String>,
    /// Когда запущен процесс агента: задание, выданное раньше, этот процесс выполнять не может.
    #[serde(default)]
    pub started: Option<chrono::DateTime<chrono::Utc>>,
    /// Сборка (`x86_64-gnu`, `aarch64-musl`…).
    #[serde(default)]
    pub build: String,
    /// Ход последнего обновления (status.json установщика на агенте).
    #[serde(default)]
    pub update: Option<serde_json::Value>,
    /// Что умеет эта версия агента (`AGENT_CAPS`): сервер не отправляет задания, которых агент не знает.
    #[serde(default)]
    pub caps: Vec<String>,
    /// Что работает на машине и что стоит защищать (рекомендации облегчённой описи, только экземпляры).
    /// None — агент ещё не смотрел (или старая версия).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<Vec<serde_json::Value>>,
}

/// Возможности агента, появившиеся после 0.8.1. `pg-restart` — перезапуск PostgreSQL из панели.
pub const AGENT_CAPS: &[&str] = &["pg-restart", "install-pkgs"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PollResp {
    pub jobs: Vec<JobSpec>,
    pub wal: Vec<WalShip>,
    /// Действующие коннекторы: каналы остальных агент опускает (коннектор удалён или изменён).
    #[serde(default)]
    pub connectors: Option<Vec<String>>,
    /// Эпоха активного узла управления (агент не выполняет задания узла с меньшей эпохой).
    #[serde(default)]
    pub epoch: u64,
    /// Все узлы управления — куда переключаться, если этот недоступен.
    #[serde(default)]
    pub servers: Vec<ServerRef>,
    /// Основной порт сервера: агент, подключённый к другому (прежнему) порту, переходит на этот.
    #[serde(default)]
    pub server_port: u16,
}

/// Узел управления для агента.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRef {
    pub url: String,
    pub fingerprint: String,
}

/// Строка вывода `restore-exec` (по SSH): журнал, итог или ошибка — по одной JSON-строке.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecEvent {
    Log(String),
    Result(Value),
    Error(String),
}

pub fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}
