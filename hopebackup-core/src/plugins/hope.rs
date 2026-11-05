//! Продукты Hope: Hope Mail Server, Hope MSG Gateway, HopeInventory, HopeDFS, HopeDC.
//!
//! У всех одна схема: служба systemd, каталог данных (WAL, JSON, redb), секреты (иногда вне каталога
//! данных), программа в /opt или /usr/local/bin. Плагин находит всё это по юниту службы, снимает
//! согласованную копию — из снимка ФС, а без него при короткой остановке службы (только для данных,
//! которым она нужна) — и восстанавливает с владельцем и проверкой, что служба снова отвечает.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, svc, Cmd, Consistency, Untar};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::exclude::Excludes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mail,
    Gateway,
    Inventory,
    Dfs,
    Dc,
}

pub struct HopeApp(pub Kind);

impl Kind {
    pub fn id(self) -> &'static str {
        match self { Kind::Mail => "hopemail", Kind::Gateway => "hopegw", Kind::Inventory => "hopeinv", Kind::Dfs => "hopedfs", Kind::Dc => "hopedc" }
    }
    fn title(self) -> &'static str {
        match self { Kind::Mail => "Hope Mail Server", Kind::Gateway => "Hope MSG Gateway", Kind::Inventory => "HopeInventory", Kind::Dfs => "HopeDFS", Kind::Dc => "HopeDC" }
    }
    fn units(self) -> &'static [&'static str] {
        match self {
            Kind::Mail => &["hopedb", "hope-mail", "hopemail", "hope-mail-server"],
            Kind::Gateway => &["hope-msg-gateway"],
            Kind::Inventory => &["hopeinv"],
            Kind::Dfs => &["hopedfs"],
            Kind::Dc => &["hopedc"],
        }
    }
}

/// Когда часть копируется: при остановленной службе (или из снимка) — или на ходу после неё.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Quiesced,
    Live,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    /// Имя потока в копии (`data`, `blobs`, `share-docs`…).
    pub name: String,
    pub title: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub phase: Phase,
    /// Владелец после восстановления (пользователь службы); пусто — как в архиве.
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub xattrs: bool,
    /// Восстановление заменяет каталог целиком (прежний — в «.before-restore-…»), а не дописывает поверх.
    #[serde(default)]
    pub replace: bool,
}

impl Part {
    /// Часть с данными службы: копируется согласованно, при восстановлении заменяется целиком.
    pub fn data(name: &str, title: &str, paths: Vec<String>, exclude: Vec<String>, owner: &str) -> Part {
        Part { name: name.into(), title: title.into(), paths, exclude, phase: Phase::Quiesced, owner: owner.into(), xattrs: false, replace: true }
    }
    /// Конфигурация и программа: на ходу, восстанавливается поверх.
    pub fn config(paths: Vec<String>) -> Part {
        Part { name: "config".into(), title: "Конфигурация и программа".into(), paths, exclude: vec![], phase: Phase::Live, owner: String::new(), xattrs: false, replace: false }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Layout {
    pub service: String,
    pub user: String,
    pub parts: Vec<Part>,
    pub health: String,
    pub bin: String,
    pub version: String,
    /// Без снимка ФС не останавливать службу (файловый сервер — простой недопустим), копировать на ходу.
    #[serde(default)]
    pub live_without_snapshot: bool,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Служба — контейнер Docker (останавливается docker stop/start), а не юнит systemd.
    #[serde(default)]
    pub container: String,
}

impl Layout {
    pub fn svc_exists(&self) -> bool {
        if !self.container.is_empty() { return std::process::Command::new("docker").args(["inspect", "--type", "container", &self.container]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false); }
        !self.service.is_empty() && svc::exists(&self.service)
    }
    pub fn svc_active(&self) -> bool {
        if !self.container.is_empty() {
            return std::process::Command::new("docker").args(["inspect", "-f", "{{.State.Running}}", &self.container]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true").unwrap_or(false);
        }
        svc::active(&self.service)
    }
    pub async fn svc_stop(&self, log: &LogFn) -> Result<()> {
        if !self.container.is_empty() { return kit::run(&Cmd::new("docker").args(["stop", &self.container]), log).await; }
        svc::stop(&self.service, log).await
    }
    pub async fn svc_start(&self, log: &LogFn) -> Result<()> {
        if !self.container.is_empty() { return kit::run(&Cmd::new("docker").args(["start", &self.container]), log).await; }
        svc::start(&self.service, log).await
    }
    /// Выполнить `f` при остановленной службе (или контейнере) и обязательно запустить обратно.
    pub async fn while_stopped<T, F, Fut>(&self, log: &LogFn, f: F) -> Result<T>
    where F: FnOnce() -> Fut, Fut: std::future::Future<Output = Result<T>> {
        if self.container.is_empty() { return svc::while_stopped(std::slice::from_ref(&self.service), log, f).await; }
        let was = self.svc_active();
        let t0 = std::time::Instant::now();
        if was { self.svc_stop(log).await?; }
        let r = f().await;
        if was {
            let st = self.svc_start(log).await;
            log(&format!("контейнер {} был остановлен {:.1} с", self.container, t0.elapsed().as_secs_f64()));
            if let Err(e) = st { return Err(e.context("копия снята, но контейнер не запустился обратно")); }
        }
        r
    }
}

// ---------------------------------------------------------------- параметры

fn common_props(k: Kind) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("service".into(), json!({ "type": "string", "title": "Служба", "description": format!("Пусто — найти: {}", k.units().join(", ")) }));
    m.insert("data_dir".into(), json!({ "type": "string", "title": "Каталог данных", "description": "Пусто — из юнита службы" }));
    m.insert("consistency".into(), kit::consistency_schema(match k {
        Kind::Dfs => "Для HopeDFS без снимка ФС служба не останавливается (auto = снимок или на ходу).",
        Kind::Mail => "При остановке служба стоит только на время копии WAL и настроек; тела писем копируются после запуска.",
        Kind::Dc => "Остановка гасит DNS, Kerberos и NTP домена — лучше снимок ФС.",
        _ => "",
    }));
    m.insert("include_program".into(), json!({ "type": "boolean", "title": "Копировать программу и юниты", "default": true, "description": "Для восстановления той же версии на чистую машину" }));
    m.insert("include_logs".into(), json!({ "type": "boolean", "title": "Копировать журналы", "default": k == Kind::Inventory || k == Kind::Dfs, "description": "Журнал аудита и журналы службы" }));
    m.insert("exclude".into(), json!({ "type": "array", "items": { "type": "string" }, "title": "Исключения" }));
    m
}

pub fn schema(k: Kind) -> Value {
    let mut m = common_props(k);
    match k {
        Kind::Mail => {
            m.insert("blob_dir".into(), json!({ "type": "string", "title": "Каталог тел писем", "description": "Пусто — из настроек хранилища (storage.json) или BLOB_DATA_DIR" }));
            m.insert("include_backups".into(), json!({ "type": "boolean", "title": "Копировать встроенные бэкапы (hopedb_backups)", "default": false }));
        }
        Kind::Gateway => {
            m.insert("include_master_key".into(), json!({ "type": "boolean", "title": "Копировать мастер-ключ /etc/hope-msg/master.key", "default": true, "description": "Без него секреты из копии не расшифровать. Копия зашифрована паролем репозитория" }));
            m.insert("include_av".into(), json!({ "type": "boolean", "title": "Копировать антивирусные базы", "default": false, "description": "Базы скачиваются заново" }));
        }
        Kind::Dfs => {
            m.insert("shares".into(), json!({ "type": "array", "items": { "type": "string" }, "title": "Шары", "description": "Имена; пусто — все шары из cluster_state.json" }));
            m.insert("include_previous_versions".into(), json!({ "type": "boolean", "title": "Копировать «Предыдущие версии» (.hopedfs/snapshots)", "default": false }));
            m.insert("include_ransom_copies".into(), json!({ "type": "boolean", "title": "Копировать страховочные копии защиты от шифровальщиков", "default": false }));
        }
        Kind::Dc => {
            m.insert("key_mode".into(), json!({ "type": "string", "title": "Мастер-ключ базы", "enum": ["export", "file", "none"], "default": "export",
                "description": "export — «hopedc-core key export» с паролем (рекомендуется); file — сам файл ключа; none — ключ храните сами (без него база не читается)" }));
            m.insert("key_password".into(), json!({ "type": "string", "format": "secret", "title": "Пароль экспорта ключа", "description": "Нужен для key_mode = export и для восстановления" }));
        }
        Kind::Inventory => {}
    }
    json!({ "type": "object", "properties": Value::Object(m) })
}

pub fn defaults(k: Kind) -> Value {
    match k {
        Kind::Dc => json!({ "consistency": "auto", "key_mode": "export", "include_program": true }),
        _ => json!({ "consistency": "auto", "include_program": true }),
    }
}

pub fn restore_schema(k: Kind) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("parts".into(), json!({ "type": "array", "items": { "type": "string" }, "title": "Что восстановить", "description": "Части копии (data, blobs, config, program, logs, secrets, share-…); пусто — всё" }));
    m.insert("target_root".into(), json!({ "type": "string", "title": "Корень восстановления", "default": "/", "description": "«/» — на место (служба останавливается и запускается); другой каталог — только распаковать для проверки" }));
    m.insert("start".into(), json!({ "type": "boolean", "title": "Запустить службу после восстановления", "default": true }));
    if k == Kind::Dc {
        m.insert("mode".into(), json!({ "type": "string", "title": "Режим", "enum": ["full", "rejoin"], "default": "full",
            "description": "full — база из копии (узел становится неавторитетной копией, остальные узлы кластера пересоберите); rejoin — только cluster_config.json и ключ: узел заберёт данные у живого пира" }));
        m.insert("key_password".into(), json!({ "type": "string", "format": "secret", "title": "Пароль ключа из копии" }));
    }
    json!({ "type": "object", "properties": Value::Object(m) })
}

// ---------------------------------------------------------------- раскладка продукта

fn find_unit(k: Kind, cfg: &Value) -> Option<kit::Unit> {
    if let Some(s) = cfg_str(cfg, "service") { return kit::unit(&s).or_else(|| Some(kit::Unit { name: s, ..Default::default() })); }
    k.units().iter().find_map(|n| kit::unit(n))
}

pub fn first_token(exec: &str) -> String { exec.split_whitespace().next().unwrap_or("").to_string() }

/// Юниты службы и её помощников (`<префикс>*.service|path|timer`, дополнения `.d`).
pub fn unit_files(prefixes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for d in ["/etc/systemd/system", "/etc/init.d"] {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if prefixes.iter().any(|p| n.starts_with(p)) { out.push(e.path().to_string_lossy().to_string()); }
            }
        }
    }
    out.sort();
    out
}

pub fn json_file(p: &Path) -> Value { std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null) }

fn under(child: &str, parent: &str) -> Option<String> {
    let c = Path::new(child);
    c.strip_prefix(parent).ok().map(|r| r.to_string_lossy().to_string()).filter(|r| !r.is_empty())
}

async fn version_of(bin: &str, args: &[&str]) -> String {
    if bin.is_empty() || !Path::new(bin).exists() { return String::new(); }
    let c = Cmd::new(bin).args(args.iter().copied());
    let log: LogFn = std::sync::Arc::new(|_: &str| {});
    match tokio::time::timeout(std::time::Duration::from_secs(10), kit::output(&c, &log)).await {
        Ok(Ok(s)) => s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(120).collect(),
        _ => String::new(),
    }
}

pub fn part(name: &str, title: &str, paths: Vec<String>, exclude: Vec<String>, phase: Phase, owner: &str) -> Part {
    Part { name: name.into(), title: title.into(), paths, exclude, phase, owner: owner.into(), xattrs: false, replace: phase == Phase::Quiesced }
}

async fn layout(k: Kind, cfg: &Value) -> Result<Layout> {
    let u = find_unit(k, cfg).unwrap_or_default();
    let env = |n: &str| u.var(n);
    let bool_cfg = |n: &str, d: bool| kit::bool_param(cfg, n, d);
    let user_ex = crate::connectors::net::p_list(cfg, "exclude");
    let mut l = Layout { service: if u.name.is_empty() { k.units()[0].to_string() } else { u.name.clone() }, ..Default::default() };
    let include_program = bool_cfg("include_program", true);
    let include_logs = bool_cfg("include_logs", k == Kind::Inventory || k == Kind::Dfs);
    match k {
        Kind::Mail => {
            l.user = if u.user.is_empty() { "hope".into() } else { u.user.clone() };
            let data = cfg_str(cfg, "data_dir").unwrap_or_else(|| u.resolve(&env("DB_DATA_DIR").unwrap_or_else(|| if u.working_dir.is_empty() { "/var/lib/hope/data".into() } else { "./hopedb_data".into() })));
            let st = json_file(&Path::new(&data).join("storage.json"));
            let primary = st["primary"].as_str().unwrap_or("");
            let ppath = st["primary_path"].as_str().unwrap_or("").trim().to_string();
            let blob = cfg_str(cfg, "blob_dir").unwrap_or_else(|| if matches!(primary, "local" | "smb") && !ppath.is_empty() { ppath.clone() } else { u.resolve(&env("BLOB_DATA_DIR").unwrap_or_else(|| if u.working_dir.is_empty() { "/var/lib/hope/blobs".into() } else { "./hopedb_blobs".into() })) });
            let backups = u.resolve(&env("HOPE_BACKUP_DIR").unwrap_or_else(|| "./hopedb_backups".into()));
            let mut ex = vec!["restore_pending.txt".to_string(), "*.part".into()];
            if !bool_cfg("include_backups", false) { if let Some(r) = under(&backups, &data) { ex.push(format!("{data}/{r}")); } }
            ex.extend(user_ex.clone());
            l.parts.push(part("data", "WAL, настройки, ключи (DB_DATA_DIR)", vec![data.clone()], ex.clone(), Phase::Quiesced, &l.user));
            // Тела писем пишутся до ссылок на них в WAL: копия после WAL содержит все нужные тела.
            let mut bp = part("blobs", "Тела писем (BLOB_DATA_DIR)", vec![blob.clone()], user_ex.clone(), Phase::Live, &l.user);
            bp.replace = false;
            l.parts.push(bp);
            l.bin = first_token(&u.exec);
            if l.bin.is_empty() { l.bin = "/usr/local/bin/hopedb-server".into(); }
            l.health = format!("https://127.0.0.1:{}/", env("HOPE_API_PORT").unwrap_or_else(|| "8443".into()));
            let mut cfgp = unit_files(&[&format!("{}.service", l.service)]);
            if Path::new("/etc/hope").exists() { cfgp.push("/etc/hope".into()); }
            if include_program { cfgp.push(l.bin.clone()); }
            l.parts.push(part("config", "Юнит, TLS (/etc/hope), программа", cfgp, vec![], Phase::Live, ""));
            l.version = version_of(&l.bin, &["--version"]).await;
            if primary == "s3" { l.notes.push("тела писем в S3 (storage.json) — добавьте бакет отдельным экземпляром «S3-хранилище»".into()); }
        }
        Kind::Gateway => {
            l.user = if u.user.is_empty() { "hopemsg".into() } else { u.user.clone() };
            let wd = if u.working_dir.is_empty() { "/var/lib/hope-gateway".to_string() } else { u.working_dir.clone() };
            let data = cfg_str(cfg, "data_dir").unwrap_or_else(|| format!("{wd}/msg_local_data"));
            let mut ex = vec![format!("{data}/updates"), "*.tmp".into()];
            if !bool_cfg("include_av", false) { ex.push(format!("{data}/av")); }
            ex.extend(user_ex.clone());
            l.parts.push(part("data", "Состояние, WAL, очередь, ящики, карантин (msg_local_data)", vec![data.clone()], ex, Phase::Quiesced, &l.user));
            let st = json_file(&Path::new(&data).join("storage.json"));
            if let Some(pp) = st["payload_path"].as_str().map(str::trim).filter(|p| p.starts_with('/') && under(p, &data).is_none()) {
                let mut pl = part("payload", "Хранилище вложений и карантина", vec![pp.to_string()], user_ex.clone(), Phase::Live, &l.user);
                pl.replace = false;
                l.parts.push(pl);
            }
            if bool_cfg("include_master_key", true) {
                let key = u.creds.iter().find(|(n, _)| n == "master.key").map(|x| x.1.clone()).unwrap_or_else(|| "/etc/hope-msg/master.key".into());
                if Path::new(&key).exists() { l.parts.push(part("secrets", "Мастер-ключ секретов", vec![key], vec![], Phase::Live, "")); }
                else { l.notes.push(format!("мастер-ключа {key} нет — секреты из копии не расшифровать без вашей копии ключа")); }
            }
            l.bin = { let b = first_token(&u.exec); if b.is_empty() { "/usr/local/bin/hope-msg-gateway".into() } else { b } };
            l.health = format!("https://127.0.0.1:{}/", env("MSG_API_PORT").unwrap_or_else(|| "6443".into()));
            let mut cfgp = unit_files(&["hope-msg-"]);
            if Path::new("/etc/keepalived/keepalived.conf").exists() { cfgp.push("/etc/keepalived/keepalived.conf".into()); }
            if include_program {
                cfgp.push(l.bin.clone());
                if let Ok(rd) = std::fs::read_dir("/usr/local/bin") { for e in rd.flatten() { let n = e.file_name().to_string_lossy().to_string(); if n.starts_with("hope-msg-") && n.ends_with("-apply") { cfgp.push(e.path().to_string_lossy().to_string()); } } }
            }
            l.parts.push(part("config", "Юниты, помощники, keepalived, программа", cfgp, vec![], Phase::Live, ""));
            l.version = version_of(&l.bin, &["--version"]).await;
        }
        Kind::Inventory => {
            l.user = if u.user.is_empty() { "hopeinv".into() } else { u.user.clone() };
            let data = cfg_str(cfg, "data_dir").unwrap_or_else(|| env("HOPEINV_DATA").unwrap_or_else(|| "/var/lib/hopeinv".into()));
            let mut ex = vec![format!("{data}/updates"), "*.tmp".into()];
            ex.extend(user_ex.clone());
            l.parts.push(part("data", "Настройки, база hopeinv.redb, сертификат", vec![data], ex, Phase::Quiesced, &l.user));
            if include_logs {
                let logd = env("HOPEINV_LOG").unwrap_or_else(|| "/var/log/hopeinv".into());
                let mut lp = part("logs", "Журнал аудита", vec![logd], vec!["hopeinv.log*".into()], Phase::Live, &l.user);
                lp.replace = false;
                l.parts.push(lp);
            }
            l.bin = { let b = first_token(&u.exec); if b.is_empty() { "/opt/hopeinv/hopeinv".into() } else { b } };
            l.health = format!("https://127.0.0.1:{}/", env("HOPEINV_ADMIN_PORT").unwrap_or_else(|| "5443".into()));
            let mut cfgp = unit_files(&["hopeinv"]);
            if include_program { if let Some(d) = Path::new(&l.bin).parent() { cfgp.push(d.to_string_lossy().to_string()); } }
            l.parts.push(part("config", "Юниты и программа (/opt/hopeinv)", cfgp, vec![], Phase::Live, ""));
            l.version = version_of(&l.bin, &["version"]).await;
        }
        Kind::Dfs => {
            l.user = "root".into();
            l.live_without_snapshot = true;
            let data = cfg_str(cfg, "data_dir").unwrap_or_else(|| env("HOPEDFS_DATA").unwrap_or_else(|| "/var/lib/hopedfs".into()));
            let mut ex = vec![format!("{data}/updates"), "initial_admin_password.txt".into(), "password_reset_link.txt".into(), "*.tmp".into()];
            ex.extend(user_ex.clone());
            l.parts.push(part("data", "Конфигурация узла и кластера, TLS, лицензия", vec![data.clone()], ex, Phase::Quiesced, ""));
            let want = crate::connectors::net::p_list(cfg, "shares");
            let st = json_file(&Path::new(&data).join("cluster_state.json"));
            let mut sx = vec![".hopedfs-*".to_string()];
            if !bool_cfg("include_previous_versions", false) { sx.push(".hopedfs/snapshots".into()); }
            if !bool_cfg("include_ransom_copies", false) { sx.push(".hopedfs/ransom".into()); }
            sx.extend(user_ex.clone());
            for s in st["shares"].as_array().cloned().unwrap_or_default() {
                let name = s["name"].as_str().unwrap_or("").to_string();
                let Some(path) = s["path"].as_str().filter(|p| p.starts_with('/')) else { continue };
                if !want.is_empty() && !want.iter().any(|w| w.eq_ignore_ascii_case(&name)) { continue; }
                let sname: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
                let mut p = part(&format!("share-{sname}"), &format!("Шара «{name}»"), vec![path.to_string()], sx.clone(), Phase::Quiesced, "");
                p.xattrs = true;
                p.replace = false;
                l.parts.push(p);
            }
            if l.parts.len() == 1 { l.notes.push("в cluster_state.json не найдено ни одной шары".into()); }
            if include_logs {
                let mut lp = part("logs", "Журнал аудита", vec![env("HOPEDFS_LOG").unwrap_or_else(|| "/var/log/hopedfs".into())], vec!["hopedfs.log*".into()], Phase::Live, "");
                lp.replace = false;
                l.parts.push(lp);
            }
            l.bin = { let b = first_token(&u.exec); if b.is_empty() { "/opt/hopedfs/hopedfs".into() } else { b } };
            l.health = format!("https://127.0.0.1:{}/api/health", env("HOPEDFS_ADMIN_PORT").unwrap_or_else(|| "7443".into()));
            let mut cfgp = unit_files(&["hopedfs"]);
            if include_program { if let Some(d) = Path::new(&l.bin).parent() { cfgp.push(d.to_string_lossy().to_string()); } }
            l.parts.push(part("config", "Юниты и программа (/opt/hopedfs)", cfgp, vec!["hopedfs.bak.*".into()], Phase::Live, ""));
            l.version = version_of(&l.bin, &["version"]).await;
            l.notes.push("лицензия привязана к машине: на другой машине нужен новый ключ".into());
            l.notes.push("после восстановления участника репликации площадок исключите его из группы и добавьте заново (иначе его новые изменения могут не разойтись)".into());
        }
        Kind::Dc => {
            l.user = if u.user.is_empty() { "hopedc".into() } else { u.user.clone() };
            let data = cfg_str(cfg, "data_dir").unwrap_or_else(|| u.resolve(&env("DC_DATA_DIR").unwrap_or_else(|| if u.working_dir.is_empty() { "/var/lib/hopedc".into() } else { "./hopedc_data".into() })));
            let mut ex = vec![format!("{data}/updates"), format!("{data}/backups"), "*.tmp".into()];
            if !include_logs { ex.push(format!("{data}/logs")); }
            ex.extend(user_ex.clone());
            let mut dp = part("data", "База домена (ewal), SYSVOL, кластер, PKI", vec![data.clone()], ex, Phase::Quiesced, &l.user);
            dp.xattrs = true; // метки целостности Astra (security.PDPL)
            l.parts.push(dp);
            let key_file = env("DC_MASTER_KEY_FILE").unwrap_or_else(|| "/etc/hopedc/master.key".into());
            if cfg_str(cfg, "key_mode").as_deref() == Some("file") {
                if Path::new(&key_file).exists() { let mut kp = part("secrets", "Мастер-ключ базы", vec![key_file.clone()], vec![], Phase::Live, &l.user); kp.xattrs = true; l.parts.push(kp); }
            }
            l.bin = { let b = first_token(&u.exec); if b.is_empty() { "/opt/hopedc/hopedc-core".into() } else { b } };
            l.health = format!("https://127.0.0.1:{}/", env("DC_UI_PORT").unwrap_or_else(|| "9443".into()));
            let mut cfgp = unit_files(&["hopedc"]);
            if Path::new("/usr/local/bin/hopedc-update-apply").exists() { cfgp.push("/usr/local/bin/hopedc-update-apply".into()); }
            if include_program { cfgp.push(l.bin.clone()); }
            let mut cp = part("config", "Юниты, программа", cfgp, vec![], Phase::Live, "");
            cp.xattrs = true;
            l.parts.push(cp);
            if include_logs {
                let logd = env("DC_LOG_DIR").unwrap_or_else(|| "/var/log/hopedc".into());
                if under(&logd, &data).is_none() { let mut lp = part("logs", "Журналы", vec![logd], vec![], Phase::Live, &l.user); lp.replace = false; l.parts.push(lp); }
            }
            l.version = version_of(&l.bin, &["--version"]).await;
            l.notes.push("кластер: восстановленный узел — неавторитетная копия; остальные узлы пересоберите (режим rejoin)".into());
            l.notes.push("лицензия привязана к оборудованию; IP и имена контроллера сохраните прежними".into());
        }
    }
    // Части без путей на диске не копируются (необязательные).
    for p in l.parts.iter_mut() { p.paths.retain(|x| !x.is_empty()); }
    l.parts.retain(|p| !p.paths.is_empty());
    Ok(l)
}

// ---------------------------------------------------------------- плагин

#[async_trait]
impl Plugin for HopeApp {
    fn id(&self) -> &'static str { self.0.id() }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let k = self.0;
        let l = layout(k, cfg).await?;
        let unit = find_unit(k, cfg);
        let size = |p: &str| -> u64 { fn du(p: &Path) -> u64 { match std::fs::symlink_metadata(p) { Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|r| r.flatten().map(|e| du(&e.path())).sum()).unwrap_or(0), Ok(m) => m.len(), Err(_) => 0 } } du(Path::new(p)) };
        Ok(json!({
            "product": k.title(),
            "service": l.service,
            "unit_found": unit.as_ref().map(|u| !u.path.is_empty()).unwrap_or(false),
            "active": l.svc_active(),
            "user": l.user,
            "version": l.version,
            "parts": l.parts.iter().map(|p| json!({ "name": p.name, "title": p.title, "paths": p.paths.iter().map(|x| json!({ "path": x, "exists": Path::new(x).exists(), "bytes": size(x) })).collect::<Vec<_>>(), "phase": p.phase })).collect::<Vec<_>>(),
            "health": l.health,
            "notes": l.notes,
        }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let k = self.0;
        let log = &ctx.log;
        let l = layout(k, &ctx.config).await?;
        if !l.parts.iter().any(|p| p.name == "data" && p.paths.iter().any(|x| Path::new(x).exists())) {
            bail!("{}: каталог данных не найден ({}) — укажите «Каталог данных» или службу", k.title(), l.parts.first().map(|p| p.paths.join(", ")).unwrap_or_default());
        }
        log(&format!("{} {} · служба {} ({})", k.title(), l.version, l.service, if l.svc_active() { "работает" } else { "не работает" }));
        for n in &l.notes { log(&format!("  ! {n}")); }
        let mut w = ctx.repo.writer();
        let how = backup_parts(ctx, &l, &mut w).await?;
        // HopeDC: мастер-ключ — экспортом с паролем (отдельно от базы в смысле формы: без пароля не прочитать).
        let mut key = "нет";
        if k == Kind::Dc {
            let mode = cfg_str(&ctx.config, "key_mode").unwrap_or_else(|| "export".into());
            if mode == "export" {
                match cfg_str(&ctx.config, "key_password") {
                    Some(pw) => {
                        let tmp = kit::SecretFile::new("", None)?;
                        let _ = std::fs::remove_file(&tmp.0);
                        let data = l.parts.iter().find(|p| p.name == "data").map(|p| p.paths[0].clone()).unwrap_or_default();
                        let c = Cmd::new(&l.bin).args(["key", "export", "--out", &tmp.path(), "--data-dir", &data, "--note", "HopeBackup"]).env("HOPEDC_KEY_PASSWORD", pw);
                        kit::output(&c, log).await.context("экспорт мастер-ключа HopeDC")?;
                        let b = std::fs::read(&tmp.0).context("файл экспорта ключа")?;
                        kit::add_bytes(&mut w, "master-key.hdckey", b).await?;
                        key = "экспорт с паролем";
                    }
                    None => log("  ! пароль экспорта ключа не задан — ключ в копию не попал: храните его копию сами (без него база не читается)"),
                }
            } else if mode == "file" { key = "файл ключа"; }
        }
        let meta = json!({ "product": k.title(), "layout": l, "consistency": how, "key": key });
        log(&format!("согласованность: {how}"));
        let (header, stats) = w.finish(&ctx.instance, k.id(), "full", json!({ "version": l.version, "consistency": how }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let k = self.0;
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let l: Layout = serde_json::from_value(m.meta["layout"].clone()).context("в копии нет описания раскладки продукта")?;
        let dc_mode = cfg_str(&ctx.params, "mode").unwrap_or_else(|| "full".into());
        let rejoin = k == Kind::Dc && dc_mode == "rejoin";
        // Пароль ключа проверяется до того, как что-либо на машине изменится.
        if k == Kind::Dc && kit::stream(&m, "master-key.hdckey").is_ok() && cfg_str(&ctx.params, "key_password").is_none() {
            bail!("в копии ключ HopeDC экспортирован с паролем — укажите «Пароль ключа из копии»");
        }
        let (root, in_place, moved, parts) = restore_parts(ctx, &l, &m, |p| !rejoin || p.name == "data" || p.name == "secrets", |p| {
            (rejoin && p.name == "data").then(|| vec![format!("{}/cluster_config.json", p.paths[0].trim_start_matches('/'))])
        }).await?;
        // HopeDC: ключ из экспорта.
        if k == Kind::Dc {
            if let Ok(s) = kit::stream(&m, "master-key.hdckey") {
                let pw = cfg_str(&ctx.params, "key_password").ok_or_else(|| anyhow!("в копии ключ HopeDC экспортирован с паролем — укажите «Пароль ключа из копии»"))?;
                let tmp = kit::SecretFile::new("", None)?;
                kit::to_file(&ctx.repo, s, &tmp.0).await?;
                let data = l.parts.iter().find(|p| p.name == "data").map(|p| p.paths[0].clone()).unwrap_or_default();
                let data = if in_place { data } else { format!("{}/{}", root.trim_end_matches('/'), data.trim_start_matches('/')) };
                if in_place && Path::new(&l.bin).exists() {
                    kit::run(&Cmd::new(&l.bin).args(["key", "import", &tmp.path(), "--data-dir", &data, "--force"]).env("HOPEDC_KEY_PASSWORD", pw), log).await.context("импорт мастер-ключа HopeDC")?;
                    let _ = kit::run(&Cmd::new(&l.bin).args(["key", "info", "--data-dir", &data]), log).await;
                } else {
                    let out = Path::new(&root).join("master-key.hdckey");
                    std::fs::copy(&tmp.0, &out)?;
                    log(&format!("  файл ключа: {} (импорт: hopedc-core key import … --data-dir {data})", out.display()));
                }
            } else if !l.parts.iter().any(|p| p.name == "secrets") {
                log("  ! ключа в копии нет: верните /etc/hopedc/master.key из своей копии (hopedc-core key import), иначе база не читается");
            }
        }
        let healthy = finish_restore(ctx, &l, in_place).await?;
        Ok(json!({ "root": root, "parts": parts, "moved_aside": moved, "healthy": healthy, "notes": l.notes }))
    }
}

/// Скопировать части раскладки: одним снимком ФС, иначе — при остановленной службе (части Quiesced),
/// затем остальное на ходу. Возвращает, как достигнута согласованность.
pub async fn backup_parts(ctx: &BackupCtx, l: &Layout, w: &mut crate::store::repo::SnapshotWriter) -> Result<&'static str> {
    let log = &ctx.log;
    let mode = kit::consistency(&ctx.config);
    let quiesced: Vec<&Part> = l.parts.iter().filter(|p| p.phase == Phase::Quiesced).collect();
    let live: Vec<&Part> = l.parts.iter().filter(|p| p.phase == Phase::Live).collect();
    let ex = |p: &Part| Excludes::new(&p.exclude, None);
    let mut how = "на ходу";
    // Один снимок ФС на все части — между собой они согласованы.
    let mut done_live = false;
    if matches!(mode, Consistency::Auto | Consistency::Snapshot) {
        let all: Vec<String> = l.parts.iter().filter(|p| p.name != "config").flat_map(|p| p.paths.iter().filter(|x| Path::new(x).exists()).cloned()).collect();
        let mut c2 = ctx.config.clone();
        if let Some(o) = c2.as_object_mut() { o.insert("snapshot".into(), json!("required")); }
        match super::files::snapshot_for(&all, &c2, &ctx.job_tag(), log.clone()).await {
            Ok(snaps) => {
                for p in quiesced.iter().chain(live.iter()).filter(|p| p.name != "config") { kit::tar_paths_x(w, &format!("{}.tar", p.name), &p.paths, ex(p), snaps.remap.clone(), p.xattrs, log).await?; }
                snaps.check()?;
                drop(snaps);
                how = "снимок ФС";
                done_live = true;
            }
            Err(e) if mode == Consistency::Snapshot => return Err(e),
            Err(e) => log(&format!("снимок ФС невозможен: {e:#}")),
        }
    }
    if !done_live {
        let stop = match mode { Consistency::Stop => true, Consistency::Auto => !l.live_without_snapshot, _ => false };
        if stop && !quiesced.is_empty() && l.svc_exists() {
            let w2 = &mut *w;
            l.while_stopped(log, || async move {
                for p in &quiesced { kit::tar_paths_x(w2, &format!("{}.tar", p.name), &p.paths, ex(p), Default::default(), p.xattrs, log).await?; }
                Ok(())
            }).await?;
            how = "служба остановлена";
        } else {
            if l.live_without_snapshot && mode == Consistency::Auto { log("снимка ФС нет — копия на ходу (файлы, изменяемые во время копии, могут оказаться в разном состоянии)"); }
            for p in &quiesced { kit::tar_paths_x(w, &format!("{}.tar", p.name), &p.paths, ex(p), Default::default(), p.xattrs, log).await?; }
        }
        for p in live.iter().filter(|p| p.name != "config") { kit::tar_paths_x(w, &format!("{}.tar", p.name), &p.paths, ex(p), Default::default(), p.xattrs, log).await?; }
    }
    if let Some(p) = l.parts.iter().find(|p| p.name == "config") {
        kit::tar_paths_x(w, "config.tar", &p.paths, ex(p), Default::default(), p.xattrs, log).await?;
    }
    Ok(how)
}

/// Восстановить части раскладки (служба на месте останавливается, но не запускается — это
/// `finish_restore`). `keep` — какие части брать, `only` — ограничить часть отдельными путями.
/// Возвращает корень, «на место», что отложено в сторону и какие части восстановлены.
pub async fn restore_parts(ctx: &RestoreCtx, l: &Layout, m: &crate::store::types::Manifest, keep: impl Fn(&Part) -> bool, only: impl Fn(&Part) -> Option<Vec<String>>) -> Result<(String, bool, Vec<String>, Vec<String>)> {
    let log = &ctx.log;
    let root = cfg_str(&ctx.params, "target_root").or_else(|| cfg_str(&ctx.params, "root")).unwrap_or_else(|| "/".into());
    let in_place = root == "/";
    let want = crate::connectors::net::p_list(&ctx.params, "parts");
    let parts: Vec<&Part> = l.parts.iter().filter(|p| (want.is_empty() || want.iter().any(|w| w == &p.name)) && keep(p)).collect();
    if parts.is_empty() { bail!("нечего восстанавливать: в копии части {}", l.parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")); }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut moved = Vec::new();
    if in_place && l.svc_exists() && l.svc_active() { l.svc_stop(log).await?; }
    for p in &parts {
        let s = match kit::stream(m, &format!("{}.tar", p.name)) { Ok(s) => s, Err(_) => { log(&format!("  часть {} отсутствует в копии", p.name)); continue } };
        // Данные заменяются целиком: лишние файлы (WAL новее копии) смешались бы с восстановленными.
        if in_place && p.replace {
            for x in &p.paths {
                let path = kit::safe_abs(x)?;
                if Path::new(&path).exists() {
                    let aside = kit::aside_name(&path, &ts);
                    std::fs::rename(&path, &aside).with_context(|| format!("перенос {path} → {aside}"))?;
                    log(&format!("  прежнее содержимое {path} → {aside}"));
                    moved.push(aside);
                }
            }
        }
        let mut opt = Untar { owner: (!p.owner.is_empty()).then(|| p.owner.clone()), ..Default::default() };
        if let Some(o) = only(p) {
            for x in &p.paths { std::fs::create_dir_all(Path::new(&root).join(x.trim_start_matches('/')))?; }
            opt.only = o;
        }
        kit::untar(&ctx.repo, s, Path::new(&root), opt, log).await?;
        if in_place && !p.owner.is_empty() && p.owner != "root" {
            if crate::util::user_ids(&p.owner).is_some() { kit::chown_to(&p.paths, &p.owner, log)?; }
            else { log(&format!("  ! нет пользователя {} — владелец оставлен как в копии (установите продукт и повторите)", p.owner)); }
        }
    }
    Ok((root, in_place, moved, parts.iter().map(|p| p.name.clone()).collect()))
}

/// Запустить службу после восстановления на место и дождаться ответа по HTTP.
pub async fn finish_restore(ctx: &RestoreCtx, l: &Layout, in_place: bool) -> Result<Value> {
    let log = &ctx.log;
    let mut healthy = Value::Null;
    if in_place && kit::bool_param(&ctx.params, "start", true) && l.svc_exists() {
        l.svc_start(log).await?;
        let ok = l.health.is_empty() || kit::wait_http(&l.health, 60).await;
        log(if ok { "служба запущена и отвечает" } else { "! служба запущена, но не отвечает по HTTP — проверьте журнал" });
        healthy = json!(ok);
    }
    for n in &l.notes { log(&format!("  ! {n}")); }
    Ok(healthy)
}

/// Копия приложения по раскладке: части, затем `extra` — дополнительные потоки (экспорт, дамп БД).
pub async fn layout_backup(ctx: &BackupCtx, product: &str, title: &str, l: &Layout, extra: Value) -> Result<BackupOutcome> {
    let log = &ctx.log;
    log(&format!("{title} {} · {} ({})", l.version, if l.container.is_empty() { format!("служба {}", l.service) } else { format!("контейнер {}", l.container) }, if l.svc_active() { "работает" } else { "не работает" }));
    for n in &l.notes { log(&format!("  ! {n}")); }
    let mut w = ctx.repo.writer();
    let how = backup_parts(ctx, l, &mut w).await?;
    log(&format!("согласованность: {how}"));
    let meta = json!({ "product": title, "layout": l, "consistency": how, "extra": extra });
    let (header, stats) = w.finish(&ctx.instance, product, "full", json!({ "version": l.version, "consistency": how }), meta).await?;
    Ok(BackupOutcome { header, stats })
}

/// Восстановление по раскладке из копии (на место — с остановкой и запуском службы).
pub async fn layout_restore(ctx: &RestoreCtx) -> Result<Value> {
    let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
    let l: Layout = serde_json::from_value(m.meta["layout"].clone()).context("в копии нет описания раскладки")?;
    let (root, in_place, moved, parts) = restore_parts(ctx, &l, &m, |_| true, |_| None).await?;
    let healthy = finish_restore(ctx, &l, in_place).await?;
    Ok(json!({ "root": root, "parts": parts, "moved_aside": moved, "healthy": healthy, "notes": l.notes }))
}

/// Общие параметры восстановления приложений по раскладке.
pub fn layout_restore_schema(parts_hint: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("parts".into(), json!({ "type": "array", "items": { "type": "string" }, "title": "Что восстановить", "description": format!("Части копии ({parts_hint}); пусто — всё") }));
    m.insert("target_root".into(), json!({ "type": "string", "title": "Корень восстановления", "default": "/", "description": "«/» — на место (служба останавливается и запускается); другой каталог — только распаковать" }));
    m.insert("start".into(), json!({ "type": "boolean", "title": "Запустить службу после восстановления", "default": true }));
    m
}
