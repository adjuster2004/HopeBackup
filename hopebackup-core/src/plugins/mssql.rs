//! Microsoft SQL Server (Linux, в том числе в контейнере): штатные BACKUP DATABASE / BACKUP LOG через
//! sqlcmd в промежуточный каталог сервера, файл .bak — потоком в репозиторий и удаляется. Цепочка:
//! полная → разностная → журналы транзакций; восстановление выстраивает её сам (RESTORE … NORECOVERY,
//! последний журнал — STOPAT на момент времени, затем RECOVERY), с переименованием базы и MOVE файлов.
//! Пароль — через SQLCMDPASSWORD (не в командной строке).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::store::types::SnapshotHeader;

pub struct Mssql;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "host": { "type": "string", "title": "Сервер", "default": "localhost", "description": "Имя или адрес; именованный экземпляр — host\\\\INSTANCE" },
        "port": { "type": "integer", "title": "Порт", "default": 1433 },
        "user": { "type": "string", "title": "Пользователь", "default": "sa", "description": "Роль sysadmin или db_backupoperator в каждой базе (для восстановления — dbcreator)" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "trust_cert": { "type": "boolean", "title": "Доверять сертификату сервера", "default": true, "description": "Самоподписанный сертификат SQL Server (sqlcmd -C)" },
        "container": { "type": "string", "title": "Контейнер Docker", "description": "sqlcmd и промежуточный каталог — в контейнере" },
        "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы", "description": "Пусто — все пользовательские базы в состоянии ONLINE" },
        "include_system": { "type": "boolean", "title": "Системные базы master, msdb, model", "default": true },
        "incr_mode": { "type": "string", "title": "Копия по расписанию «Журнал»", "enum": ["log", "diff"], "default": "log",
            "description": "log — журнал транзакций (восстановление на момент; для баз в модели SIMPLE — разностная); diff — разностная" },
        "copy_only": { "type": "boolean", "title": "COPY_ONLY для полных копий", "default": false, "description": "Не нарушать цепочку другой системы резервного копирования (разностные тогда невозможны)" },
        "compression": { "type": "boolean", "title": "Сжатие SQL Server", "default": false, "description": "Выключено — лучше дедупликация (одинаковые страницы между копиями)" },
        "stage_dir": { "type": "string", "title": "Промежуточный каталог (на сервере SQL)", "default": "/var/opt/mssql/hopebackup", "description": "Куда SQL Server пишет .bak; запись разрешена пользователю mssql" },
        "stage_local": { "type": "string", "title": "Тот же каталог на этой машине", "description": "Если SQL Server на другой машине — путь к общему каталогу здесь; пусто — тот же путь" }
    } })
}

pub fn defaults() -> Value { json!({ "host": "localhost", "port": 1433, "user": "sa", "trust_cert": true, "include_system": true, "incr_mode": "log", "stage_dir": "/var/opt/mssql/hopebackup" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы", "description": "Пусто — все базы копии" },
        "rename_to": { "type": "string", "title": "Под именем", "description": "Для одной базы — восстановить рядом с рабочей под этим именем (файлы переименовываются)" },
        "to_time": { "type": "string", "title": "Момент восстановления", "description": "ГГГГ-ММ-ДД ЧЧ:ММ:СС — журналы до этого момента (STOPAT)" },
        "data_dir": { "type": "string", "title": "Каталог файлов данных", "description": "Пусто — как в копии (или каталог по умолчанию сервера при новом имени)" },
        "log_dir": { "type": "string", "title": "Каталог журналов" },
        "recovery": { "type": "boolean", "title": "Перевести базы в рабочее состояние (RECOVERY)", "default": true, "description": "Выключено — оставить в NORECOVERY для ручного применения журналов" },
        "host": { "type": "string", "title": "Целевой сервер" },
        "port": { "type": "integer", "title": "Порт" },
        "user": { "type": "string", "title": "Пользователь" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["host", "port", "user", "password", "trust_cert", "container", "stage_dir", "stage_local"] }

#[derive(Clone)]
struct Conn { host: String, port: u16, user: String, password: String, trust: bool, container: Option<String>, stage: String, stage_local: String }

impl Conn {
    fn from(v: &Value) -> Conn {
        let stage = cfg_str(v, "stage_dir").unwrap_or_else(|| "/var/opt/mssql/hopebackup".into()).trim_end_matches('/').to_string();
        Conn {
            host: cfg_str(v, "host").unwrap_or_else(|| "localhost".into()),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(1433) as u16,
            user: cfg_str(v, "user").unwrap_or_else(|| "sa".into()),
            password: cfg_str(v, "password").unwrap_or_default(),
            trust: kit::bool_param(v, "trust_cert", true),
            container: cfg_str(v, "container"),
            stage_local: cfg_str(v, "stage_local").map(|s| s.trim_end_matches('/').to_string()).unwrap_or_else(|| stage.clone()),
            stage,
        }
    }
    async fn sqlcmd(&self) -> Result<String> {
        kit::find_prog(self.container.as_deref(), &["sqlcmd", "/opt/mssql-tools18/bin/sqlcmd", "/opt/mssql-tools/bin/sqlcmd"]).await
            .ok_or_else(|| anyhow!("нет sqlcmd{} — установите mssql-tools18 (или go-sqlcmd)", self.container.as_ref().map(|c| format!(" в контейнере {c}")).unwrap_or_default()))
    }
    fn server(&self) -> String { if self.host.contains('\\') || self.port == 1433 && self.host.contains(',') { self.host.clone() } else { format!("{},{}", self.host, self.port) } }
    async fn cmd(&self) -> Result<Cmd> {
        let mut c = Cmd::new(self.sqlcmd().await?).container(self.container.clone()).args(["-S", &self.server(), "-U", &self.user, "-b", "-h", "-1", "-W", "-s", "|", "-w", "65535", "-r", "1"]);
        if self.trust { c = c.arg("-C"); }
        if !self.password.is_empty() { c = c.env("SQLCMDPASSWORD", &self.password); }
        Ok(c)
    }
    /// Запрос: строки, поля через «|». Долгие операции (BACKUP/RESTORE) — без ограничения времени.
    async fn q(&self, sql: &str, log: &LogFn) -> Result<Vec<Vec<String>>> {
        let c = self.cmd().await?.args(["-Q", &format!("SET NOCOUNT ON; {sql}")]);
        let out = kit::output(&c, log).await.map_err(|e| anyhow!("{}", clean_err(&format!("{e:#}"))))?;
        Ok(out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("Processed ") && !l.contains("successfully processed")).map(|l| l.split('|').map(|x| x.trim().to_string()).collect()).collect())
    }
    async fn exec(&self, sql: &str, log: &LogFn) -> Result<()> {
        log(&format!("  SQL: {}", sql.chars().take(300).collect::<String>()));
        let c = self.cmd().await?.args(["-Q", sql]);
        let out = kit::output(&c, log).await.map_err(|e| anyhow!("{}", clean_err(&format!("{e:#}"))))?;
        for l in out.lines().filter(|l| l.contains("successfully processed")) { log(&format!("  {}", l.trim())); }
        Ok(())
    }
}

/// Текст ошибки sqlcmd без служебных префиксов.
fn clean_err(s: &str) -> String { s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim()).collect::<Vec<_>>().join(" ") }

fn qi(n: &str) -> String { format!("[{}]", n.replace(']', "]]")) }
fn qs(s: &str) -> String { format!("N'{}'", s.replace('\'', "''")) }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct DbPiece { name: String, kind: String, recovery: String, file: String, first_lsn: String, last_lsn: String, database_backup_lsn: String }

/// Имя файла .bak без особых символов.
fn file_name(db: &str, kind: &str) -> String {
    let safe: String = db.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("{safe}.{kind}.{}.bak", crate::util::short_id())
}

async fn read_and_remove(c: &Conn, w: &mut crate::store::repo::SnapshotWriter, name: &str, file: &str, log: &LogFn) -> Result<u64> {
    let r = match &c.container {
        Some(ct) => kit::stream_cmd(w, name, &Cmd::new("cat").arg(format!("{}/{file}", c.stage)).container(Some(ct.clone())), log).await.map(|e| e.size),
        None => {
            let p = Path::new(&c.stage_local).join(file);
            let f = std::fs::File::open(&p).with_context(|| format!("файл копии {} (промежуточный каталог на этой машине — «stage_local»)", p.display()))?;
            let e = w.add_stream(name, f, None).await?;
            log(&format!("  {name}: {}", crate::util::human_bytes(e.size)));
            Ok(e.size)
        }
    };
    remove_staged(c, file, log).await;
    r
}

async fn remove_staged(c: &Conn, file: &str, log: &LogFn) {
    match &c.container {
        Some(ct) => { let _ = kit::output(&Cmd::new("rm").args(["-f", &format!("{}/{file}", c.stage)]).container(Some(ct.clone())), log).await; }
        None => { let _ = std::fs::remove_file(Path::new(&c.stage_local).join(file)); }
    }
}

async fn ensure_stage(c: &Conn, log: &LogFn) -> Result<()> {
    match &c.container {
        Some(ct) => { kit::output(&Cmd::new("mkdir").args(["-p", &c.stage]).container(Some(ct.clone())), log).await?; }
        None => {
            if !Path::new(&c.stage_local).exists() {
                std::fs::create_dir_all(&c.stage_local).with_context(|| format!("промежуточный каталог {}", c.stage_local))?;
                if let Some((u, g)) = crate::util::user_ids("mssql") { let _ = std::os::unix::fs::chown(&c.stage_local, Some(u), Some(g)); }
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&c.stage_local, std::fs::Permissions::from_mode(0o770));
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Plugin for Mssql {
    fn id(&self) -> &'static str { "mssql" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let c = Conn::from(cfg);
        let v = c.q("SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)), CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(260))", log).await?;
        let dbs = c.q("SELECT name, recovery_model_desc, state_desc FROM sys.databases WHERE name <> 'tempdb'", log).await?;
        Ok(json!({ "version": v.first().and_then(|r| r.first()), "edition": v.first().and_then(|r| r.get(1)), "data_path": v.first().and_then(|r| r.get(2)),
            "databases": dbs.iter().map(|r| json!({ "name": r[0], "recovery": r.get(1), "state": r.get(2) })).collect::<Vec<_>>() }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let c = Conn::from(&ctx.config);
        let v = c.q("SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)), CAST(SERVERPROPERTY('Edition') AS nvarchar(128))", log).await.context("подключение к SQL Server")?;
        let ver = v.first().and_then(|r| r.first()).cloned().unwrap_or_default();
        let edition = v.first().and_then(|r| r.get(1)).cloned().unwrap_or_default();
        log(&format!("SQL Server {ver} ({edition})"));
        let want = crate::connectors::net::p_list(&ctx.config, "databases");
        let sys = kit::bool_param(&ctx.config, "include_system", true);
        let all = c.q("SELECT name, recovery_model_desc, state_desc FROM sys.databases WHERE name <> 'tempdb'", log).await?;
        let incr = ctx.kind == "incr" || ctx.kind == "log" || ctx.kind == "diff";
        let mode = if ctx.kind == "diff" { "diff".to_string() } else if ctx.kind == "log" { "log".into() } else { cfg_str(&ctx.config, "incr_mode").unwrap_or_else(|| "log".into()) };
        let mut dbs: Vec<(String, String)> = Vec::new();
        for r in &all {
            let (n, rec, st) = (r[0].clone(), r.get(1).cloned().unwrap_or_default(), r.get(2).cloned().unwrap_or_default());
            let system = ["master", "msdb", "model"].contains(&n.as_str());
            if !want.is_empty() && !want.contains(&n) { continue; }
            if want.is_empty() && system && !sys { continue; }
            if st != "ONLINE" { log(&format!("  база {n}: {st} — пропущена")); continue; }
            // Системные базы — только полные копии (master — без разностных).
            if incr && system { continue; }
            dbs.push((n, rec));
        }
        for w in &want { if !all.iter().any(|r| &r[0] == w) { bail!("нет базы {w}"); } }
        if dbs.is_empty() { bail!("нет баз для копии"); }
        ensure_stage(&c, log).await?;
        // Для разностной и журнала нужна полная копия этой системы (или не COPY_ONLY).
        let prev = kit::snapshots_of(&ctx.repo, &ctx.instance).await?;
        if incr && !prev.iter().any(|s| s.kind == "full") { bail!("сначала нужна полная копия: с неё начинается цепочка"); }
        let mut w = ctx.repo.writer();
        let mut pieces = Vec::new();
        let copy_only = kit::bool_param(&ctx.config, "copy_only", false);
        let comp = if kit::bool_param(&ctx.config, "compression", false) { ", COMPRESSION" } else { "" };
        for (db, rec) in &dbs {
            let kind = if !incr { "full" } else if mode == "log" && rec != "SIMPLE" { "log" } else { "diff" };
            if incr && mode == "log" && kind == "diff" { log(&format!("  {db}: модель восстановления SIMPLE — журнала нет, разностная копия")); }
            let file = file_name(db, kind);
            let path = format!("{}/{file}", c.stage);
            let sql = match kind {
                "full" => format!("BACKUP DATABASE {} TO DISK = {} WITH INIT, FORMAT, CHECKSUM{comp}{}, NAME = N'HopeBackup full'", qi(db), qs(&path), if copy_only { ", COPY_ONLY" } else { "" }),
                "diff" => format!("BACKUP DATABASE {} TO DISK = {} WITH DIFFERENTIAL, INIT, FORMAT, CHECKSUM{comp}, NAME = N'HopeBackup diff'", qi(db), qs(&path)),
                _ => format!("BACKUP LOG {} TO DISK = {} WITH INIT, FORMAT, CHECKSUM{comp}, NAME = N'HopeBackup log'", qi(db), qs(&path)),
            };
            if let Err(e) = c.exec(&sql, log).await { remove_staged(&c, &file, log).await; return Err(e.context(format!("копия базы {db}"))); }
            let lsn = c.q(&format!("SELECT TOP 1 CAST(first_lsn AS nvarchar(40)), CAST(last_lsn AS nvarchar(40)), CAST(database_backup_lsn AS nvarchar(40)) FROM msdb.dbo.backupset WHERE database_name = {} ORDER BY backup_set_id DESC", qs(db)), log).await.unwrap_or_default();
            let l = lsn.first().cloned().unwrap_or_default();
            read_and_remove(&c, &mut w, &format!("db/{db}.bak"), &file, log).await?;
            pieces.push(DbPiece { name: db.clone(), kind: kind.into(), recovery: rec.clone(), file: format!("db/{db}.bak"), first_lsn: l.first().cloned().unwrap_or_default(), last_lsn: l.get(1).cloned().unwrap_or_default(), database_backup_lsn: l.get(2).cloned().unwrap_or_default() });
        }
        let kind = if incr { "incr" } else { "full" };
        let meta = json!({ "version": ver, "edition": edition, "dbs": pieces, "copy_only": copy_only });
        let (header, stats) = w.finish(&ctx.instance, "mssql", kind, json!({ "version": ver, "dbs": pieces.iter().map(|p| format!("{}:{}", p.name, p.kind)).collect::<Vec<_>>() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (h, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let snaps = kit::snapshots_of(&ctx.repo, &ctx.instance).await?;
        let to_time = cfg_str(&ctx.params, "to_time").map(|s| kit::parse_time(&s)).transpose()?;
        let mut names: Vec<String> = crate::connectors::net::p_list(&ctx.params, "databases");
        let here: Vec<DbPiece> = serde_json::from_value(m.meta["dbs"].clone()).unwrap_or_default();
        if names.is_empty() { names = here.iter().map(|p| p.name.clone()).collect(); }
        // В копии журнала нет системных баз — восстановление по ней их не трогает.
        let rename = cfg_str(&ctx.params, "rename_to");
        if rename.is_some() && names.len() != 1 { bail!("«Под именем» — только для одной базы (укажите её в «Базы»)"); }
        let c = Conn::from(&ctx.params);
        ensure_stage(&c, log).await?;
        let defaults = c.q("SELECT CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(260)), CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(260))", log).await.unwrap_or_default();
        let (def_data, def_log) = (defaults.first().and_then(|r| r.first()).cloned().unwrap_or_default(), defaults.first().and_then(|r| r.get(1)).cloned().unwrap_or_default());
        let recovery = kit::bool_param(&ctx.params, "recovery", true);
        let mut done = Vec::new();
        for db in &names {
            let chain = chain_for(&ctx.repo, &snaps, &h, db, to_time).await?;
            let target = rename.clone().unwrap_or_else(|| db.clone());
            log(&format!("база {db}{}: цепочка {}", if target != *db { format!(" → {target}") } else { String::new() }, chain.iter().map(|(s, p)| format!("{} {}", p.kind, s.time.with_timezone(&chrono::Local).format("%d.%m %H:%M:%S"))).collect::<Vec<_>>().join(" → ")));
            if db == "master" && target == *db { log("  ! master восстанавливается только в однопользовательском режиме сервера (-m): восстановите её под другим именем или вручную"); continue; }
            // Существующую базу — в монопольный режим (разорвать подключения).
            let _ = c.exec(&format!("IF DB_ID({}) IS NOT NULL AND DATABASEPROPERTYEX({}, 'Status') = 'ONLINE' ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE", qs(&target), qs(&target), qi(&target)), log).await;
            let n = chain.len();
            for (i, (s, p)) in chain.iter().enumerate() {
                let (_, sm) = ctx.repo.manifest(&s.id).await?;
                let st = kit::stream(&sm, &p.file)?;
                let file = file_name(&target, &p.kind);
                stage_put(&c, &ctx.repo, st, &file, log).await?;
                let path = format!("{}/{file}", c.stage);
                let stopat = to_time.filter(|_| p.kind == "log").map(|t| format!(", STOPAT = {}", qs(&t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S").to_string()))).unwrap_or_default();
                let last = i + 1 == n;
                let rec = if last && recovery { "RECOVERY" } else { "NORECOVERY" };
                let sql = match p.kind.as_str() {
                    "full" => {
                        let moves = moves_for(&c, &path, db, &target, &ctx.params, &def_data, &def_log, log).await?;
                        format!("RESTORE DATABASE {} FROM DISK = {} WITH {rec}, REPLACE, STATS = 25{moves}", qi(&target), qs(&path))
                    }
                    "diff" => format!("RESTORE DATABASE {} FROM DISK = {} WITH {rec}", qi(&target), qs(&path)),
                    _ => format!("RESTORE LOG {} FROM DISK = {} WITH {rec}{stopat}", qi(&target), qs(&path)),
                };
                let r = c.exec(&sql, log).await;
                remove_staged(&c, &file, log).await;
                r.with_context(|| format!("восстановление {target} ({})", p.kind))?;
            }
            if recovery { let _ = c.exec(&format!("IF DATABASEPROPERTYEX({}, 'UserAccess') = 'SINGLE_USER' ALTER DATABASE {} SET MULTI_USER", qs(&target), qi(&target)), log).await; }
            done.push(json!({ "database": db, "restored_as": target, "chain": chain.iter().map(|(s, p)| json!({ "snapshot": s.id, "kind": p.kind })).collect::<Vec<_>>() }));
        }
        Ok(json!({ "databases": done, "to_time": to_time, "recovery": recovery }))
    }
}

/// Цепочка для базы: полная, затем последняя разностная после неё, затем журналы — до выбранной копии
/// или до первой копии журнала не раньше момента восстановления.
async fn chain_for(repo: &std::sync::Arc<crate::store::repo::Repo>, snaps: &[SnapshotHeader], sel: &SnapshotHeader, db: &str, to_time: Option<chrono::DateTime<chrono::Utc>>) -> Result<Vec<(SnapshotHeader, DbPiece)>> {
    let mut items: Vec<(SnapshotHeader, DbPiece)> = Vec::new();
    for s in snaps.iter().filter(|s| s.time <= sel.time || to_time.is_some()) {
        let dbs: Vec<String> = s.extra.get("dbs").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|y| y.to_string())).collect()).unwrap_or_default();
        if !dbs.iter().any(|x| x.rsplit_once(':').map(|(n, _)| n == db).unwrap_or(false)) { continue; }
        let (_, m) = repo.manifest(&s.id).await?;
        let ps: Vec<DbPiece> = serde_json::from_value(m.meta["dbs"].clone()).unwrap_or_default();
        if let Some(p) = ps.into_iter().find(|p| p.name == db) { items.push((s.clone(), p)); }
    }
    let base = items.iter().rposition(|(s, p)| p.kind == "full" && s.time <= sel.time).ok_or_else(|| anyhow!("нет полной копии базы {db} раньше выбранной"))?;
    let mut out = vec![items[base].clone()];
    let end = |s: &SnapshotHeader| if to_time.is_some() { true } else { s.time <= sel.time };
    let after: Vec<&(SnapshotHeader, DbPiece)> = items[base + 1..].iter().filter(|(s, _)| end(s)).collect();
    // Полная после основы, но до момента — тогда основа она (выбор пользователя — ближайшая раньше).
    let diff = after.iter().filter(|(s, p)| p.kind == "diff" && (to_time.map(|t| s.time <= t).unwrap_or(true))).last().cloned();
    let from = diff.map(|d| d.0.time).unwrap_or(items[base].0.time);
    if let Some(d) = diff { out.push(d.clone()); }
    for it in after.iter().filter(|(s, p)| p.kind == "log" && s.time > from) {
        out.push((*it).clone());
        if let Some(t) = to_time { if it.0.time >= t { break; } }
    }
    Ok(out)
}

/// Положить .bak из копии в промежуточный каталог сервера.
async fn stage_put(c: &Conn, repo: &std::sync::Arc<crate::store::repo::Repo>, s: &crate::store::types::StreamEntry, file: &str, log: &LogFn) -> Result<()> {
    match &c.container {
        Some(ct) => kit::feed(repo, s, &Cmd::new("sh").args(["-c", &format!("cat > {}", crate::util::shq(&format!("{}/{file}", c.stage)))]).container(Some(ct.clone())), log).await,
        None => {
            let p = Path::new(&c.stage_local).join(file);
            kit::to_file(repo, s, &p).await?;
            if let Some((u, g)) = crate::util::user_ids("mssql") { let _ = std::os::unix::fs::chown(&p, Some(u), Some(g)); }
            Ok(())
        }
    }
}

/// MOVE для файлов базы: при новом имени или заданных каталогах.
#[allow(clippy::too_many_arguments)]
async fn moves_for(c: &Conn, path: &str, db: &str, target: &str, p: &Value, def_data: &str, def_log: &str, log: &LogFn) -> Result<String> {
    let data_dir = cfg_str(p, "data_dir");
    let log_dir = cfg_str(p, "log_dir");
    if target == db && data_dir.is_none() && log_dir.is_none() { return Ok(String::new()); }
    let files = c.q(&format!("RESTORE FILELISTONLY FROM DISK = {}", qs(path)), log).await?;
    let mut out = String::new();
    for f in files.iter().filter(|f| f.len() >= 3) {
        let (logical, physical, typ) = (&f[0], &f[1], &f[2]);
        let sep = if physical.contains('\\') { '\\' } else { '/' };
        let (dir0, base) = physical.rsplit_once(sep).map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((String::new(), physical.clone()));
        let dir = if typ == "L" { log_dir.clone().or_else(|| (!def_log.is_empty()).then(|| def_log.trim_end_matches(['/', '\\']).to_string())).unwrap_or(dir0.clone()) } else { data_dir.clone().or_else(|| (!def_data.is_empty()).then(|| def_data.trim_end_matches(['/', '\\']).to_string())).unwrap_or(dir0.clone()) };
        let name = if target != db { base.replacen(db, target, 1).replace(&format!("{target}{target}"), target) } else { base.clone() };
        let name = if target != db && name == base { format!("{target}_{base}") } else { name };
        out.push_str(&format!(", MOVE {} TO {}", qs(logical), qs(&format!("{dir}{sep}{name}"))));
    }
    Ok(out)
}
