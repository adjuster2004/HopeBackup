//! Плагин PostgreSQL 14+ и совместимых форков (qdatabase).
//!
//! Версия не зашита: каталог бинарников и версия определяются на хосте, все утилиты берутся из того же
//! каталога, что и сервер. Полная копия — `pg_basebackup` tar-потоком прямо в движок хранения (без
//! временных файлов), непрерывность — архив WAL: `archive_command` кладёт сегмент в локальный спул,
//! агент отправляет его на площадку. Восстановление — распаковка + restore_command + цель по времени.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::tarsniff::TarSniffer;
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Postgres;

/// Минимальная поддерживаемая мажорная версия.
pub const MIN_MAJOR: u32 = 14;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "bin_dir": { "type": "string", "title": "Каталог бинарников (bin_dir)", "description": "Пусто — найти автоматически: /usr/lib/postgresql/*/bin, /usr/pgsql-*/bin, /opt/qdatabase-*/bin, запущенный процесс postgres" },
            "host": { "type": "string", "title": "Хост или каталог сокета", "default": "/var/run/postgresql" },
            "port": { "type": "integer", "title": "Порт", "default": 5432 },
            "user": { "type": "string", "title": "Пользователь СУБД", "default": "postgres", "description": "Роль с REPLICATION (для pg_basebackup) и правом чтения баз (для pg_dump)" },
            "password": { "type": "string", "format": "secret", "title": "Пароль", "description": "Пусто — вход по peer/trust от имени пользователя ОС" },
            "os_user": { "type": "string", "title": "Пользователь ОС сервера СУБД", "default": "postgres" },
            "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы для логического дампа", "description": "Дополнительно к физической копии — для восстановления одной базы" },
            "backup_method": { "type": "string", "title": "Способ базовой копии", "enum": ["auto", "basebackup", "lowlevel"], "default": "auto",
                "description": "auto — pg_basebackup, а если репликационное подключение недоступно (max_wal_senders = 0 или нет правила replication в pg_hba.conf, как у встроенной PostgreSQL GitLab) — низкоуровневая копия pg_backup_start/stop с чтением каталога данных" }
        }
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PgConf {
    #[serde(default)]
    pub bin_dir: Option<String>,
    #[serde(default = "d_host")]
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_user")]
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "d_user")]
    pub os_user: String,
    #[serde(default)]
    pub databases: Vec<String>,
    /// auto | basebackup | lowlevel.
    #[serde(default)]
    pub backup_method: Option<String>,
    /// Что агент подставил сам (сокет, порт, роль) — пишется в журнал предупреждением.
    #[serde(skip)]
    pub note: Option<String>,
}
fn d_host() -> String { "/var/run/postgresql".into() }
fn d_port() -> u16 { 5432 }
fn d_user() -> String { "postgres".into() }

impl PgConf {
    pub fn from(v: &Value) -> Result<PgConf> {
        let mut c: PgConf = serde_json::from_value(v.clone()).context("параметры PostgreSQL")?;
        c.bin_dir = c.bin_dir.filter(|s| !s.trim().is_empty());
        c.password = c.password.filter(|s| !s.is_empty());
        if c.host.trim().is_empty() { c.host = d_host(); }
        c.autodetect(&running_clusters());
        Ok(c)
    }

    /// Указанного сокета нет, а PostgreSQL на сервере работает (GitLab Omnibus: /var/opt/gitlab/postgresql,
    /// пользователь gitlab-psql; сборки с другим каталогом сокета) — подключиться к работающему кластеру.
    /// Роль и пользователь ОС меняются, только если остались по умолчанию: владелец кластера — его суперпользователь.
    pub fn autodetect(&mut self, all: &[RunningPg]) {
        if !self.host.starts_with('/') || Path::new(&self.host).join(format!(".s.PGSQL.{}", self.port)).exists() { return; }
        // bin_dir указан — только кластер этих программ (иначе, пока нужный остановлен, попали бы в чужой).
        let pick = match self.bin_dir.as_deref() {
            Some(b) => all.iter().find(|c| c.bin_dir == b.trim_end_matches('/')),
            None => if all.len() == 1 { all.first() } else { None },
        };
        let Some(c) = pick else { return };
        if c.socket_dir.is_empty() || c.port == 0 { return; }
        let was = format!("{}:{}", self.host, self.port);
        self.host = c.socket_dir.clone();
        self.port = c.port;
        let mut who = String::new();
        let default_os = self.os_user.is_empty() || self.os_user == "postgres" || self.os_user == "root";
        if self.user == "postgres" && default_os && !c.owner.is_empty() && c.owner != "postgres" && crate::util::user_ids(&c.owner).is_some() {
            self.user = c.owner.clone();
            self.os_user = c.owner.clone();
            who = format!(", роль и пользователь ОС — {}", c.owner);
        }
        let kind = if c.bin_dir.starts_with("/opt/gitlab/") { " (встроенная PostgreSQL GitLab)" } else { "" };
        self.note = Some(format!("сокета {was} нет — подключаюсь к работающему PostgreSQL{kind}: сокет {}, порт {}{who}. Чтобы зафиксировать, укажите это в параметрах экземпляра («Хост», «Порт», «Пользователь СУБД», «Пользователь ОС сервера СУБД»)", self.host, self.port));
    }
}

/// Работающий кластер PostgreSQL на этой машине (по процессу postmaster и его postmaster.pid).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunningPg {
    pub bin_dir: String,
    pub data_dir: String,
    pub port: u16,
    pub socket_dir: String,
    pub owner: String,
}

/// postmaster.pid: 1 — pid, 2 — каталог данных, 4 — порт, 5 — каталог сокета (первый, если их несколько).
pub fn parse_postmaster_pid(t: &str) -> (u16, String) {
    let l: Vec<&str> = t.lines().collect();
    let port = l.get(3).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let sock = l.get(4).map(|x| x.split(',').next().unwrap_or("").trim().to_string()).unwrap_or_default();
    (port, sock)
}

pub fn running_clusters() -> Vec<RunningPg> {
    let mut out: Vec<RunningPg> = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    let name_of = |pid: &str| std::fs::read_link(format!("/proc/{pid}/exe")).ok().and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string())).unwrap_or_default();
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) { continue; }
        let Ok(exe) = std::fs::read_link(e.path().join("exe")) else { continue };
        let n = exe.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if n != "postgres" && n != "postmaster" { continue; }
        // Только главный процесс кластера: родитель — не postgres.
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let ppid = stat.rsplit(')').next().and_then(|x| x.split_whitespace().nth(1)).unwrap_or("0").to_string();
        if name_of(&ppid) == "postgres" || name_of(&ppid) == "postmaster" { continue; }
        let args: Vec<String> = std::fs::read(e.path().join("cmdline")).unwrap_or_default().split(|b| *b == 0).map(|x| String::from_utf8_lossy(x).to_string()).collect();
        let cwd = std::fs::read_link(e.path().join("cwd")).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let data_dir = args.iter().position(|a| a == "-D").and_then(|i| args.get(i + 1).cloned())
            .map(|d| if d.starts_with('/') { d } else { format!("{cwd}/{d}") }).unwrap_or(cwd);
        let (port, socket_dir) = std::fs::read_to_string(Path::new(&data_dir).join("postmaster.pid")).map(|t| parse_postmaster_pid(&t)).unwrap_or((0, String::new()));
        let uid = std::fs::read_to_string(e.path().join("status")).unwrap_or_default().lines()
            .find_map(|l| l.strip_prefix("Uid:").and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse::<u32>().ok()));
        let owner = uid.and_then(crate::util::user_name).unwrap_or_default();
        let bin_dir = exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let c = RunningPg { bin_dir, data_dir, port, socket_dir, owner };
        if !out.contains(&c) { out.push(c); }
    }
    out
}

/// Что найдено на хосте.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PgInfo {
    pub bin_dir: String,
    pub version_string: String,
    pub version: String,
    pub major: u32,
    /// `postgresql` или `qdatabase`.
    pub flavor: String,
    #[serde(default)]
    pub server_version_num: Option<u32>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub archive_mode: Option<String>,
    #[serde(default)]
    pub archive_command: Option<String>,
    #[serde(default)]
    pub wal_level: Option<String>,
    #[serde(default)]
    pub wal_segment_size: Option<u64>,
    #[serde(default)]
    pub system_identifier: Option<String>,
    #[serde(default)]
    pub config_file: Option<String>,
    #[serde(default)]
    pub hba_file: Option<String>,
    #[serde(default)]
    pub ident_file: Option<String>,
    #[serde(default)]
    pub archive_ok: bool,
    #[serde(default)]
    pub recommended_archive_command: String,
    #[serde(default)]
    pub connect_error: Option<String>,
    #[serde(default)]
    pub candidates: Vec<String>,
    /// Есть параметры, которые применятся только после перезапуска (например, archive_mode).
    #[serde(default)]
    pub pending_restart: bool,
    /// Команда перезапуска этого кластера (для подсказки в панели).
    #[serde(default)]
    pub restart_command: String,
    /// Агент подключился не по параметрам, а к найденному работающему кластеру (что подставлено).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autodetected: Option<String>,
}

/// Каталог программ можно запускать от root: он и файл postgres принадлежат root и не доступны
/// на запись группе и остальным (как и все каталоги выше). Иначе локальный пользователь мог бы
/// подложить «postgres» и получить root, когда агент его запустит.
pub fn trusted_bin_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let ok = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.uid() == 0 && m.mode() & 0o022 == 0).unwrap_or(false);
    if !dir.is_absolute() || dir.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return false; }
    if !ok(&dir.join("postgres")) { return false; }
    let mut d = Some(dir);
    while let Some(x) = d { if !ok(x) { return false; } d = x.parent(); }
    true
}

/// Каталоги бинарников: из запущенных процессов, затем стандартные места установки
/// (только принадлежащие root — см. `trusted_bin_dir`).
pub fn find_bin_dirs() -> Vec<PathBuf> {
    let mut v = find_bin_dirs_all();
    v.retain(|d| trusted_bin_dir(d));
    v
}

fn find_bin_dirs_all() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) { continue; }
            if let Ok(exe) = std::fs::read_link(e.path().join("exe")) {
                let n = exe.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if n == "postgres" || n == "postmaster" {
                    if let Some(p) = exe.parent() { out.push(p.to_path_buf()); }
                }
            }
        }
    }
    let mut globbed = Vec::new();
    let mut scan = |dir: &str, pred: &dyn Fn(&str) -> bool, sub: &str| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if pred(&n) {
                    let p = e.path().join(sub);
                    if p.join("postgres").exists() { globbed.push(p); }
                }
            }
        }
    };
    scan("/usr/lib/postgresql", &|_| true, "bin");
    scan("/usr", &|n| n.starts_with("pgsql-") || n == "pgsql", "bin");
    scan("/opt", &|n| n.starts_with("qdatabase") || n.starts_with("pgpro") || n.starts_with("postgres"), "bin");
    scan("/usr/local", &|n| n == "pgsql", "bin");
    // Новые версии раньше старых.
    globbed.sort_by(|a, b| b.cmp(a));
    out.extend(globbed);
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn parse_version(s: &str) -> Option<(String, u32)> {
    // «postgres (PostgreSQL) 16.4 (Ubuntu …)» → «16.4», 16. Берём первое число вида N или N.M.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
            let v = s[start..i].trim_end_matches('.').to_string();
            let major: u32 = v.split('.').next()?.parse().ok()?;
            if major >= 9 { return Some((v, major)); }
        }
        i += 1;
    }
    None
}

/// qdatabase (прежнее имя в копиях — qbase).
fn is_qdb(flavor: &str) -> bool { flavor == "qdatabase" || flavor == "qbase" }

fn detect_flavor(bin_dir: &str, version_string: &str) -> &'static str {
    let l = version_string.to_lowercase();
    if bin_dir.contains("qdatabase") || l.contains("qbase") || l.contains("qdatabase") { "qdatabase" } else { "postgresql" }
}

/// Команда от имени пользователя ОС сервера СУБД (если агент запущен от root).
fn pg_cmd(bin: &Path, tool: &str, conf: &PgConf) -> std::process::Command {
    let mut c = std::process::Command::new(bin.join(tool));
    c.env("PGHOST", &conf.host).env("PGPORT", conf.port.to_string()).env("PGUSER", &conf.user).env("PSQLRC", "/dev/null").env("PGCONNECT_TIMEOUT", "15").env("LC_ALL", "C");
    if let Some(p) = &conf.password { c.env("PGPASSWORD", p); }
    as_os_user(&mut c, &conf.os_user);
    c
}

fn as_os_user(c: &mut std::process::Command, os_user: &str) {
    c.current_dir("/");
    if crate::util::is_root() {
        if let Some((uid, gid)) = crate::util::user_ids(os_user) {
            // Как su: дополнительные группы пользователя (например, ssl-cert для ключа SSL) сохраняются.
            // Command::uid() их сбрасывает, поэтому группы и uid меняем сами перед exec.
            let name = std::ffi::CString::new(os_user).unwrap_or_default();
            unsafe {
                c.pre_exec(move || {
                    if libc::initgroups(name.as_ptr(), gid as _) != 0 { return Err(std::io::Error::last_os_error()); }
                    if libc::setgid(gid) != 0 { return Err(std::io::Error::last_os_error()); }
                    if libc::setuid(uid) != 0 { return Err(std::io::Error::last_os_error()); }
                    Ok(())
                });
            }
            c.env("HOME", "/tmp").env("USER", os_user);
        }
    }
}

async fn output(mut c: std::process::Command) -> Result<String> {
    let prog = format!("{:?}", c.get_program());
    let out = tokio::task::spawn_blocking(move || c.output()).await?.with_context(|| format!("запуск {prog}"))?;
    if !out.status.success() {
        bail!("{prog}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn psql(bin: &Path, conf: &PgConf, db: &str, sql: &str) -> Result<String> {
    let mut c = pg_cmd(bin, "psql", conf);
    c.args(["-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-d", db, "-c", sql]);
    match output(c).await {
        Ok(s) => Ok(s.trim().to_string()),
        Err(e) => {
            let msg = format!("{e:#}");
            match explain(&msg, conf, &effective_os_user(&conf.os_user), &running_clusters()) { Some(h) => bail!("{msg}\n\n{h}"), None => bail!(msg) }
        }
    }
}

/// От чьего имени на самом деле запускаются программы СУБД (см. `as_os_user`).
fn effective_os_user(os_user: &str) -> String {
    if crate::util::is_root() {
        if crate::util::user_ids(os_user).is_some() { os_user.to_string() } else { "root".into() }
    } else {
        crate::util::user_name(unsafe { libc::geteuid() }).unwrap_or_else(|| "?".into())
    }
}

/// Подсказка администратору: что значит ошибка psql и что поправить в параметрах экземпляра.
/// Суперпользователь кластера, к которому подключаемся: владелец работающего кластера с этим сокетом или
/// программами (у GitLab Omnibus — gitlab-psql), иначе postgres. Второе — пояснение для GitLab.
fn superuser_for(conf: &PgConf, clusters: &[RunningPg]) -> (String, String) {
    let host = conf.host.trim_end_matches('/');
    let c = clusters.iter().find(|c| !c.socket_dir.is_empty() && c.socket_dir.trim_end_matches('/') == host)
        .or_else(|| conf.bin_dir.as_deref().and_then(|b| clusters.iter().find(|c| c.bin_dir == b.trim_end_matches('/'))));
    let gitlab = c.map(|c| c.bin_dir.starts_with("/opt/gitlab/")).unwrap_or(false) || conf.host.starts_with("/var/opt/gitlab");
    let su = c.map(|c| c.owner.clone()).filter(|o| !o.is_empty() && o != "root")
        .unwrap_or_else(|| if gitlab { "gitlab-psql".into() } else { "postgres".into() });
    let note = if gitlab { format!(" Это встроенная PostgreSQL GitLab: её суперпользователь — {su}, а роль gitlab — роль приложения без прав на копию и настройку WAL.") } else { String::new() };
    (su, note)
}

pub fn explain(err: &str, conf: &PgConf, os: &str, clusters: &[RunningPg]) -> Option<String> {
    let user = &conf.user;
    let (su, gl) = superuser_for(conf, clusters);
    // Каталог сокета для совета: текущий, если он настоящий; у GitLab — его собственный.
    let sock = if conf.host.starts_with('/') { conf.host.clone() } else if !gl.is_empty() { "/var/opt/gitlab/postgresql".into() } else { "/var/run/postgresql".into() };
    let fix_where = "Где поправить: страница экземпляра → вкладка «Параметры» → блок «Параметры плагина», затем «Сохранить» и повторите задание.";
    let missing = if !conf.os_user.is_empty() && crate::util::is_root() && crate::util::user_ids(&conf.os_user).is_none() {
        format!(" Пользователя ОС «{}» на сервере нет, поэтому программы запускались от root.", conf.os_user)
    } else { String::new() };
    let socket = conf.host.starts_with('/');
    if err.contains("Peer authentication failed") || err.contains("Ident authentication failed") {
        let kind = if err.contains("Peer") { "peer" } else { "ident" };
        return Some(format!(
            "Что это значит: вход через {} проверяется методом {kind} — PostgreSQL пускает роль «{user}», только если программа запущена от пользователя ОС с тем же именем. \
             Сейчас пользователь ОС — «{os}», роль СУБД — «{user}».{missing}{gl}\n\
             Как исправить (рекомендуется): «Пользователь СУБД» = {su} и «Пользователь ОС сервера СУБД» = {su}, пароль не нужен — у роли {su} есть права и на копию, и на настройку архива WAL.\n\
             Или вход по паролю: «Хост» = 127.0.0.1 и пароль роли «{user}»; роли нужны права REPLICATION и суперпользователя (для настройки архива WAL).\n{fix_where}",
            if socket { "сокет" } else { "сеть" }));
    }
    if err.contains("password authentication failed") {
        return Some(format!("Что это значит: неверный пароль роли «{user}». Проверьте поле «Пароль» или войдите без пароля через сокет: «Хост» = {sock}, «Пользователь СУБД» и «Пользователь ОС сервера СУБД» = {su}.{gl}\n{fix_where}"));
    }
    if err.contains("no password supplied") {
        return Some(format!("Что это значит: сервер требует пароль для роли «{user}», а он не задан. Укажите «Пароль» или войдите без пароля через сокет от {su} («Пользователь СУБД» и «Пользователь ОС сервера СУБД» = {su}).{gl}\n{fix_where}"));
    }
    if err.contains("does not exist") && err.contains("role") {
        return Some(format!("Что это значит: роли «{user}» в СУБД нет. Подходит суперпользователь кластера {su} («Пользователь СУБД» и «Пользователь ОС сервера СУБД» = {su}).{gl}\n{fix_where}"));
    }
    if err.contains("no pg_hba.conf entry") {
        return Some(format!("Что это значит: в pg_hba.conf нет правила, разрешающего этот вход (роль «{user}», хост «{}»). Проще всего входить через сокет от {su}: «Хост» = {sock}, «Пользователь СУБД» и «Пользователь ОС сервера СУБД» = {su}. Либо добавьте правило в pg_hba.conf и выполните select pg_reload_conf().\n{fix_where}", conf.host));
    }
    if err.contains("Connection refused") || (err.contains("No such file or directory") && err.contains(".s.PGSQL")) {
        let found: Vec<String> = clusters.iter().filter(|c| !c.socket_dir.is_empty())
            .map(|c| format!("сокет {}, порт {}, пользователь ОС {} ({})", c.socket_dir, c.port, if c.owner.is_empty() { "?" } else { &c.owner }, c.bin_dir)).collect();
        if !found.is_empty() {
            return Some(format!("Что это значит: по адресу «{}», порт {} PostgreSQL не отвечает. На сервере работает: {}. Укажите «Хост» = каталог сокета, «Порт», а «Пользователь СУБД» и «Пользователь ОС сервера СУБД» = пользователь ОС этого кластера.\n{fix_where}", conf.host, conf.port, found.join("; ")));
        }
        return Some(format!("Что это значит: по адресу «{}», порт {} PostgreSQL не отвечает — сервер не запущен или слушает другой порт или каталог сокета. Проверьте на сервере: systemctl status 'postgresql*' и ss -lxp | grep PGSQL; затем поправьте «Хост» и «Порт».\n{fix_where}", conf.host, conf.port));
    }
    if err.contains("max_wal_senders") || (err.contains("no pg_hba.conf entry") && err.contains("replication")) {
        return Some(format!("Что это значит: репликационное подключение для pg_basebackup закрыто настройками кластера.{gl} Агент 0.8.9 и новее в этом случае сам делает низкоуровневую копию (pg_backup_start/stop) — обновите агент в «Обновлениях». Либо разрешите репликацию: max_wal_senders ≥ 2 и правило «local replication {su} peer» в pg_hba.conf.\n{fix_where}"));
    }
    if err.contains("must be superuser") || err.contains("permission denied to set parameter") {
        return Some(format!("Что это значит: у роли «{user}» нет прав суперпользователя, а они нужны для настройки архива WAL (ALTER SYSTEM). Укажите «Пользователь СУБД» и «Пользователь ОС сервера СУБД» = {su} (вход через сокет без пароля) или выдайте права: ALTER ROLE {user} SUPERUSER.{gl}\n{fix_where}"));
    }
    None
}

pub fn archive_command_for(instance: &str) -> String {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "/usr/local/bin/hopebackup".into());
    format!("{} wal-push --spool {} %p", exe, spool_dir(instance).display())
}

pub fn spool_dir(instance: &str) -> PathBuf { crate::agent::data_dir().join("spool").join(instance) }

pub async fn discover_info(conf: &PgConf, instance: &str) -> Result<PgInfo> {
    let candidates = find_bin_dirs();
    let bin = match &conf.bin_dir {
        Some(b) => PathBuf::from(b),
        None => candidates.first().cloned().ok_or_else(|| anyhow!("PostgreSQL не найден: укажите каталог бинарников (bin_dir)"))?,
    };
    if !bin.join("postgres").exists() { bail!("в {} нет исполняемого файла postgres", bin.display()); }
    if !trusted_bin_dir(&bin) { bail!("каталог {} (или файл postgres) не принадлежит root либо доступен на запись другим — такие программы агент не запускает", bin.display()); }
    let mut vc = std::process::Command::new(bin.join("postgres"));
    vc.arg("--version");
    let vs = output(vc).await?.trim().to_string();
    let (version, major) = parse_version(&vs).ok_or_else(|| anyhow!("не удалось разобрать версию: {vs}"))?;
    let bin_s = bin.to_string_lossy().to_string();
    let mut info = PgInfo {
        flavor: detect_flavor(&bin_s, &vs).to_string(),
        bin_dir: bin_s,
        version_string: vs,
        version,
        major,
        recommended_archive_command: archive_command_for(instance),
        candidates: candidates.iter().map(|p| p.to_string_lossy().to_string()).collect(),
        ..Default::default()
    };
    if major < MIN_MAJOR { bail!("поддерживается PostgreSQL {MIN_MAJOR} и новее, найдено {}", info.version); }
    let sql = "select current_setting('server_version_num'), current_setting('data_directory'), current_setting('archive_mode'), current_setting('archive_command'), current_setting('wal_level'), current_setting('wal_segment_size'), (select system_identifier from pg_control_system()), current_setting('config_file'), current_setting('hba_file'), current_setting('ident_file')";
    match psql(&bin, conf, "postgres", &sql.replace(", ", " || '|' || ")).await {
        Ok(row) => {
            let f: Vec<&str> = row.split('|').collect();
            if f.len() >= 7 {
                info.server_version_num = f[0].parse().ok();
                info.data_dir = Some(f[1].to_string());
                info.archive_mode = Some(f[2].to_string());
                info.archive_command = Some(f[3].to_string());
                info.wal_level = Some(f[4].to_string());
                info.wal_segment_size = parse_size(f[5]);
                info.system_identifier = Some(f[6].to_string());
                if f.len() >= 10 {
                    info.config_file = Some(f[7].to_string());
                    info.hba_file = Some(f[8].to_string());
                    info.ident_file = Some(f[9].to_string());
                }
                info.archive_ok = matches!(f[2], "on" | "always") && f[3].contains("wal-push") && f[3].contains(&format!("/spool/{instance}"));
                // Сервер может быть новее найденных утилит (или наоборот): утилиты должны совпадать с сервером.
                if let Some(svn) = info.server_version_num {
                    if svn / 10000 != major { info.connect_error = Some(format!("сервер {} не совпадает с утилитами {}: укажите bin_dir сервера", svn / 10000, major)); }
                }
            }
        }
        Err(e) => info.connect_error = Some(format!("{e:#}")),
    }
    info.autodetected = conf.note.clone();
    if info.connect_error.is_none() {
        info.pending_restart = psql(&bin, conf, "postgres", "select exists(select 1 from pg_settings where pending_restart)").await.map(|v| v == "t").unwrap_or(false);
        info.restart_command = restart_command(&info);
    }
    Ok(info)
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, mul) = if let Some(x) = s.strip_suffix("GB") { (x, 1u64 << 30) } else if let Some(x) = s.strip_suffix("MB") { (x, 1 << 20) } else if let Some(x) = s.strip_suffix("kB") { (x, 1 << 10) } else { (s, 1) };
    n.trim().parse::<u64>().ok().map(|v| v * mul)
}

/// Имя сегмента WAL по LSN вида `0/2000028`.
pub fn wal_segment(tli: u32, lsn: &str, seg_size: u64) -> Option<String> {
    let (hi, lo) = lsn.split_once('/')?;
    let v = (u64::from_str_radix(hi, 16).ok()? << 32) | u64::from_str_radix(lo, 16).ok()?;
    let segno = v / seg_size;
    let per = 0x1_0000_0000u64 / seg_size;
    Some(format!("{:08X}{:08X}{:08X}", tli, segno / per, segno % per))
}

fn parse_backup_label(s: &str) -> (Option<String>, Option<u32>) {
    let mut seg = None;
    let mut tli = None;
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("START WAL LOCATION:") {
            if let Some(i) = r.find("(file ") { seg = Some(r[i + 6..].trim_end_matches(')').trim().to_string()); }
        }
        if let Some(r) = l.strip_prefix("START TIMELINE:") { tli = r.trim().parse().ok(); }
    }
    (seg, tli)
}

/// Последний сегмент WAL, без которого копия неконсистентна (из backup_manifest).
fn wal_end_from_manifest(m: &[u8], seg_size: u64) -> Option<String> {
    let v: Value = serde_json::from_slice(m).ok()?;
    let r = v.get("WAL-Ranges")?.as_array()?.last()?;
    let tli = r.get("Timeline")?.as_u64()? as u32;
    wal_segment(tli, r.get("End-LSN")?.as_str()?, seg_size)
}

/// Дочитать stderr процесса в журнал задания (в отдельном потоке).
fn pipe_stderr(child: &mut std::process::Child, log: LogFn, prefix: &'static str) -> std::thread::JoinHandle<Vec<String>> {
    let err = child.stderr.take();
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        if let Some(e) = err {
            for l in std::io::BufReader::new(e).lines().map_while(Result::ok) {
                if !l.trim().is_empty() {
                    log(&format!("{prefix}: {l}"));
                    tail.push(l);
                    if tail.len() > 20 { tail.remove(0); }
                }
            }
        }
        tail
    })
}

fn progress_logger(log: LogFn, what: String) -> impl Fn(&crate::store::repo::WriteStats) + Send + Sync {
    let last = parking_lot::Mutex::new(Instant::now());
    move |s| {
        let mut l = last.lock();
        if l.elapsed() >= Duration::from_secs(5) {
            *l = Instant::now();
            log(&format!("{what}: прочитано {}, новых {}, отправлено {}", crate::util::human_bytes(s.bytes_read), crate::util::human_bytes(s.new_bytes), crate::util::human_bytes(s.uploaded)));
        }
    }
}

#[async_trait]
impl Plugin for Postgres {
    fn id(&self) -> &'static str { "postgresql" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let conf = PgConf::from(cfg)?;
        if let Some(n) = &conf.note { _log(&format!("внимание: {n}")); }
        let instance = cfg_str(cfg, "_instance").unwrap_or_else(|| "instance".into());
        Ok(serde_json::to_value(discover_info(&conf, &instance).await?)?)
    }

    async fn action(&self, name: &str, cfg: &Value, instance: &str, log: &LogFn) -> Result<Value> {
        match name {
            "setup-wal" => setup_wal(&PgConf::from(cfg)?, instance, log).await,
            "restart" => restart_pg(&PgConf::from(cfg)?, instance, log).await,
            _ => bail!("агент версии {} не знает действие «{name}» — обновите агент («Обновления» → таблица агентов)", crate::VERSION),
        }
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let conf = PgConf::from(&ctx.config)?;
        if let Some(n) = &conf.note { (ctx.log)(&format!("внимание: {n}")); }
        let info = discover_info(&conf, &ctx.instance).await?;
        if let Some(e) = &info.connect_error { bail!("нет подключения к серверу: {e}"); }
        let log = ctx.log.clone();
        log(&format!("{} {} · {} · PGDATA {}", if is_qdb(&info.flavor) { "qdatabase" } else { "PostgreSQL" }, info.version, info.bin_dir, info.data_dir.clone().unwrap_or_default()));
        let bin = PathBuf::from(&info.bin_dir);
        let seg_size = info.wal_segment_size.unwrap_or(16 << 20);
        let mut w = ctx.repo.writer();
        let mut extra = json!({ "major": info.major, "version": info.version, "flavor": info.flavor, "system_identifier": info.system_identifier });
        let mut meta = json!({ "pg": info });
        let full = ctx.kind != "dump";
        if full {
            if !info.archive_ok {
                bail!("архивация WAL для этого экземпляра не настроена (archive_mode = {}, archive_command = «{}»). Настройте его: страница экземпляра → вкладка «Обзор» → блок «Архив WAL» → кнопка «Настроить архив WAL», затем перезапустите PostgreSQL и повторите копию. Без журнала копию нельзя восстановить", info.archive_mode.clone().unwrap_or_default(), info.archive_command.clone().unwrap_or_default());
            }
            let label_text;
            let wal_end;
            if let Some(why) = lowlevel_reason(&bin, &conf).await {
                log(&format!("низкоуровневая копия (pg_backup_start/stop): {why}"));
                let data_dir = PathBuf::from(info.data_dir.clone().ok_or_else(|| anyhow!("неизвестен каталог данных"))?);
                let (rp, wp) = std::io::pipe()?;
                let (b2, c2, l2, lbl) = (bin.clone(), conf.clone(), log.clone(), format!("hopebackup {}", ctx.instance));
                let major = info.major;
                let prod = std::thread::spawn(move || lowlevel_produce(b2, c2, major, data_dir, lbl, wp, l2));
                let prog = progress_logger(log.clone(), "базовая копия".into());
                let res = w.add_stream("base.tar", rp, Some(&prog)).await;
                let (label, stop_lsn) = prod.join().map_err(|_| anyhow!("поток копии аварийно завершился"))??;
                res?;
                let tli = parse_backup_label(&label).1.unwrap_or(1);
                wal_end = wal_segment(tli, &stop_lsn, seg_size);
                label_text = label;
            } else {
            log("pg_basebackup: поток tar в хранилище (без временных файлов)");
            let mut c = pg_cmd(&bin, "pg_basebackup", &conf);
            c.args(["-D", "-", "-F", "t", "-X", "none", "-c", "fast", "--no-password", "-l"]).arg(format!("hopebackup {}", ctx.instance));
            c.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
            let mut child = c.spawn().context("запуск pg_basebackup")?;
            let errs = pipe_stderr(&mut child, log.clone(), "pg_basebackup");
            let stdout = child.stdout.take().unwrap();
            let (sniff, cap) = TarSniffer::new(stdout, &["backup_label", "backup_manifest"]);
            let prog = progress_logger(log.clone(), "базовая копия".into());
            let r = w.add_stream("base.tar", sniff, Some(&prog)).await;
            let st = tokio::task::spawn_blocking(move || child.wait()).await??;
            let tail = errs.join().unwrap_or_default();
            r?;
            if !st.success() { bail!("pg_basebackup завершился с ошибкой: {}", tail.join(" | ")); }
            let cap = cap.lock().clone();
            label_text = cap.get("backup_label").map(|b| String::from_utf8_lossy(b).to_string()).ok_or_else(|| anyhow!("в потоке нет backup_label — копия неполная"))?;
            wal_end = cap.get("backup_manifest").and_then(|m| wal_end_from_manifest(m, seg_size));
            }
            let label = label_text;
            let (wal_start, _tli) = parse_backup_label(&label);
            log(&format!("журнал WAL для этой копии: {} … {}", wal_start.clone().unwrap_or("?".into()), wal_end.clone().unwrap_or("?".into())));
            extra["wal_start"] = json!(wal_start);
            extra["wal_end"] = json!(wal_end);
            meta["backup_label"] = json!(label);
            // Debian/Ubuntu: postgresql.conf и pg_hba.conf лежат в /etc/postgresql/…, а не в каталоге данных,
            // и pg_basebackup их не забирает — сохраняем отдельно.
            if let Some(files) = external_configs(&info) {
                log(&format!("внешние файлы конфигурации: {}", files.iter().map(|(_, p)| p.display().to_string()).collect::<Vec<_>>().join(", ")));
                let (r, wpipe) = std::io::pipe()?;
                let list = files.clone();
                let prod = std::thread::spawn(move || -> Result<()> {
                    let mut b = tar::Builder::new(wpipe);
                    for (name, path) in list {
                        if path.is_dir() { b.append_dir_all(&name, &path)?; } else { b.append_path_with_name(&path, &name)?; }
                    }
                    b.finish()?;
                    Ok(())
                });
                let res = w.add_stream("config.tar", r, None).await;
                prod.join().map_err(|_| anyhow!("поток конфигурации аварийно завершился"))??;
                res?;
            }
        }
        let mut dumped = Vec::new();
        for db in &conf.databases {
            log(&format!("pg_dump: база {db}"));
            let mut c = pg_cmd(&bin, "pg_dump", &conf);
            c.args(["-F", "c", "-Z", "0", "--no-password", "-d", db]);
            c.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
            let mut child = c.spawn().context("запуск pg_dump")?;
            let errs = pipe_stderr(&mut child, log.clone(), "pg_dump");
            let stdout = child.stdout.take().unwrap();
            let prog = progress_logger(log.clone(), format!("дамп {db}"));
            let r = w.add_stream(&format!("dump/{db}.pgdump"), stdout, Some(&prog)).await;
            let st = tokio::task::spawn_blocking(move || child.wait()).await??;
            let tail = errs.join().unwrap_or_default();
            r?;
            if !st.success() { bail!("pg_dump {db}: {}", tail.join(" | ")); }
            dumped.push(db.clone());
        }
        if !full && dumped.is_empty() { bail!("для логической копии укажите базы (databases)"); }
        meta["databases"] = json!(dumped);
        let (header, stats) = w.finish(&ctx.instance, "postgresql", if full { "full" } else { "dump" }, extra, meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> { restore(ctx).await }
}

/// Почему pg_basebackup не подойдёт (None — подойдёт): репликационное подключение закрыто настройками кластера.
async fn lowlevel_reason(bin: &Path, conf: &PgConf) -> Option<String> {
    match conf.backup_method.as_deref().unwrap_or("auto") {
        "lowlevel" => return Some("так задано в параметрах экземпляра".into()),
        "basebackup" => return None,
        _ => {}
    }
    let row = psql(bin, conf, "postgres", "select current_setting('max_wal_senders') || '|' || (select count(*) from pg_hba_file_rules where 'replication' = any(database) and error is null)").await.ok()?;
    let (senders, rules) = row.split_once('|')?;
    if senders.trim() == "0" { return Some("max_wal_senders = 0 — репликационные подключения выключены".into()); }
    if rules.trim() == "0" { return Some("в pg_hba.conf нет правила для подключения replication".into()); }
    None
}

/// Служебное содержимое каталога данных, которое в копию не входит (как у pg_basebackup): каталоги
/// остаются пустыми, файлы пропускаются.
const LL_EMPTY_DIRS: &[&str] = &["pg_wal", "pg_replslot", "pg_dynshmem", "pg_notify", "pg_serial", "pg_snapshots", "pg_stat_tmp", "pg_subtrans"];
const LL_SKIP_FILES: &[&str] = &["postmaster.pid", "postmaster.opts", "backup_label", "backup_label.old", "tablespace_map", "pg_internal.init"];

/// Низкоуровневая базовая копия: в одном сеансе psql pg_backup_start → tar каталога данных потоком → pg_backup_stop
/// (с ожиданием архивации WAL). backup_label из pg_backup_stop кладётся в конец tar — восстановление такое же,
/// как у копии pg_basebackup. Сеанс оборвался — PostgreSQL сам отменяет копию.
/// Возвращает (backup_label, LSN окончания).
fn lowlevel_produce(bin: PathBuf, conf: PgConf, major: u32, data_dir: PathBuf, label: String, out: std::io::PipeWriter, log: LogFn) -> Result<(String, String)> {
    use std::io::{BufRead, Read, Write};
    let mut c = pg_cmd(&bin, "psql", &conf);
    c.args(["-X", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1", "-d", "postgres"]);
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().context("запуск psql")?;
    let errs = pipe_stderr(&mut child, log.clone(), "psql");
    let mut stdin = child.stdin.take().unwrap();
    let mut rd = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut ask = |sql: &str| -> Result<Vec<String>> {
        writeln!(stdin, "{sql};\n\\echo __HB_DONE__")?;
        stdin.flush()?;
        let mut lines = Vec::new();
        loop {
            let mut l = String::new();
            if rd.read_line(&mut l)? == 0 { bail!("psql завершился посреди копии"); }
            let l = l.trim_end().to_string();
            if l == "__HB_DONE__" { return Ok(lines); }
            if !l.is_empty() { lines.push(l); }
        }
    };
    let lbl = label.replace('\'', "''");
    let start_sql = if major >= 15 { format!("select pg_backup_start('{lbl}', true)") } else { format!("select pg_start_backup('{lbl}', true, false)") };
    let r = (|| -> Result<(String, String)> {
        let start = ask(&start_sql)?.first().cloned().unwrap_or_default();
        log(&format!("pg_backup_start: {start} — чтение каталога данных"));
        if std::fs::read_dir(data_dir.join("pg_tblspc")).map(|mut d| d.next().is_some()).unwrap_or(false) {
            bail!("в кластере есть табличные пространства (pg_tblspc) — низкоуровневая копия их не поддерживает; включите репликационное подключение для pg_basebackup");
        }
        let mut b = tar::Builder::new(out);
        b.follow_symlinks(false);
        let md0 = std::fs::metadata(&data_dir)?;
        let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
        let mut files = 0u64;
        while let Some(rel) = stack.pop() {
            let dir = data_dir.join(&rel);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let rp = rel.join(&name);
                let top = rel.as_os_str().is_empty();
                if LL_SKIP_FILES.contains(&name.as_str()) || name.starts_with("pgsql_tmp") { continue; }
                let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
                let mut h = tar::Header::new_gnu();
                h.set_metadata_in_mode(&md, tar::HeaderMode::Complete);
                if md.is_dir() || (top && LL_EMPTY_DIRS.contains(&name.as_str())) {
                    // Служебные каталоги (и pg_wal, даже если это ссылка) — пустыми.
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    if md.file_type().is_symlink() { h.set_mode(0o700); }
                    b.append_data(&mut h, &rp, std::io::empty())?;
                    if md.is_dir() && !(top && LL_EMPTY_DIRS.contains(&name.as_str())) { stack.push(rp); }
                } else if md.file_type().is_symlink() {
                    let target = std::fs::read_link(e.path())?;
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    b.append_link(&mut h, &rp, &target)?;
                } else if md.is_file() {
                    // Файл меняется во время копии — это нормально (WAL при восстановлении всё исправит): читаем не
                    // больше размера на момент stat, укоротившийся дополняем нулями, исчезнувший пропускаем.
                    let Ok(f) = std::fs::File::open(e.path()) else { continue };
                    let n = md.len();
                    h.set_size(n);
                    let rd = f.take(n).chain(std::io::repeat(0)).take(n);
                    b.append_data(&mut h, &rp, rd)?;
                    files += 1;
                }
            }
        }
        let stop_sql = if major >= 15 { "select lsn::text || ' ' || encode(convert_to(labelfile, 'UTF8'), 'hex') from pg_backup_stop(true)" }
                       else { "select lsn::text || ' ' || encode(convert_to(labelfile, 'UTF8'), 'hex') from pg_stop_backup(false, true)" };
        log(&format!("файлов: {files}; pg_backup_stop — ждём архивации журнала"));
        let row = ask(stop_sql)?.first().cloned().ok_or_else(|| anyhow!("pg_backup_stop не вернул результат"))?;
        let (lsn, hexl) = row.split_once(' ').ok_or_else(|| anyhow!("неожиданный ответ pg_backup_stop: {row}"))?;
        let label = String::from_utf8(hex::decode(hexl.trim())?)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(label.len() as u64);
        h.set_mode(0o600);
        { use std::os::unix::fs::MetadataExt; h.set_uid(md0.uid() as u64); h.set_gid(md0.gid() as u64); }
        h.set_mtime(chrono::Utc::now().timestamp().max(0) as u64);
        b.append_data(&mut h, "backup_label", label.as_bytes())?;
        b.into_inner()?.flush()?;
        Ok((label, lsn.to_string()))
    })();
    let _ = writeln!(stdin, "\\q");
    drop(stdin);
    if r.is_err() { let _ = child.kill(); }
    let _ = child.wait();
    let tail = errs.join().unwrap_or_default();
    r.map_err(|e| if tail.is_empty() { e } else { anyhow!("{e:#}: {}", tail.join(" | ")) })
}

/// Файлы конфигурации вне каталога данных (имя в архиве, путь на хосте).
fn external_configs(info: &PgInfo) -> Option<Vec<(String, PathBuf)>> {
    let data = PathBuf::from(info.data_dir.as_ref()?);
    let mut out = Vec::new();
    for (name, f) in [("postgresql.conf", &info.config_file), ("pg_hba.conf", &info.hba_file), ("pg_ident.conf", &info.ident_file)] {
        if let Some(f) = f {
            let p = PathBuf::from(f);
            if !p.starts_with(&data) && p.is_file() { out.push((name.to_string(), p)); }
        }
    }
    if let Some(cf) = &info.config_file {
        let confd = Path::new(cf).parent().map(|d| d.join("conf.d"));
        if let Some(cd) = confd.filter(|d| d.is_dir() && !d.starts_with(&data)) { out.push(("conf.d".into(), cd)); }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Подготовить конфигурацию восстановленного кластера: внешние файлы — в каталог данных, пути на старый
/// каталог закомментировать, SSL выключить, если на цели нет указанных сертификатов.
async fn prepare_config(ctx: &RestoreCtx, manifest: &crate::store::types::Manifest, pgdata: &Path, log: &LogFn) -> Result<Vec<String>> {
    let mut extra = Vec::new();
    let conf = pgdata.join("postgresql.conf");
    if !conf.exists() {
        if let Some(s) = manifest.streams.iter().find(|s| s.name == "config.tar") {
            log("распаковка внешних файлов конфигурации (postgresql.conf, pg_hba.conf…) в каталог данных");
            let (reader, fetch) = ctx.repo.stream_reader(s)?;
            let dst = pgdata.to_path_buf();
            let u = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut a = tar::Archive::new(reader);
                a.unpack(&dst)?;
                let mut rest = a.into_inner();
                let _ = std::io::copy(&mut rest, &mut std::io::sink());
                Ok(())
            });
            let (u, f) = tokio::join!(u, fetch);
            u??;
            f??;
        } else {
            log("в копии нет postgresql.conf — создаётся минимальная конфигурация");
            std::fs::write(&conf, "# HopeBackup: минимальная конфигурация восстановленного кластера\nlisten_addresses = 'localhost'\n")?;
            if !pgdata.join("pg_hba.conf").exists() {
                std::fs::write(pgdata.join("pg_hba.conf"), "local all all peer\nhost all all 127.0.0.1/32 scram-sha-256\nhost all all ::1/128 scram-sha-256\n")?;
            }
        }
        let text = std::fs::read_to_string(&conf).unwrap_or_default();
        let mut outl = Vec::new();
        for l in text.lines() {
            let k = l.trim_start().split(|c: char| c == '=' || c.is_whitespace()).next().unwrap_or("");
            if ["data_directory", "hba_file", "ident_file", "external_pid_file"].contains(&k) {
                outl.push(format!("# HopeBackup (восстановление): {l}"));
            } else {
                outl.push(l.to_string());
            }
        }
        std::fs::write(&conf, outl.join("\n") + "\n")?;
    }
    // SSL: сертификаты из исходной конфигурации могут отсутствовать на цели.
    let text = std::fs::read_to_string(&conf).unwrap_or_default();
    let ssl_on = text.lines().any(|l| { let t = l.trim(); t.starts_with("ssl") && !t.starts_with("ssl_") && t.contains("on") });
    if ssl_on {
        let missing: Vec<String> = text
            .lines()
            .filter_map(|l| { let t = l.trim(); if t.starts_with("ssl_cert_file") || t.starts_with("ssl_key_file") { t.split('\'').nth(1).map(|s| s.to_string()) } else { None } })
            .filter(|p| { let pp = Path::new(p); let full = if pp.is_absolute() { pp.to_path_buf() } else { pgdata.join(pp) }; !full.exists() })
            .collect();
        if !missing.is_empty() {
            log(&format!("на цели нет сертификатов SSL ({}) — SSL в восстановленном кластере выключен", missing.join(", ")));
            extra.push("ssl = 'off'".to_string());
        }
    }
    Ok(extra)
}

/// Включить архивацию WAL в спул агента.
async fn setup_wal(conf: &PgConf, instance: &str, log: &LogFn) -> Result<Value> {
    if let Some(n) = &conf.note { log(&format!("внимание: {n}")); }
    let info = discover_info(conf, instance).await?;
    if let Some(e) = &info.connect_error { bail!("нет подключения к серверу: {e}"); }
    let bin = PathBuf::from(&info.bin_dir);
    let spool = spool_dir(instance);
    std::fs::create_dir_all(spool.join(".tmp"))?;
    if crate::util::is_root() {
        if let Some((uid, gid)) = crate::util::user_ids(&conf.os_user) { crate::util::chown_r(&spool, uid, gid)?; }
        // archive_command работает от пользователя СУБД: ему нужен проход к спулу через каталог данных
        // агента (без права чтения списка файлов — agent.json и прочее остаются закрытыми).
        use std::os::unix::fs::PermissionsExt;
        for d in [crate::agent::data_dir(), crate::agent::data_dir().join("spool")] {
            if let Ok(m) = std::fs::metadata(&d) {
                let mode = m.permissions().mode() & 0o7777;
                if mode & 0o011 != 0o011 { std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode | 0o011))?; log(&format!("{}: открыт проход пользователю СУБД ({:o})", d.display(), mode | 0o011)); }
            }
        }
    }
    let cmd = archive_command_for(instance);
    let mut restart = false;
    if info.wal_level.as_deref() == Some("minimal") {
        psql(&bin, conf, "postgres", "alter system set wal_level = 'replica'").await?;
        restart = true;
    }
    if !matches!(info.archive_mode.as_deref(), Some("on") | Some("always")) {
        psql(&bin, conf, "postgres", "alter system set archive_mode = 'on'").await?;
        restart = true;
    }
    psql(&bin, conf, "postgres", &format!("alter system set archive_command = '{}'", cmd.replace('\'', "''"))).await?;
    // Незаполненный сегмент WAL уходит в архив не позже чем через минуту — это и есть RPO при малой нагрузке.
    let at = psql(&bin, conf, "postgres", "select current_setting('archive_timeout')").await.unwrap_or_default();
    if at == "0" || at.is_empty() {
        psql(&bin, conf, "postgres", "alter system set archive_timeout = '60s'").await?;
        log("archive_timeout = 60s");
    }
    psql(&bin, conf, "postgres", "select pg_reload_conf()").await?;
    log(&format!("archive_command = {cmd}"));
    let restart_cmd = restart_command(&info);
    if restart { log(&format!("archive_mode/wal_level изменены — нужен перезапуск PostgreSQL (`{restart_cmd}`), после него архивация начнёт работать; затем — «Полная копия»")); }
    Ok(json!({ "archive_command": cmd, "restart_required": restart, "restart_command": restart_cmd, "spool": spool }))
}

/// Как перезапустить этот кластер: служба systemd, в которой работает postmaster (по cgroup процесса);
/// кластер Debian/Ubuntu — pg_ctlcluster; иначе pg_ctl от пользователя СУБД.
#[derive(Clone, Debug, PartialEq)]
pub enum Restart {
    Unit(String),
    /// GitLab Omnibus: PostgreSQL под runit — только `gitlab-ctl` (pg_ctl в обход runit поссорил бы их).
    Gitlab,
    Cluster(String, String),
    PgCtl(String),
}

impl Restart {
    pub fn command(&self) -> String {
        match self {
            Restart::Unit(u) => format!("systemctl restart {u}"),
            Restart::Gitlab => "gitlab-ctl restart postgresql".into(),
            Restart::Cluster(v, n) => format!("pg_ctlcluster {v} {n} restart"),
            Restart::PgCtl(d) => format!("pg_ctl -D {} restart -m fast   # от пользователя СУБД", crate::util::shq(d)),
        }
    }
}

fn safe_name(s: &str) -> bool { !s.is_empty() && s.len() < 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || "@._:-".contains(c)) }

/// Имя службы systemd из /proc/<pid>/cgroup (`0::/system.slice/system-postgresql.slice/postgresql@16-main.service`).
fn unit_from_cgroup(cg: &str) -> Option<String> {
    cg.split(['/', '\n']).find(|x| x.ends_with(".service") && (x.contains("postgres") || x.contains("qdb") || x.contains("pgpro")))
        .map(|x| x.trim_end_matches(".service").to_string()).filter(|u| safe_name(u))
}

/// Кластер Debian по файлу настроек `/etc/postgresql/<версия>/<имя>/postgresql.conf`.
fn debian_cluster(config_file: Option<&str>) -> Option<(String, String)> {
    let rest = config_file?.strip_prefix("/etc/postgresql/")?;
    let mut it = rest.split('/');
    let (v, n, f) = (it.next()?, it.next()?, it.next()?);
    (f == "postgresql.conf" && it.next().is_none() && safe_name(v) && safe_name(n)).then(|| (v.to_string(), n.to_string()))
}

pub fn restart_target(info: &PgInfo) -> Option<Restart> {
    let data_dir = info.data_dir.as_deref()?;
    let unit = std::fs::read_to_string(Path::new(data_dir).join("postmaster.pid")).ok()
        .and_then(|t| t.lines().next().and_then(|l| l.trim().parse::<u32>().ok()))
        .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok())
        .and_then(|cg| unit_from_cgroup(&cg));
    if let Some(u) = unit { return Some(Restart::Unit(u)); }
    if info.bin_dir.starts_with("/opt/gitlab/") && Path::new(GITLAB_CTL).exists() { return Some(Restart::Gitlab); }
    if let Some((v, n)) = debian_cluster(info.config_file.as_deref()) {
        if Path::new("/usr/bin/pg_ctlcluster").exists() { return Some(Restart::Cluster(v, n)); }
    }
    Some(Restart::PgCtl(data_dir.to_string()))
}

const GITLAB_CTL: &str = "/opt/gitlab/bin/gitlab-ctl";

fn restart_command(info: &PgInfo) -> String {
    restart_target(info).map(|r| r.command()).unwrap_or_else(|| "systemctl restart postgresql".into())
}

/// Перезапуск кластера по кнопке в панели (после «Настроить архив WAL»): агент сам выбирает способ
/// (`restart_target`) — команду из панели он не принимает. Затем ждёт, пока СУБД снова примет подключения,
/// и проверяет, что изменения, требовавшие перезапуска, применились.
async fn restart_pg(conf: &PgConf, instance: &str, log: &LogFn) -> Result<Value> {
    if !crate::util::is_root() { bail!("агент работает не от root — перезапустите PostgreSQL на сервере вручную"); }
    if let Some(n) = &conf.note { log(&format!("внимание: {n}")); }
    let info = discover_info(conf, instance).await?;
    if let Some(e) = &info.connect_error { bail!("нет подключения к серверу: {e}"); }
    let bin = PathBuf::from(&info.bin_dir);
    let target = restart_target(&info).ok_or_else(|| anyhow!("не удалось определить каталог данных кластера"))?;
    log(&format!("перезапуск PostgreSQL {}: {}", info.version, target.command()));
    let t0 = Instant::now();
    let mut c = match &target {
        Restart::Unit(u) => { let mut c = std::process::Command::new("systemctl"); c.args(["restart", u.as_str()]); c }
        Restart::Gitlab => { let mut c = std::process::Command::new(GITLAB_CTL); c.args(["restart", "postgresql"]); c }
        Restart::Cluster(v, n) => { let mut c = std::process::Command::new("/usr/bin/pg_ctlcluster"); c.args([v.as_str(), n.as_str(), "restart", "-m", "fast"]); c }
        Restart::PgCtl(d) => {
            // Без -l postmaster унаследовал бы наш канал вывода, и ожидание вывода pg_ctl не закончилось бы.
            let dir = crate::agent::data_dir().join("pg-restart");
            std::fs::create_dir_all(&dir)?;
            if let Some((uid, gid)) = crate::util::user_ids(&conf.os_user) { crate::util::chown_r(&dir, uid, gid)?; }
            let mut c = pg_cmd(&bin, "pg_ctl", conf);
            c.args(["-D", d.as_str(), "restart", "-m", "fast", "-w", "-t", "120", "-l"]).arg(dir.join(format!("{instance}.log")));
            c
        }
    };
    c.stdin(std::process::Stdio::null());
    let out = tokio::time::timeout(std::time::Duration::from_secs(180), tokio::task::spawn_blocking(move || c.output())).await
        .map_err(|_| anyhow!("перезапуск не завершился за 3 минуты — проверьте службу на сервере"))??
        .context("запуск команды перезапуска")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("перезапуск не удался ({}): {}", target.command(), if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err });
    }
    // СУБД после перезапуска может ещё восстанавливаться — ждём подключения до двух минут.
    let mut up = false;
    for _ in 0..120 {
        if psql(&bin, conf, "postgres", "select 1").await.is_ok() { up = true; break; }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !up { bail!("после перезапуска PostgreSQL не принимает подключения 2 минуты — проверьте журнал СУБД на сервере"); }
    let secs = t0.elapsed().as_secs();
    let after = discover_info(conf, instance).await?;
    log(&format!("PostgreSQL снова принимает подключения ({secs} с); archive_mode = {}, ожидают перезапуска: {}",
        after.archive_mode.clone().unwrap_or_default(), if after.pending_restart { "есть" } else { "нет" }));
    if after.archive_ok { log("архив WAL работает — следующий шаг: «Полная копия»"); }
    Ok(json!({ "restarted": target.command(), "seconds": secs, "archive_mode": after.archive_mode, "archive_ok": after.archive_ok, "pending_restart": after.pending_restart }))
}

/// `archive_command`: положить сегмент в локальный спул (быстро и надёжно; на площадку его отправит агент).
/// Так медленный канал не тормозит СУБД, а сбой связи не переполняет pg_wal повторами.
pub fn wal_push(spool: &Path, path: &Path) -> Result<()> {
    let name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("неверный путь сегмента"))?;
    let dst = spool.join(name);
    let data = std::fs::read(path).with_context(|| format!("чтение {}", path.display()))?;
    if let Ok(old) = std::fs::read(&dst) {
        if old == data { return Ok(()); }
        bail!("в спуле уже есть другой {name}");
    }
    let tmpdir = spool.join(".tmp");
    std::fs::create_dir_all(&tmpdir)?;
    let tmp = tmpdir.join(format!("{name}.{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, &data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &dst)?;
    if let Ok(d) = std::fs::File::open(spool) { let _ = d.sync_all(); }
    Ok(())
}

/// Файл задания восстановления для restore_command (доступ к площадке и мастер-ключ; 0600).
#[derive(Serialize, Deserialize)]
pub struct WalFetchJob {
    pub remote: crate::store::Remote,
    pub master_hex: String,
    pub ns: String,
}

/// `restore_command`: достать сегмент с площадки. Код 1 без сообщения — «сегмента нет» (конец журнала).
pub async fn wal_fetch(job: &Path, name: &str, dst: &Path) -> Result<bool> {
    let j: WalFetchJob = serde_json::from_slice(&std::fs::read(job).with_context(|| format!("файл задания {}", job.display()))?)?;
    let be = crate::store::backend_for(&j.remote)?;
    let m: [u8; 32] = hex::decode(&j.master_hex)?.try_into().map_err(|_| anyhow!("неверный ключ"))?;
    let repo = crate::store::Repo::open_with_key(be, m).await?;
    match repo.get_object(&j.ns, name).await? {
        Some(data) => {
            let tmp = dst.with_extension("hbtmp");
            std::fs::write(&tmp, &data)?;
            std::fs::rename(&tmp, dst)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[derive(Deserialize)]
struct RestoreParams {
    pgdata: String,
    #[serde(default)]
    bin_dir: Option<String>,
    #[serde(default = "d_user")]
    os_user: String,
    #[serde(default)]
    target_time: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default = "d_true")]
    start: bool,
    /// `cluster` — кластер целиком; `dump` — одна база из логического дампа в работающий сервер.
    #[serde(default = "d_cluster")]
    mode: String,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    host: Option<String>,
    /// Нет PostgreSQL нужной версии — установить (репозиторий дистрибутива, иначе PGDG).
    #[serde(default)]
    install: bool,
}
fn d_true() -> bool { true }

/// Установить PostgreSQL мажорной версии `major` на цель (чистая ВМ при восстановлении).
/// Debian/Ubuntu/Astra — пакет дистрибутива или apt.postgresql.org; RHEL-семейство — yum.postgresql.org.
/// Пустой кластер, который создаёт пакет Debian, удаляется: на его месте будет восстановленный.
async fn install_postgres(major: u32, log: &LogFn) -> Result<()> {
    let script = format!(r#"set -e
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update -q
  if ! apt-get install -y -q postgresql-{major} 2>/dev/null; then
    apt-get install -y -q curl ca-certificates gnupg lsb-release
    install -d /usr/share/postgresql-common/pgdg
    curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc https://www.postgresql.org/media/keys/ACCC4CF8.asc
    CODENAME=$(. /etc/os-release; echo ${{VERSION_CODENAME:-$(lsb_release -cs)}})
    echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt $CODENAME-pgdg main" > /etc/apt/sources.list.d/pgdg.list
    apt-get update -q
    apt-get install -y -q postgresql-{major}
  fi
  if command -v pg_lsclusters >/dev/null 2>&1 && pg_lsclusters -h | awk '{{print $1" "$2}}' | grep -qx "{major} main"; then
    pg_dropcluster --stop {major} main
  fi
elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; then
  PM=$(command -v dnf || command -v yum)
  REL=$(rpm -E %rhel)
  $PM install -y -q https://download.postgresql.org/pub/repos/yum/reporpms/EL-$REL-x86_64/pgdg-redhat-repo-latest.noarch.rpm || true
  $PM -qy module disable postgresql 2>/dev/null || true
  $PM install -y -q postgresql{major}-server
else
  echo "неизвестный менеджер пакетов" >&2; exit 1
fi
"#);
    log(&format!("установка PostgreSQL {major} на цель"));
    let out = tokio::process::Command::new("sh").arg("-c").arg(&script).output().await?;
    let err = String::from_utf8_lossy(&out.stderr);
    for l in err.lines().filter(|l| !l.trim().is_empty() && !l.contains("debconf") && !l.starts_with("W:")).take(15) { log(&format!("  {l}")); }
    if !out.status.success() { bail!("установка PostgreSQL {major} не удалась (код {})", out.status.code().unwrap_or(-1)); }
    Ok(())
}
fn d_cluster() -> String { "cluster".into() }

async fn restore(ctx: &RestoreCtx) -> Result<Value> {
    let p: RestoreParams = serde_json::from_value(ctx.params.clone()).context("параметры восстановления")?;
    let log = ctx.log.clone();
    let (header, manifest) = ctx.repo.manifest(&ctx.snapshot_id).await?;
    let major = header.extra.get("major").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let flavor = header.extra.get("flavor").and_then(|v| v.as_str()).unwrap_or("postgresql").to_string();
    // Утилиты той же мажорной версии и того же форка, что в копии.
    let bin = match &p.bin_dir {
        Some(b) if !b.is_empty() => {
            let b = PathBuf::from(b);
            if !trusted_bin_dir(&b) { bail!("каталог {} (или файл postgres) не принадлежит root либо доступен на запись другим — такие программы агент не запускает", b.display()); }
            b
        }
        _ => {
            let mut found = None;
            for d in find_bin_dirs() {
                let mut vc = std::process::Command::new(d.join("postgres"));
                vc.arg("--version");
                if let Ok(vs) = output(vc).await {
                    if parse_version(&vs).map(|v| v.1) == Some(major) && detect_flavor(&d.to_string_lossy(), &vs) == flavor { found = Some(d); break; }
                }
            }
            match found {
                Some(d) => d,
                None if p.install && !is_qdb(&flavor) && major >= MIN_MAJOR => {
                    install_postgres(major, &log).await?;
                    let mut f = None;
                    for d in find_bin_dirs() {
                        let mut vc = std::process::Command::new(d.join("postgres"));
                        vc.arg("--version");
                        if let Ok(vs) = output(vc).await { if parse_version(&vs).map(|v| v.1) == Some(major) { f = Some(d); break; } }
                    }
                    f.ok_or_else(|| anyhow!("PostgreSQL {major} установлен, но бинарники не найдены"))?
                }
                None if is_qdb(&flavor) => bail!("на цели нет qdatabase {major}: установите ту же сборку qdatabase (/opt/qdatabase-*), что в копии"),
                None => bail!("на цели нет PostgreSQL {major}: установите ту же версию, что в копии, или включите «Установить PostgreSQL»"),
            }
        }
    };
    let mut vc = std::process::Command::new(bin.join("postgres"));
    vc.arg("--version");
    let vs = output(vc).await?;
    let tmajor = parse_version(&vs).map(|v| v.1).unwrap_or(0);
    if major != 0 && tmajor != major { bail!("в копии {flavor} {major}, на цели {} — физическая копия восстанавливается только в ту же мажорную версию", vs.trim()); }
    log(&format!("цель: {} ({})", vs.trim(), bin.display()));

    if p.mode == "dump" {
        let db = p.database.clone().ok_or_else(|| anyhow!("укажите базу (database)"))?;
        let s = manifest.streams.iter().find(|s| s.name == format!("dump/{db}.pgdump")).ok_or_else(|| anyhow!("в копии нет дампа базы {db}"))?;
        let conf = PgConf { bin_dir: None, host: p.host.clone().unwrap_or_else(d_host), port: p.port.unwrap_or(5432), user: p.os_user.clone(), password: None, os_user: p.os_user.clone(), databases: vec![], backup_method: None, note: None };
        let mut c = pg_cmd(&bin, "pg_restore", &conf);
        c.args(["--create", "--no-owner", "-d", "postgres"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = c.spawn()?;
        let errs = pipe_stderr(&mut child, log.clone(), "pg_restore");
        let stdin = child.stdin.take().unwrap();
        ctx.repo.restore_stream(s, stdin).await?;
        let st = tokio::task::spawn_blocking(move || child.wait()).await??;
        let tail = errs.join().unwrap_or_default();
        if !st.success() { bail!("pg_restore: {}", tail.join(" | ")); }
        return Ok(json!({ "mode": "dump", "database": db }));
    }

    let pgdata = PathBuf::from(&p.pgdata);
    if pgdata.exists() && std::fs::read_dir(&pgdata)?.next().is_some() {
        bail!("каталог {} не пуст — восстановление в непустой каталог запрещено", pgdata.display());
    }
    std::fs::create_dir_all(&pgdata)?;
    std::fs::set_permissions(&pgdata, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let base = manifest.streams.iter().find(|s| s.name == "base.tar").ok_or_else(|| anyhow!("в копии нет базовой копии кластера"))?;
    log(&format!("распаковка базовой копии ({}) в {}", crate::util::human_bytes(base.size), pgdata.display()));
    let (reader, fetch) = ctx.repo.stream_reader(base)?;
    let dst = pgdata.clone();
    let unpack = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut a = tar::Archive::new(reader);
        a.set_preserve_permissions(true);
        a.set_overwrite(false);
        a.unpack(&dst).context("распаковка tar")?;
        // Дочитать хвост, чтобы поток выборки завершился штатно.
        let mut rest = a.into_inner();
        let _ = std::io::copy(&mut rest, &mut std::io::sink());
        Ok(())
    });
    let (u, f) = tokio::join!(unpack, fetch);
    u??;
    f??;
    for d in ["pg_wal/archive_status", "pg_replslot", "pg_tblspc", "pg_stat_tmp", "pg_notify", "pg_serial", "pg_snapshots", "pg_twophase", "pg_commit_ts", "pg_dynshmem", "pg_logical/snapshots", "pg_logical/mappings"] {
        let _ = std::fs::create_dir_all(pgdata.join(d));
    }
    let _ = std::fs::remove_file(pgdata.join("postmaster.pid"));

    let conf_extra = prepare_config(ctx, &manifest, &pgdata, &log).await?;

    // Задание для restore_command: доступ к площадке и ключ — только пользователю СУБД.
    // Каталог данных агента закрыт (0700), пользователь СУБД туда не пройдёт: от root задание лежит в /run
    // (tmpfs — ключ не остаётся на диске), каталог задания доступен только группе пользователя СУБД.
    let jobdir = if crate::util::is_root() {
        let run = std::path::Path::new("/run/hopebackup");
        std::fs::create_dir_all(run)?;
        std::fs::set_permissions(run, std::os::unix::fs::PermissionsExt::from_mode(0o711))?;
        run.join("restore")
    } else {
        crate::agent::data_dir().join("restore")
    };
    std::fs::create_dir_all(&jobdir)?;
    let jobfile = jobdir.join(format!("{}.json", ctx.job_id));
    let job = WalFetchJob { remote: ctx.remote.clone(), master_hex: hex::encode(ctx.repo.keys.master()), ns: format!("wal/{}", ctx.instance) };
    crate::util::write_json(&jobfile, &job, true)?;
    let exe = std::env::current_exe()?.to_string_lossy().to_string();
    let mut conf = format!(
        "\n# HopeBackup: восстановление {} из снапшота {}\nrestore_command = '{} wal-fetch --job {} %f %p'\nrecovery_target_action = 'promote'\narchive_mode = 'off'\n",
        ctx.job_id, ctx.snapshot_id, exe, jobfile.display()
    );
    if let Some(t) = p.target_time.as_ref().filter(|t| !t.trim().is_empty()) {
        conf.push_str(&format!("recovery_target_time = '{}'\n", t.replace('\'', "")));
        log(&format!("восстановление на момент {t}"));
    } else {
        log("восстановление до конца доступного журнала WAL");
    }
    if let Some(port) = p.port { conf.push_str(&format!("port = {port}\n")); }
    for l in &conf_extra { conf.push_str(l); conf.push('\n'); }
    let auto = pgdata.join("postgresql.auto.conf");
    let mut cur = std::fs::read_to_string(&auto).unwrap_or_default();
    cur.push_str(&conf);
    std::fs::write(&auto, cur)?;
    std::fs::write(pgdata.join("recovery.signal"), b"")?;
    if crate::util::is_root() {
        let (uid, gid) = crate::util::user_ids(&p.os_user).ok_or_else(|| anyhow!("нет пользователя ОС {}", p.os_user))?;
        crate::util::chown_r(&pgdata, uid, gid)?;
        std::os::unix::fs::chown(&jobfile, Some(uid), Some(gid))?;
        std::os::unix::fs::chown(&jobdir, Some(0), Some(gid))?;
        std::fs::set_permissions(&jobdir, std::os::unix::fs::PermissionsExt::from_mode(0o750))?;
    }
    let mut result = json!({ "pgdata": pgdata, "bin_dir": bin, "target_time": p.target_time, "started": false });
    if !p.start { log("кластер подготовлен, запуск не запрошен"); return Ok(result); }

    // Каталог сокета из конфигурации кластера.
    let mut c = std::process::Command::new(bin.join("postgres"));
    c.args(["-C", "unix_socket_directories", "-D"]).arg(&pgdata);
    as_os_user(&mut c, &p.os_user);
    let sock = output(c).await.ok().and_then(|s| s.split(',').next().map(|x| x.trim().to_string())).filter(|s| !s.is_empty()).unwrap_or_else(|| "/tmp".into());
    if !Path::new(&sock).exists() {
        std::fs::create_dir_all(&sock)?;
        if crate::util::is_root() { if let Some((u, g)) = crate::util::user_ids(&p.os_user) { std::os::unix::fs::chown(&sock, Some(u), Some(g))?; } }
    }
    let mut c = std::process::Command::new(bin.join("postgres"));
    c.args(["-C", "port", "-D"]).arg(&pgdata);
    as_os_user(&mut c, &p.os_user);
    let port: u16 = output(c).await.ok().and_then(|s| s.trim().parse().ok()).unwrap_or(5432);
    let logfile = pgdata.join("hopebackup-restore.log");
    log(&format!("запуск сервера (порт {port}), журнал восстановления — {}", logfile.display()));
    let mut c = std::process::Command::new(bin.join("pg_ctl"));
    c.args(["-D"]).arg(&pgdata).args(["-l"]).arg(&logfile).args(["-W", "start"]);
    as_os_user(&mut c, &p.os_user);
    output(c).await.context("pg_ctl start")?;
    let conf = PgConf { bin_dir: None, host: sock.clone(), port, user: p.os_user.clone(), password: None, os_user: p.os_user.clone(), databases: vec![], backup_method: None, note: None };
    let started = Instant::now();
    let mut last_log = Instant::now() - Duration::from_secs(60);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match psql(&bin, &conf, "postgres", "select pg_is_in_recovery()::text || '|' || coalesce(pg_last_wal_replay_lsn()::text, '')").await {
            Ok(r) if r.starts_with("false") => break,
            Ok(r) => {
                if last_log.elapsed() > Duration::from_secs(10) { last_log = Instant::now(); log(&format!("идёт восстановление, применено до {}", r.split('|').nth(1).unwrap_or("?"))); }
            }
            Err(_) => {
                // Сервер мог остановиться (например, цель восстановления недостижима) — смотрим журнал.
                let mut st = std::process::Command::new(bin.join("pg_ctl"));
                st.args(["-D"]).arg(&pgdata).arg("status");
                as_os_user(&mut st, &p.os_user);
                if output(st).await.is_err() {
                    let tail = std::fs::read_to_string(&logfile).unwrap_or_default();
                    let tail: Vec<&str> = tail.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                    bail!("сервер остановился во время восстановления:\n{}", tail.join("\n"));
                }
            }
        }
        if started.elapsed() > Duration::from_secs(6 * 3600) { bail!("восстановление не завершилось за 6 часов"); }
    }
    // Восстановление закончено: убрать доступ к площадке из конфигурации и сам файл задания.
    for s in ["alter system reset restore_command", "alter system reset recovery_target_time", "alter system reset recovery_target_action", "select pg_reload_conf()"] {
        let _ = psql(&bin, &conf, "postgres", s).await;
    }
    let _ = std::fs::remove_file(&jobfile);
    let dbs = psql(&bin, &conf, "postgres", "select string_agg(datname || ':' || pg_database_size(datname), ',') from pg_database where not datistemplate").await.unwrap_or_default();
    let dbs: Vec<Value> = dbs.split(',').filter(|s| !s.is_empty()).map(|s| { let (n, sz) = s.split_once(':').unwrap_or((s, "0")); json!({ "name": n, "size": sz.parse::<u64>().unwrap_or(0) }) }).collect();
    log(&format!("сервер работает, восстановление завершено за {} с; баз: {}", started.elapsed().as_secs(), dbs.len()));
    result["started"] = json!(true);
    result["port"] = json!(port);
    result["socket"] = json!(sock);
    result["databases"] = json!(dbs);
    result["seconds"] = json!(started.elapsed().as_secs());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_omnibus_autodetect() {
        let pid = "1234\n/var/opt/gitlab/postgresql/data\n1730000000\n5432\n/var/opt/gitlab/postgresql\n\n  5432001  0\nready   \n";
        assert_eq!(parse_postmaster_pid(pid), (5432, "/var/opt/gitlab/postgresql".to_string()));
        let gl = RunningPg { bin_dir: "/opt/gitlab/embedded/postgresql/14/bin".into(), data_dir: "/var/opt/gitlab/postgresql/data".into(), port: 5432, socket_dir: "/var/opt/gitlab/postgresql".into(), owner: "root".into() };
        // Владелец «root» как пример существующего пользователя ОС (gitlab-psql в тестовой машине нет).
        let mut c = PgConf::from(&json!({ "host": "/nonexistent-hb-sock" })).unwrap();
        c.note = None; c.host = "/nonexistent-hb-sock".into();
        c.autodetect(&[gl.clone()]);
        assert_eq!((c.host.as_str(), c.port, c.user.as_str(), c.os_user.as_str()), ("/var/opt/gitlab/postgresql", 5432, "root", "root"));
        assert!(c.note.as_deref().unwrap().contains("встроенная PostgreSQL GitLab"), "{:?}", c.note);
        // Роль задана явно — не трогаем, меняем только сокет.
        let mut c = PgConf::from(&json!({ "host": "/nonexistent-hb-sock", "user": "backup", "os_user": "backup" })).unwrap();
        c.host = "/nonexistent-hb-sock".into(); c.autodetect(&[gl.clone()]);
        assert_eq!((c.host.as_str(), c.user.as_str()), ("/var/opt/gitlab/postgresql", "backup"));
        // Два кластера и bin_dir не указан — не угадываем.
        let mut c = PgConf::from(&json!({ "host": "/nonexistent-hb-sock" })).unwrap();
        c.host = "/nonexistent-hb-sock".into(); c.note = None;
        c.autodetect(&[gl.clone(), RunningPg { bin_dir: "/usr/lib/postgresql/16/bin".into(), port: 5433, socket_dir: "/run/x".into(), ..Default::default() }]);
        assert!(c.note.is_none() && c.host == "/nonexistent-hb-sock");
        // bin_dir другого кластера — не подключаемся к GitLab.
        let mut c = PgConf::from(&json!({ "host": "/nonexistent-hb-sock", "bin_dir": "/usr/lib/postgresql/16/bin" })).unwrap();
        c.host = "/nonexistent-hb-sock".into(); c.note = None; c.autodetect(&[gl.clone()]);
        assert!(c.note.is_none());
        assert_eq!(Restart::Gitlab.command(), "gitlab-ctl restart postgresql");
    }

    #[test]
    fn restart_targets() {
        assert_eq!(unit_from_cgroup("0::/system.slice/system-postgresql.slice/postgresql@16-main.service\n"), Some("postgresql@16-main".into()));
        assert_eq!(unit_from_cgroup("0::/system.slice/postgresql-15.service"), Some("postgresql-15".into()));
        assert_eq!(unit_from_cgroup("0::/user.slice/user-0.slice/session-3.scope"), None);
        assert_eq!(unit_from_cgroup("0::/system.slice/postgres;rm -rf.service"), None);
        assert_eq!(debian_cluster(Some("/etc/postgresql/16/main/postgresql.conf")), Some(("16".into(), "main".into())));
        assert_eq!(debian_cluster(Some("/var/lib/pgsql/16/data/postgresql.conf")), None);
        assert_eq!(debian_cluster(Some("/etc/postgresql/16/main/x/postgresql.conf")), None);
        assert_eq!(Restart::Cluster("16".into(), "main".into()).command(), "pg_ctlcluster 16 main restart");
    }

    #[test]
    fn explains_connect_errors() {
        let conf = PgConf { bin_dir: None, host: "/var/run/postgresql".into(), port: 5432, user: "zabbix".into(), password: Some("x".into()), os_user: "root".into(), databases: vec![], backup_method: None, note: None };
        let e = "psql: error: connection to server on socket \"/var/run/postgresql/.s.PGSQL.5432\" failed: FATAL:  Peer authentication failed for user \"zabbix\"";
        let h = explain(e, &conf, "root", &[]).unwrap();
        assert!(h.contains("пользователь ОС — «root», роль СУБД — «zabbix»") && h.contains("= postgres") && h.contains("вкладка «Параметры»"), "{h}");
        assert!(explain("FATAL:  password authentication failed for user \"zabbix\"", &conf, "root", &[]).unwrap().contains("неверный пароль"));
        assert!(explain("FATAL:  role \"zz\" does not exist", &conf, "root", &[]).unwrap().contains("роли «zabbix»"));
        assert!(explain("ERROR:  must be superuser to execute ALTER SYSTEM command", &conf, "root", &[]).unwrap().contains("ALTER ROLE zabbix SUPERUSER"));
        assert!(explain("connect: No such file or directory\n\tIs the server running locally and accepting connections on Unix domain socket \"/var/run/postgresql/.s.PGSQL.5432\"?", &conf, "root", &[]).unwrap().contains("не отвечает"));
        assert!(explain("ERROR: syntax error", &conf, "root", &[]).is_none());
        // GitLab Omnibus: совет — gitlab-psql, а не postgres (роль gitlab — роль приложения).
        let gl = RunningPg { bin_dir: "/opt/gitlab/embedded/postgresql/14/bin".into(), data_dir: "/var/opt/gitlab/postgresql/data".into(), port: 5432, socket_dir: "/var/opt/gitlab/postgresql".into(), owner: "gitlab-psql".into() };
        let gconf = PgConf { bin_dir: None, host: "/var/opt/gitlab/postgresql".into(), port: 5432, user: "gitlab".into(), password: None, os_user: "zabbix".into(), databases: vec![], backup_method: None, note: None };
        let h = explain("FATAL:  Peer authentication failed for user \"gitlab\"", &gconf, "zabbix", &[gl]).unwrap();
        assert!(h.contains("= gitlab-psql и «Пользователь ОС сервера СУБД» = gitlab-psql") && h.contains("встроенная PostgreSQL GitLab") && !h.contains("= postgres"), "{h}");
        // Без работающего кластера, но по пути GitLab — тоже gitlab-psql.
        assert!(explain("FATAL:  Peer authentication failed for user \"gitlab\"", &gconf, "zabbix", &[]).unwrap().contains("= gitlab-psql"));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("postgres (PostgreSQL) 16.4 (Ubuntu 16.4-1.pgdg22.04+1)"), Some(("16.4".into(), 16)));
        assert_eq!(parse_version("postgres (PostgreSQL) 14.12"), Some(("14.12".into(), 14)));
        assert_eq!(parse_version("postgres (QDatabase) 15.0"), Some(("15.0".into(), 15)));
        assert_eq!(detect_flavor("/opt/qdatabase-15.0/bin", "postgres (PostgreSQL) 15.0"), "qdatabase");
        assert_eq!(detect_flavor("/usr/lib/postgresql/16/bin", "postgres (PostgreSQL) 16.4"), "postgresql");
    }

    #[test]
    fn segments() {
        assert_eq!(wal_segment(1, "0/2000028", 16 << 20).unwrap(), "000000010000000000000002");
        assert_eq!(wal_segment(2, "1/A3000000", 16 << 20).unwrap(), "0000000200000001000000A3");
        let (s, t) = parse_backup_label("START WAL LOCATION: 0/2000028 (file 000000010000000000000002)\nSTART TIMELINE: 1\n");
        assert_eq!(s.unwrap(), "000000010000000000000002");
        assert_eq!(t, Some(1));
        assert_eq!(parse_size("16MB"), Some(16 << 20));
    }
}
