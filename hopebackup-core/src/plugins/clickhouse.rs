//! ClickHouse: два способа.
//!  • backup — штатный BACKUP DATABASE … TO Disk('backups', …) (согласованно по таблицам, все движки),
//!    затем каталог копии с диска — tar-потоком в репозиторий и удаляется; восстановление — RESTORE.
//!    Нужен диск для копий в конфигурации сервера (<backups><allowed_disk>backups</allowed_disk>).
//!  • native — без настройки сервера: схема (SHOW CREATE) и данные таблиц в формате Native потоком
//!    SELECT … FORMAT Native; восстановление — CREATE и INSERT … FORMAT Native.
//! Пароль — в файле параметров клиента (--config-file), не в командной строке.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd, Secret};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct ClickHouse;

const SYSTEM_DBS: &[&str] = &["system", "INFORMATION_SCHEMA", "information_schema", "_temporary_and_external_tables"];

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "host": { "type": "string", "title": "Адрес", "default": "127.0.0.1" },
        "port": { "type": "integer", "title": "Порт (native)", "default": 9000 },
        "user": { "type": "string", "title": "Пользователь", "default": "default", "description": "Права: SHOW, SELECT, BACKUP (для восстановления — CREATE, INSERT, RESTORE)" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "secure": { "type": "boolean", "title": "TLS (порт 9440)", "default": false },
        "container": { "type": "string", "title": "Контейнер Docker" },
        "method": { "type": "string", "title": "Способ", "enum": ["auto", "backup", "native"], "default": "auto",
            "description": "backup — BACKUP на диск копий сервера (все движки, согласованно по таблице); native — схема и данные SELECT … FORMAT Native (без настройки сервера); auto — backup, если диск копий настроен и доступен с этой машины" },
        "backup_disk": { "type": "string", "title": "Диск для копий", "default": "backups", "description": "Имя диска из storage_configuration, разрешённого в <backups><allowed_disk>" },
        "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы", "description": "Пусто — все, кроме системных" }
    } })
}

pub fn defaults() -> Value { json!({ "host": "127.0.0.1", "port": 9000, "user": "default", "method": "auto", "backup_disk": "backups" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы", "description": "Пусто — все базы копии" },
        "rename_to": { "type": "string", "title": "Под именем", "description": "Для одной базы — восстановить рядом под этим именем" },
        "replace": { "type": "boolean", "title": "Существующую базу — переименовать в <имя>_before_restore_<время>", "default": true, "description": "Выключено — восстановление в существующую базу остановится" },
        "host": { "type": "string", "title": "Целевой сервер" },
        "port": { "type": "integer", "title": "Порт" },
        "user": { "type": "string", "title": "Пользователь" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["host", "port", "user", "password", "secure", "container", "backup_disk"] }

struct Conn { host: String, port: u16, user: String, password: String, secure: bool, container: Option<String> }

impl Conn {
    fn from(v: &Value) -> Conn {
        Conn {
            host: cfg_str(v, "host").unwrap_or_else(|| "127.0.0.1".into()),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(9000) as u16,
            user: cfg_str(v, "user").unwrap_or_else(|| "default".into()),
            password: cfg_str(v, "password").unwrap_or_default(),
            secure: kit::bool_param(v, "secure", false),
            container: cfg_str(v, "container"),
        }
    }
    async fn client(&self, log: &LogFn) -> Result<(Cmd, Secret)> {
        let p = kit::find_prog(self.container.as_deref(), &["clickhouse-client", "clickhouse"]).await.ok_or_else(|| anyhow!("нет clickhouse-client"))?;
        let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let conf = format!("<config><host>{}</host><port>{}</port><user>{}</user><password>{}</password><secure>{}</secure></config>\n", x(&self.host), self.port, x(&self.user), x(&self.password), if self.secure { 1 } else { 0 });
        let sec = Secret::with_ext(self.container.as_deref(), &conf, ".xml", log).await?;
        let mut c = Cmd::new(&p).container(self.container.clone());
        if p.ends_with("/clickhouse") || p == "clickhouse" { c = c.arg("client"); }
        Ok((c.arg(format!("--config-file={}", sec.path())), sec))
    }
    async fn q(&self, sql: &str, log: &LogFn) -> Result<Vec<Vec<String>>> {
        let (c, _s) = self.client(log).await?;
        let out = kit::output(&c.args(["--format", "TSVRaw", "--query", sql]), log).await?;
        Ok(out.lines().filter(|l| !l.is_empty()).map(|l| l.split('\t').map(|x| x.to_string()).collect()).collect())
    }
    /// Ответ одним текстом (DDL с переводами строк).
    async fn raw(&self, sql: &str, log: &LogFn) -> Result<String> {
        let (c, _s) = self.client(log).await?;
        Ok(kit::output(&c.args(["--format", "TSVRaw", "--query", sql]), log).await?.trim_end().to_string())
    }
    async fn exec(&self, sql: &str, log: &LogFn) -> Result<String> {
        log(&format!("  SQL: {}", sql.chars().take(240).collect::<String>()));
        let (c, _s) = self.client(log).await?;
        kit::output(&c.args(["--multiquery", "--query", sql]), log).await
    }
}

fn qi(n: &str) -> String { format!("`{}`", n.replace('\\', "\\\\").replace('`', "\\`")) }
fn qs(s: &str) -> String { format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")) }

/// Движки, данные которых копируются способом native (у остальных — только схема).
fn has_data(engine: &str) -> bool {
    engine.contains("MergeTree") || ["Log", "TinyLog", "StripeLog", "Memory", "Set", "Join"].contains(&engine)
}

/// Порядок создания: таблицы, затем словари, затем представления (зависят от таблиц).
fn order(engine: &str) -> u8 { match engine { "View" | "MaterializedView" | "LiveView" | "WindowView" => 2, "Dictionary" => 1, _ => 0 } }

#[async_trait]
impl Plugin for ClickHouse {
    fn id(&self) -> &'static str { "clickhouse" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let c = Conn::from(cfg);
        let v = c.q("SELECT version()", log).await?;
        let dbs = c.q("SELECT database, count(), sum(total_bytes) FROM system.tables WHERE database NOT IN ('system','INFORMATION_SCHEMA','information_schema') GROUP BY database", log).await.unwrap_or_default();
        let disk = c.q(&format!("SELECT path FROM system.disks WHERE name = {}", qs(&cfg_str(cfg, "backup_disk").unwrap_or_else(|| "backups".into()))), log).await.unwrap_or_default();
        Ok(json!({ "version": v.first().and_then(|r| r.first()), "backup_disk_path": disk.first().and_then(|r| r.first()),
            "databases": dbs.iter().map(|r| json!({ "name": r[0], "tables": r.get(1), "bytes": r.get(2) })).collect::<Vec<_>>() }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let c = Conn::from(&ctx.config);
        let ver = c.q("SELECT version()", log).await.context("подключение к ClickHouse")?.first().and_then(|r| r.first().cloned()).unwrap_or_default();
        let mut dbs = crate::connectors::net::p_list(&ctx.config, "databases");
        if dbs.is_empty() { dbs = c.q("SELECT name FROM system.databases", log).await?.into_iter().filter_map(|r| r.into_iter().next()).filter(|n| !SYSTEM_DBS.contains(&n.as_str())).collect(); }
        if dbs.is_empty() { bail!("нет пользовательских баз"); }
        let disk = cfg_str(&ctx.config, "backup_disk").unwrap_or_else(|| "backups".into());
        let disk_path = c.q(&format!("SELECT path FROM system.disks WHERE name = {}", qs(&disk)), log).await.unwrap_or_default().first().and_then(|r| r.first().cloned());
        let mut method = cfg_str(&ctx.config, "method").unwrap_or_else(|| "auto".into());
        if method == "auto" {
            method = if disk_path.as_ref().map(|p| c.container.is_some() || Path::new(p).is_dir()).unwrap_or(false) { "backup".into() } else { "native".into() };
        }
        log(&format!("ClickHouse {ver} · базы {} · способ {method}", dbs.join(", ")));
        let mut w = ctx.repo.writer();
        let mut tables_meta = Vec::new();
        if method == "backup" {
            let dp = disk_path.ok_or_else(|| anyhow!("нет диска «{disk}» для копий: добавьте его в storage_configuration и <backups><allowed_disk>{disk}</allowed_disk></backups> или выберите способ native"))?;
            let name = format!("hopebackup-{}", crate::util::short_id());
            let what = dbs.iter().map(|d| format!("DATABASE {}", qi(d))).collect::<Vec<_>>().join(", ");
            let r = async {
                c.exec(&format!("BACKUP {what} TO Disk({}, {})", qs(&disk), qs(&format!("{name}/"))), log).await.context("BACKUP")?;
                let dir = format!("{}/{name}", dp.trim_end_matches('/'));
                match &c.container {
                    Some(ct) => { kit::stream_cmd(&mut w, "backup.tar", &Cmd::new("tar").args(["-C", &dir, "-cf", "-", "."]).container(Some(ct.clone())), log).await?; }
                    None => { kit::tar_paths(&mut w, "backup.tar", &[dir.clone()], Default::default(), Default::default(), log).await?; }
                }
                Ok::<String, anyhow::Error>(dir)
            }.await;
            // Каталог копии на диске сервера — удалить в любом случае.
            let dir = format!("{}/{name}", dp.trim_end_matches('/'));
            match &c.container { Some(ct) => { let _ = kit::output(&Cmd::new("rm").args(["-rf", &dir]).container(Some(ct.clone())), log).await; } None => { let _ = std::fs::remove_dir_all(&dir); } }
            r?;
            tables_meta.push(json!({ "backup_dir": dir }));
        } else {
            for db in &dbs {
                let cdb = c.raw(&format!("SHOW CREATE DATABASE {}", qi(db)), log).await?;
                let mut tabs = c.q(&format!("SELECT name, engine FROM system.tables WHERE database = {} AND NOT is_temporary AND name NOT LIKE '.inner%'", qs(db)), log).await?;
                tabs.sort_by_key(|r| order(r.get(1).map(|s| s.as_str()).unwrap_or("")));
                let mut schema = format!("{};\n", cdb.trim().trim_end_matches(';'));
                let mut list = Vec::new();
                for t in &tabs {
                    let (name, engine) = (t[0].clone(), t.get(1).cloned().unwrap_or_default());
                    let ddl = c.raw(&format!("SHOW CREATE TABLE {}.{}", qi(db), qi(&name)), log).await?;
                    schema.push_str(&format!("{};\n", ddl.trim().trim_end_matches(';')));
                    if has_data(&engine) {
                        let (cl, _s) = c.client(log).await?;
                        let cmd = cl.args(["--query", &format!("SELECT * FROM {}.{} FORMAT Native", qi(db), qi(&name))]);
                        kit::stream_cmd(&mut w, &format!("data/{db}/{name}.native"), &cmd, log).await?;
                    }
                    if engine == "MaterializedView" && !ddl.contains(" TO ") { log(&format!("  ! {db}.{name}: материализованное представление без TO — его данные не копируются (пересчитаются только новые вставки)")); }
                    list.push(json!({ "name": name, "engine": engine, "data": has_data(&engine) }));
                }
                kit::add_bytes(&mut w, &format!("schema/{db}.sql"), schema.into_bytes()).await?;
                tables_meta.push(json!({ "database": db, "tables": list }));
            }
        }
        let meta = json!({ "version": ver, "method": method, "databases": dbs, "detail": tables_meta, "disk": disk });
        let (header, stats) = w.finish(&ctx.instance, "clickhouse", "full", json!({ "version": ver, "method": method }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let c = Conn::from(&ctx.params);
        let all: Vec<String> = m.meta["databases"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        let mut dbs = crate::connectors::net::p_list(&ctx.params, "databases");
        if dbs.is_empty() { dbs = all.clone(); }
        for d in &dbs { if !all.contains(d) { bail!("базы {d} нет в копии ({})", all.join(", ")); } }
        let rename = cfg_str(&ctx.params, "rename_to");
        if rename.is_some() && dbs.len() != 1 { bail!("«Под именем» — для одной базы"); }
        let replace = kit::bool_param(&ctx.params, "replace", true);
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let mut moved = Vec::new();
        for db in &dbs {
            let target = rename.clone().unwrap_or_else(|| db.clone());
            let exists = !c.q(&format!("SELECT name FROM system.databases WHERE name = {}", qs(&target)), log).await?.is_empty();
            if exists {
                if !replace { bail!("база {target} уже есть — включите замену или восстановите под другим именем"); }
                let aside = format!("{target}_before_restore_{ts}");
                c.exec(&format!("RENAME DATABASE {} TO {}", qi(&target), qi(&aside)), log).await.with_context(|| format!("перенос {target} → {aside} (движок базы Atomic)"))?;
                log(&format!("прежняя база {target} → {aside}"));
                moved.push(aside);
            }
        }
        if m.meta["method"] == "backup" {
            let disk = cfg_str(&ctx.params, "backup_disk").or_else(|| m.meta["disk"].as_str().map(|s| s.to_string())).unwrap_or_else(|| "backups".into());
            let dp = c.q(&format!("SELECT path FROM system.disks WHERE name = {}", qs(&disk)), log).await?.first().and_then(|r| r.first().cloned()).ok_or_else(|| anyhow!("на целевом сервере нет диска «{disk}» для копий"))?;
            let name = format!("hopebackup-restore-{}", crate::util::short_id());
            let dir = format!("{}/{name}", dp.trim_end_matches('/'));
            let s = kit::stream(&m, "backup.tar")?;
            let orig = m.meta["detail"][0]["backup_dir"].as_str().unwrap_or("").trim_start_matches('/').to_string();
            match &c.container {
                Some(ct) => {
                    kit::output(&Cmd::new("mkdir").args(["-p", &dir]).container(Some(ct.clone())), log).await?;
                    let strip = orig.split('/').filter(|x| !x.is_empty()).count().to_string();
                    kit::feed(&ctx.repo, s, &Cmd::new("tar").args(["-C", &dir, "-xf", "-", "--strip-components", &strip]).container(Some(ct.clone())), log).await?;
                }
                None => {
                    kit::untar(&ctx.repo, s, Path::new("/"), kit::Untar { relocate: vec![(orig.clone(), dir.trim_start_matches('/').to_string())], owner: crate::util::user_ids("clickhouse").map(|_| "clickhouse".to_string()), ..Default::default() }, log).await?;
                    if crate::util::user_ids("clickhouse").is_some() { kit::chown_to(&[dir.clone()], "clickhouse", log)?; }
                }
            }
            let what = dbs.iter().map(|d| match &rename { Some(r) => format!("DATABASE {} AS {}", qi(d), qi(r)), None => format!("DATABASE {}", qi(d)) }).collect::<Vec<_>>().join(", ");
            let r = c.exec(&format!("RESTORE {what} FROM Disk({}, {})", qs(&disk), qs(&format!("{name}/"))), log).await;
            match &c.container { Some(ct) => { let _ = kit::output(&Cmd::new("rm").args(["-rf", &dir]).container(Some(ct.clone())), log).await; } None => { let _ = std::fs::remove_dir_all(&dir); } }
            r.context("RESTORE")?;
        } else {
            for db in &dbs {
                let target = rename.clone().unwrap_or_else(|| db.clone());
                let schema = String::from_utf8(kit::read_small(&ctx.repo, kit::stream(&m, &format!("schema/{db}.sql"))?).await?)?;
                let schema = if target != *db { rename_schema(&schema, db, &target) } else { schema };
                for stmt in split_statements(&schema) { c.exec(&stmt, log).await.with_context(|| format!("схема {target}"))?; }
                let detail = m.meta["detail"].as_array().and_then(|a| a.iter().find(|d| d["database"] == db.as_str())).cloned().unwrap_or_default();
                for t in detail["tables"].as_array().into_iter().flatten().filter(|t| t["data"] == true) {
                    let name = t["name"].as_str().unwrap_or("");
                    let s = kit::stream(&m, &format!("data/{db}/{name}.native"))?;
                    let (cl, _s) = c.client(log).await?;
                    kit::feed(&ctx.repo, s, &cl.args(["--query", &format!("INSERT INTO {}.{} FORMAT Native", qi(&target), qi(name))]), log).await.with_context(|| format!("данные {target}.{name}"))?;
                }
                log(&format!("база {target} восстановлена"));
            }
        }
        Ok(json!({ "databases": dbs, "renamed_to": rename, "moved_aside": moved, "method": m.meta["method"] }))
    }
}

/// Схема под новым именем базы: CREATE DATABASE и полные имена `db`.`t` / db.t в CREATE.
fn rename_schema(s: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    for line in s.split_inclusive('\n') {
        let l = line.replace(&format!("{}.", qi(from)), &format!("{}.", qi(to))).replace(&format!(" {from}."), &format!(" {to}."));
        let l = if l.starts_with("CREATE DATABASE") { l.replacen(&qi(from), &qi(to), 1).replacen(&format!("CREATE DATABASE {from}"), &format!("CREATE DATABASE {to}"), 1) } else { l };
        out.push_str(&l);
    }
    out
}

/// Операторы схемы (разделитель — «;» в конце строки).
fn split_statements(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        cur.push_str(line);
        cur.push('\n');
        if line.trim_end().ends_with(';') { let t = cur.trim().trim_end_matches(';').to_string(); if !t.is_empty() { out.push(t); } cur.clear(); }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_rename() {
        let s = "CREATE DATABASE shop\nENGINE = Atomic;\nCREATE TABLE shop.orders\n(\n    `id` UInt64\n)\nENGINE = MergeTree\nORDER BY id;\n";
        let r = rename_schema(s, "shop", "shop_copy");
        assert!(r.contains("CREATE DATABASE shop_copy") && r.contains("CREATE TABLE shop_copy.orders"));
        assert_eq!(split_statements(&r).len(), 2);
        assert!(has_data("ReplicatedMergeTree") && !has_data("Distributed"));
    }
}
