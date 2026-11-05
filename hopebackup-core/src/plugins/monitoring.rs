//! Мониторинг: Zabbix (база MySQL/MariaDB или PostgreSQL — дамп без остановки, по желанию без данных
//! history*/trends*; конфигурация сервера, веб-интерфейса, скрипты оповещений) и Grafana (grafana.db —
//! согласованной копией SQLite .backup или дамп внешней базы; каталог данных, плагины, provisioning).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, svc, Cmd, SecretFile, WorkDir};
use super::mysql::Conn as MyConn;
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Monitoring;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "product": { "type": "string", "title": "Продукт", "enum": ["zabbix", "grafana"], "default": "zabbix" },
        "config_file": { "type": "string", "title": "Файл конфигурации", "description": "Пусто — /etc/zabbix/zabbix_server.conf или /etc/grafana/grafana.ini" },
        "db_type": { "type": "string", "title": "Zabbix: СУБД", "enum": ["auto", "mysql", "postgresql"], "default": "auto" },
        "db_password": { "type": "string", "format": "secret", "title": "Пароль базы", "description": "Пусто — из файла конфигурации (DBPassword / [database] password)" },
        "history": { "type": "boolean", "title": "Zabbix: данные history* и trends*", "default": true, "description": "Выключено — только структура этих таблиц (копия в разы меньше, история метрик теряется)" },
        "service": { "type": "string", "title": "Служба", "description": "Пусто — zabbix-server или grafana-server" }
    } })
}

pub fn defaults() -> Value { json!({ "product": "zabbix", "db_type": "auto", "history": true }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "restore_database": { "type": "boolean", "title": "Восстановить базу", "default": true },
        "database": { "type": "string", "title": "В базу", "description": "Пусто — как в копии; другое имя — рядом с рабочей" },
        "restore_config": { "type": "boolean", "title": "Восстановить конфигурацию", "default": true },
        "target_dir": { "type": "string", "title": "Только файлы в каталог", "description": "Сервер не трогать" },
        "start": { "type": "boolean", "title": "Запустить службу после восстановления", "default": true }
    } })
}

/// Параметры «Ключ=значение» (zabbix_server.conf).
fn kv(text: &str, k: &str) -> Option<String> {
    text.lines().map(str::trim).filter(|l| !l.starts_with('#')).find_map(|l| l.strip_prefix(&format!("{k}=")).map(|v| v.trim().to_string())).filter(|v| !v.is_empty())
}

/// Значение из INI (секция, ключ).
fn ini(text: &str, sec: &str, k: &str) -> Option<String> {
    let mut cur = String::new();
    for l in text.lines().map(str::trim) {
        if l.starts_with('[') { cur = l.trim_matches(['[', ']']).to_string(); continue; }
        if cur != sec || l.starts_with(';') || l.starts_with('#') { continue; }
        if let Some((a, b)) = l.split_once('=') { if a.trim() == k { let v = b.trim().trim_matches('"').to_string(); if !v.is_empty() { return Some(v); } } }
    }
    None
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct DbRef { kind: String, host: String, port: u16, socket: String, name: String, user: String, #[serde(skip)] password: String }

fn pg_cmd(prog: &str, d: &DbRef, db: &str) -> Result<(Cmd, SecretFile)> {
    let esc = |s: &str| s.replace('\\', "\\\\").replace(':', "\\:");
    let pass = SecretFile::new(&format!("*:*:*:{}:{}\n", esc(&d.user), esc(&d.password)), None)?;
    let mut c = Cmd::new(prog).env("PGPASSFILE", pass.path()).args(["-U", &d.user, "-d", db]);
    if !d.host.is_empty() { c = c.args(["-h", &d.host, "-p", &d.port.to_string()]); }
    Ok((c, pass))
}

fn my(d: &DbRef) -> MyConn {
    MyConn { host: if d.host == "localhost" { String::new() } else { d.host.clone() }, port: d.port, socket: d.socket.clone(), user: d.user.clone(), password: d.password.clone(), container: None }
}

/// Дамп базы: MySQL — одна база без CREATE DATABASE (можно загрузить под другим именем); PostgreSQL — pg_dump -Fc.
async fn dump_db(w: &mut crate::store::repo::SnapshotWriter, d: &DbRef, skip_data: &[String], log: &LogFn) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if d.kind == "postgresql" {
        let (mut c, _p) = pg_cmd("pg_dump", d, &d.name)?;
        c = c.arg("-Fc");
        for t in skip_data { c = c.arg(format!("--exclude-table-data={t}")); }
        kit::stream_cmd(w, "database.pgdump", &c, log).await.context("pg_dump")?;
        names.push("database.pgdump".into());
        return Ok(names);
    }
    let c = my(d);
    let df = c.defaults_file()?;
    let dump = c.prog(&["mariadb-dump", "mysqldump"]).await?;
    let base = c.cmd(&dump, &df).args(["--single-transaction", "--quick", "--routines", "--triggers", "--events", "--hex-blob", "--default-character-set=utf8mb4"]);
    if skip_data.is_empty() {
        kit::stream_cmd(w, "database.sql", &base.clone().arg(&d.name), log).await.context("дамп базы")?;
        names.push("database.sql".into());
    } else {
        let all: Vec<String> = c.query(&format!("SELECT table_name FROM information_schema.tables WHERE table_schema = '{}'", d.name.replace('\'', "''")), log).await?.into_iter().filter_map(|r| r.into_iter().next()).collect();
        let mut skip: Vec<String> = all.into_iter().filter(|t| skip_data.iter().any(|p| glob1(p, t))).collect();
        skip.sort();
        log(&format!("без данных: {}", skip.join(", ")));
        kit::stream_cmd(w, "schema.sql", &base.clone().args(["--no-data", &d.name]), log).await.context("дамп структуры")?;
        let mut dc = base.arg("--no-create-info");
        for t in &skip { dc = dc.arg(format!("--ignore-table={}.{t}", d.name)); }
        kit::stream_cmd(w, "data.sql", &dc.arg(&d.name), log).await.context("дамп данных")?;
        names.push("schema.sql".into());
        names.push("data.sql".into());
    }
    Ok(names)
}

fn glob1(p: &str, s: &str) -> bool { match p.strip_suffix('*') { Some(pre) => s.starts_with(pre), None => p == s } }

async fn load_db(ctx: &RestoreCtx, m: &crate::store::types::Manifest, d: &DbRef, target: &str, log: &LogFn) -> Result<()> {
    if d.kind == "postgresql" {
        let (c0, _p) = pg_cmd("psql", d, "postgres")?;
        let _ = kit::output(&c0.args(["-c", &format!("CREATE DATABASE \"{}\"", target.replace('"', "\"\""))]), log).await;
        let (c, _p2) = pg_cmd("pg_restore", d, target)?;
        return kit::feed(&ctx.repo, kit::stream(m, "database.pgdump")?, &c.args(["--clean", "--if-exists", "--no-owner"]), log).await.context("pg_restore");
    }
    let c = my(d);
    let df = c.defaults_file()?;
    let cl = c.client().await?;
    c.query(&format!("CREATE DATABASE IF NOT EXISTS `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin", target.replace('`', "``")), log).await?;
    for n in ["database.sql", "schema.sql", "data.sql"] {
        if let Ok(s) = kit::stream(m, n) { kit::feed(&ctx.repo, s, &c.cmd(&cl, &df).arg(target), log).await.with_context(|| format!("загрузка {n}"))?; }
    }
    Ok(())
}

struct App { product: String, service: String, conf: String, db: Option<DbRef>, config_paths: Vec<String>, data_dir: String, sqlite: Option<String>, user: String, health: String }

fn detect(cfg: &Value) -> Result<App> {
    let product = cfg_str(cfg, "product").unwrap_or_else(|| "zabbix".into());
    if product == "grafana" {
        let conf = cfg_str(cfg, "config_file").unwrap_or_else(|| "/etc/grafana/grafana.ini".into());
        let t = std::fs::read_to_string(&conf).with_context(|| format!("чтение {conf}"))?;
        let data = ini(&t, "paths", "data").unwrap_or_else(|| "/var/lib/grafana".into());
        let typ = ini(&t, "database", "type").unwrap_or_else(|| "sqlite3".into());
        let port = ini(&t, "server", "http_port").unwrap_or_else(|| "3000".into());
        let mut app = App { product, service: cfg_str(cfg, "service").unwrap_or_else(|| "grafana-server".into()), conf: conf.clone(), db: None, config_paths: vec![Path::new(&conf).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/etc/grafana".into())],
            data_dir: data.clone(), sqlite: None, user: "grafana".into(), health: format!("http://127.0.0.1:{port}/api/health") };
        if typ == "sqlite3" {
            let p = ini(&t, "database", "path").unwrap_or_else(|| "grafana.db".into());
            app.sqlite = Some(if p.starts_with('/') { p } else { format!("{data}/{p}") });
        } else {
            let (h, pt) = ini(&t, "database", "host").map(|h| h.split_once(':').map(|(a, b)| (a.to_string(), b.parse().unwrap_or(0))).unwrap_or((h, 0))).unwrap_or(("127.0.0.1".into(), 0));
            let kind = if typ == "postgres" { "postgresql" } else { "mysql" };
            app.db = Some(DbRef { kind: kind.into(), host: h, port: if pt == 0 { if kind == "mysql" { 3306 } else { 5432 } } else { pt }, socket: String::new(), name: ini(&t, "database", "name").unwrap_or_else(|| "grafana".into()),
                user: ini(&t, "database", "user").unwrap_or_else(|| "grafana".into()), password: cfg_str(cfg, "db_password").or_else(|| ini(&t, "database", "password")).unwrap_or_default() });
        }
        return Ok(app);
    }
    let conf = cfg_str(cfg, "config_file").unwrap_or_else(|| "/etc/zabbix/zabbix_server.conf".into());
    let t = std::fs::read_to_string(&conf).with_context(|| format!("чтение {conf}"))?;
    let kind = match cfg_str(cfg, "db_type").as_deref() {
        Some("mysql") => "mysql".to_string(),
        Some("postgresql") => "postgresql".to_string(),
        _ => {
            let l = std::process::Command::new("ldd").arg("/usr/sbin/zabbix_server").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
            if l.contains("libpq") { "postgresql".into() } else if l.contains("mysql") || l.contains("mariadb") { "mysql".into() } else if kv(&t, "DBPort").as_deref() == Some("5432") { "postgresql".into() } else { "mysql".into() }
        }
    };
    let host = kv(&t, "DBHost").unwrap_or_else(|| "localhost".into());
    let db = DbRef { port: kv(&t, "DBPort").and_then(|p| p.parse().ok()).unwrap_or(if kind == "mysql" { 3306 } else { 5432 }), socket: kv(&t, "DBSocket").unwrap_or_default(),
        name: kv(&t, "DBName").unwrap_or_else(|| "zabbix".into()), user: kv(&t, "DBUser").unwrap_or_else(|| "zabbix".into()),
        password: cfg_str(cfg, "db_password").or_else(|| kv(&t, "DBPassword")).unwrap_or_default(), host: if kind == "postgresql" && host == "localhost" { String::new() } else { host }, kind };
    let mut paths = vec![Path::new(&conf).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/etc/zabbix".into())];
    for k in ["AlertScriptsPath", "ExternalScripts"] { if let Some(p) = kv(&t, k) { paths.push(p); } }
    for p in ["/usr/lib/zabbix/alertscripts", "/usr/lib/zabbix/externalscripts", "/usr/share/zabbix/conf/zabbix.conf.php"] { paths.push(p.into()); }
    paths.sort();
    paths.dedup();
    paths.retain(|p| Path::new(p).exists());
    // Вложенные пути уже входят в родительский каталог.
    let all = paths.clone();
    paths.retain(|p| !all.iter().any(|q| q != p && p.starts_with(&format!("{q}/"))));
    Ok(App { product, service: cfg_str(cfg, "service").unwrap_or_else(|| "zabbix-server".into()), conf, db: Some(db), config_paths: paths, data_dir: String::new(), sqlite: None, user: "zabbix".into(), health: String::new() })
}

#[async_trait]
impl Plugin for Monitoring {
    fn id(&self) -> &'static str { "zabbix" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let a = detect(cfg)?;
        Ok(json!({ "product": a.product, "config": a.conf, "database": a.db, "sqlite": a.sqlite, "service": a.service, "active": svc::active(&a.service) }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let a = detect(&ctx.config)?;
        let mut w = ctx.repo.writer();
        let mut streams = Vec::new();
        if let Some(d) = &a.db {
            log(&format!("{} · база {} {} ({})", a.product, d.kind, d.name, if d.host.is_empty() { "локально".into() } else { d.host.clone() }));
            let skip: Vec<String> = if a.product == "zabbix" && !kit::bool_param(&ctx.config, "history", true) { vec!["history*".into(), "trends*".into()] } else { vec![] };
            streams = dump_db(&mut w, d, &skip, log).await?;
        }
        if let Some(db) = &a.sqlite {
            if !Path::new(db).exists() { bail!("нет файла базы Grafana {db}"); }
            match kit::which("sqlite3") {
                Some(_) => {
                    // .backup — согласованная копия работающей базы SQLite без остановки.
                    let wd = WorkDir::new(None, None)?;
                    let f = wd.join("grafana.db");
                    kit::run(&Cmd::new("sqlite3").args([db.as_str(), &format!(".backup '{}'", f.display())]), log).await.context("sqlite3 .backup")?;
                    let e = w.add_stream("grafana.db", std::fs::File::open(&f)?, None).await?;
                    log(&format!("  grafana.db: {}", crate::util::human_bytes(e.size)));
                }
                None => {
                    log("нет sqlite3 — база копируется при остановленной службе");
                    let f = db.clone();
                    let w2 = &mut w;
                    svc::while_stopped(std::slice::from_ref(&a.service), log, || async move { let e = w2.add_stream("grafana.db", std::fs::File::open(&f)?, None).await?; Ok(e) }).await?;
                }
            }
            streams.push("grafana.db".into());
        }
        if !a.data_dir.is_empty() && Path::new(&a.data_dir).exists() {
            let ex = crate::exclude::Excludes::new(&["grafana.db".to_string(), "grafana.db-journal".into(), "grafana.db-wal".into(), "grafana.db-shm".into(), "png".into(), "csv".into(), "log".into()], None);
            kit::tar_paths(&mut w, "data.tar", &[a.data_dir.clone()], ex, Default::default(), log).await?;
        }
        if !a.config_paths.is_empty() { kit::tar_paths(&mut w, "config.tar", &a.config_paths, Default::default(), Default::default(), log).await?; }
        let meta = json!({ "product": a.product, "service": a.service, "db": a.db, "sqlite": a.sqlite, "data_dir": a.data_dir, "user": a.user, "health": a.health, "streams": streams });
        let (header, stats) = w.finish(&ctx.instance, "zabbix", "full", json!({ "product": a.product }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let meta = m.meta.clone();
        if let Some(d) = cfg_str(&ctx.params, "target_dir") {
            std::fs::create_dir_all(&d)?;
            for s in &m.streams {
                if s.name.ends_with(".tar") { kit::untar(&ctx.repo, s, Path::new(&d), Default::default(), log).await?; } else { kit::to_file(&ctx.repo, s, &Path::new(&d).join(&s.name)).await?; }
            }
            return Ok(json!({ "target_dir": d }));
        }
        let service = cfg_str(&ctx.params, "service").or_else(|| meta["service"].as_str().map(|s| s.to_string())).unwrap_or_default();
        if svc::exists(&service) && svc::active(&service) { svc::stop(&service, log).await?; }
        let mut done = Vec::new();
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        if kit::bool_param(&ctx.params, "restore_config", true) {
            if let Ok(s) = kit::stream(&m, "config.tar") { kit::untar(&ctx.repo, s, Path::new("/"), Default::default(), log).await?; done.push("config"); }
            if let Ok(s) = kit::stream(&m, "data.tar") { kit::untar(&ctx.repo, s, Path::new("/"), Default::default(), log).await?; done.push("data"); }
        }
        if kit::bool_param(&ctx.params, "restore_database", true) {
            if let Some(db) = meta["sqlite"].as_str() {
                if Path::new(db).exists() { std::fs::rename(db, kit::aside_name(db, &ts))?; }
                kit::to_file(&ctx.repo, kit::stream(&m, "grafana.db")?, Path::new(db)).await?;
                let user = meta["user"].as_str().unwrap_or("grafana");
                if crate::util::user_ids(user).is_some() { kit::chown_to(&[db.to_string()], user, log)?; }
                done.push("grafana.db");
            } else if !meta["db"].is_null() {
                let mut d: DbRef = serde_json::from_value(meta["db"].clone())?;
                // Пароль в копию не пишется — из параметров экземпляра или файла конфигурации (он уже восстановлен).
                d.password = cfg_str(&ctx.params, "db_password").or_else(|| detect(&ctx.params).ok().and_then(|a| a.db.map(|x| x.password))).unwrap_or_default();
                let target = cfg_str(&ctx.params, "database").unwrap_or_else(|| d.name.clone());
                load_db(ctx, &m, &d, &target, log).await?;
                log(&format!("база загружена в {target}"));
                done.push("database");
            }
        }
        let mut healthy = Value::Null;
        if kit::bool_param(&ctx.params, "start", true) && svc::exists(&service) {
            svc::start(&service, log).await?;
            if let Some(h) = meta["health"].as_str().filter(|h| !h.is_empty()) { healthy = json!(kit::wait_http(h, 60).await); } else { healthy = json!(svc::active(&service)); }
        }
        if done.is_empty() { return Err(anyhow!("ничего не восстановлено")); }
        Ok(json!({ "restored": done, "healthy": healthy }))
    }
}

pub fn restore_keys() -> &'static [&'static str] { &["product", "config_file", "db_password", "service", "db_type"] }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conf_parsing() {
        let z = "# x\nDBHost=localhost\nDBName=zabbix\n# DBPassword=old\nDBPassword=s3cr3t\n";
        assert_eq!(kv(z, "DBPassword").as_deref(), Some("s3cr3t"));
        let g = "[paths]\ndata = /srv/grafana\n[database]\n;type = mysql\ntype = sqlite3\n";
        assert_eq!(ini(g, "paths", "data").as_deref(), Some("/srv/grafana"));
        assert_eq!(ini(g, "database", "type").as_deref(), Some("sqlite3"));
        assert!(glob1("history*", "history_uint") && !glob1("history*", "items"));
    }
}
