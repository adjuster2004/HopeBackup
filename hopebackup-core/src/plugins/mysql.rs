//! MySQL и MariaDB: логический дамп (mysqldump / mariadb-dump, согласованный --single-transaction) или
//! физическая копия (mariabackup / xtrabackup потоком xbstream), архив журналов binlog для восстановления
//! на момент времени. Пароль — во временном файле параметров 0600 (--defaults-extra-file), не в командной
//! строке; в контейнере Docker — через окружение docker exec.
//!
//! Восстановление: дамп — в любой сервер (весь или одна база, по желанию под другим именем); физическая —
//! распаковка, prepare и замена каталога данных (прежний остаётся рядом) или только распаковка в каталог;
//! затем, если задан момент, — прокрутка binlog до него (mysqlbinlog --stop-datetime | mysql).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{BufRead, Read};
use std::path::Path;

use super::appkit::{self as kit, svc, Cmd, SecretFile, WorkDir};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Mysql;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "host": { "type": "string", "title": "Адрес сервера", "description": "Пусто — локальный сокет" },
        "port": { "type": "integer", "title": "Порт", "default": 3306 },
        "socket": { "type": "string", "title": "Сокет", "description": "Пусто — по умолчанию клиента (/run/mysqld/mysqld.sock)" },
        "user": { "type": "string", "title": "Пользователь", "default": "root", "description": "Права: SELECT, SHOW VIEW, TRIGGER, EVENT, LOCK TABLES, RELOAD, PROCESS, REPLICATION CLIENT (для binlog — ещё REPLICATION SLAVE)" },
        "password": { "type": "string", "format": "secret", "title": "Пароль", "description": "Пусто — вход по сокету (unix_socket) от root" },
        "container": { "type": "string", "title": "Контейнер Docker", "description": "Имя контейнера с сервером: утилиты выполняются в нём (docker exec)" },
        "method": { "type": "string", "title": "Способ полной копии", "enum": ["auto", "dump", "physical"], "default": "auto",
            "description": "dump — логический дамп (переносим между версиями, можно восстановить одну базу); physical — mariabackup/xtrabackup (быстрее для больших баз, та же версия сервера); auto — физическая для баз больше 20 ГБ, если есть утилита" },
        "databases": { "type": "array", "items": { "type": "string" }, "title": "Базы", "description": "Пусто — все (с системной mysql: пользователи и права)" },
        "binlog": { "type": "boolean", "title": "Архив binlog (восстановление на момент времени)", "default": true, "description": "Копия журнала — по расписанию «Журнал» (FLUSH BINARY LOGS и закрытые файлы binlog). Нужен log_bin" },
        "service": { "type": "string", "title": "Служба", "description": "Для физического восстановления на место; пусто — mariadb, mysql или mysqld" }
    } })
}

pub fn defaults() -> Value { json!({ "port": 3306, "user": "root", "method": "auto", "binlog": true }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "database": { "type": "string", "title": "Только база", "description": "Из полного дампа — одна база; пусто — всё" },
        "rename_to": { "type": "string", "title": "Под именем", "description": "Восстановить базу под другим именем (рядом с рабочей)" },
        "to_time": { "type": "string", "title": "Момент восстановления", "description": "ГГГГ-ММ-ДД ЧЧ:ММ:СС — прокрутить binlog до этого момента; пусто — на момент копии" },
        "host": { "type": "string", "title": "Целевой сервер", "description": "Пусто — сервер экземпляра" },
        "port": { "type": "integer", "title": "Порт" },
        "user": { "type": "string", "title": "Пользователь" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "target_dir": { "type": "string", "title": "Каталог для физической копии", "description": "Распаковать и подготовить (prepare) сюда, сервер не трогать; пусто — заменить каталог данных сервера" },
        "start": { "type": "boolean", "title": "Запустить сервер после замены каталога данных", "default": true }
    } })
}

// ---------------------------------------------------------------- подключение

#[derive(Clone)]
pub(crate) struct Conn {
    pub host: String,
    pub port: u16,
    pub socket: String,
    pub user: String,
    pub password: String,
    pub container: Option<String>,
}

impl Conn {
    fn from(v: &Value, over: Option<&Value>) -> Conn {
        let g = |k: &str| over.and_then(|o| cfg_str(o, k)).or_else(|| cfg_str(v, k));
        Conn {
            host: g("host").unwrap_or_default(),
            port: over.and_then(|o| o.get("port")).and_then(|x| x.as_u64()).or_else(|| v.get("port").and_then(|x| x.as_u64())).unwrap_or(3306) as u16,
            socket: g("socket").unwrap_or_default(),
            user: g("user").unwrap_or_else(|| "root".into()),
            password: over.and_then(|o| cfg_str(o, "password")).or_else(|| cfg_str(v, "password")).unwrap_or_default(),
            container: cfg_str(v, "container"),
        }
    }

    /// Файл параметров клиента ([client] и разделы утилит копии): на хосте — пароль в нём.
    pub(crate) fn defaults_file(&self) -> Result<Option<SecretFile>> {
        if self.container.is_some() { return Ok(None); }
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let mut body = String::new();
        for sec in ["client", "mariabackup", "xtrabackup", "mysqlbinlog", "mariadb-binlog"] {
            body.push_str(&format!("[{sec}]\nuser={}\n", q(&self.user)));
            if !self.password.is_empty() { body.push_str(&format!("password={}\n", q(&self.password))); }
            if !self.host.is_empty() { body.push_str(&format!("host={}\nport={}\n", q(&self.host), self.port)); }
            if !self.socket.is_empty() { body.push_str(&format!("socket={}\n", q(&self.socket))); }
        }
        Ok(Some(SecretFile::new(&body, None)?))
    }

    /// Команда утилиты MySQL: первым аргументом — файл параметров (так требуют утилиты).
    pub(crate) fn cmd(&self, prog: &str, df: &Option<SecretFile>) -> Cmd {
        let mut c = Cmd::new(prog).container(self.container.clone());
        match df {
            Some(f) => c = c.arg(format!("--defaults-extra-file={}", f.path())),
            None => {
                c = c.arg(format!("--user={}", self.user));
                if !self.password.is_empty() { c = c.env("MYSQL_PWD", &self.password); }
                if !self.host.is_empty() { c = c.arg(format!("--host={}", self.host)).arg(format!("--port={}", self.port)); }
                if !self.socket.is_empty() { c = c.arg(format!("--socket={}", self.socket)); }
            }
        }
        c
    }

    pub(crate) async fn prog(&self, names: &[&str]) -> Result<String> {
        kit::find_prog(self.container.as_deref(), names).await.ok_or_else(|| anyhow!("нет {} {}", names.join(" / "), match &self.container { Some(c) => format!("в контейнере {c}"), None => "на хосте — установите клиент MySQL/MariaDB".into() }))
    }

    pub(crate) async fn client(&self) -> Result<String> { self.prog(&["mariadb", "mysql"]).await }

    /// Запрос: строки, поля через табуляцию.
    pub(crate) async fn query(&self, sql: &str, log: &LogFn) -> Result<Vec<Vec<String>>> {
        let df = self.defaults_file()?;
        let c = self.cmd(&self.client().await?, &df).args(["-N", "-B", "-e", sql]);
        let out = kit::output(&c, log).await?;
        Ok(out.lines().map(|l| l.split('\t').map(|x| x.to_string()).collect()).collect())
    }

    async fn vars(&self, log: &LogFn) -> Result<std::collections::HashMap<String, String>> {
        let rows = self.query("SHOW GLOBAL VARIABLES WHERE Variable_name IN ('version','version_comment','datadir','log_bin','log_bin_basename','gtid_mode','server_id','innodb_version')", log).await?;
        Ok(rows.into_iter().filter(|r| r.len() >= 2).map(|r| (r[0].clone(), r[1].clone())).collect())
    }

    /// Текущий файл binlog (SHOW MASTER STATUS, в MySQL 8.4 — SHOW BINARY LOG STATUS).
    async fn current_binlog(&self, log: &LogFn) -> Option<(String, u64)> {
        for q in ["SHOW MASTER STATUS", "SHOW BINARY LOG STATUS"] {
            if let Ok(r) = self.query(q, log).await {
                if let Some(x) = r.first().filter(|x| x.len() >= 2) { return Some((x[0].clone(), x[1].parse().unwrap_or(4))); }
            }
        }
        None
    }
}

fn is_mariadb(v: &std::collections::HashMap<String, String>) -> bool {
    v.get("version").map(|s| s.to_lowercase().contains("mariadb")).unwrap_or(false) || v.get("version_comment").map(|s| s.to_lowercase().contains("mariadb")).unwrap_or(false)
}

/// Позиция binlog из заголовка дампа (--master-data=2 / --source-data=2).
pub fn dump_binlog_pos(head: &str) -> Option<(String, u64)> {
    for l in head.lines() {
        let l = l.trim_start_matches("-- ");
        let (fk, pk) = if l.contains("MASTER_LOG_FILE") { ("MASTER_LOG_FILE='", "MASTER_LOG_POS=") } else if l.contains("SOURCE_LOG_FILE") { ("SOURCE_LOG_FILE='", "SOURCE_LOG_POS=") } else { continue };
        let f = l.split(fk).nth(1)?.split('\'').next()?.to_string();
        let p = l.split(pk).nth(1)?.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
        return Some((f, p));
    }
    None
}

/// Порядковый номер файла binlog (mysql-bin.000012 → 12).
fn binlog_seq(n: &str) -> u64 { n.rsplit('.').next().and_then(|x| x.parse().ok()).unwrap_or(0) }

/// Фильтр дампа: только раздел базы `db` (по меткам «-- Current Database:»), по желанию — под новым именем.
pub struct DumpFilter<R: BufRead> {
    r: R,
    db: String,
    rename: Option<String>,
    on: Option<bool>,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: BufRead> DumpFilter<R> {
    pub fn new(r: R, db: &str, rename: Option<String>) -> Self { DumpFilter { r, db: db.into(), rename, on: None, buf: Vec::new(), pos: 0 } }

    fn next_line(&mut self) -> std::io::Result<bool> {
        self.buf.clear();
        self.pos = 0;
        loop {
            let mut line = Vec::new();
            if self.r.read_until(b'\n', &mut line)? == 0 { return Ok(false); }
            let s = String::from_utf8_lossy(&line);
            if let Some(rest) = s.strip_prefix("-- Current Database: ") {
                let name = rest.trim().trim_matches('`').replace("``", "`");
                self.on = Some(name == self.db);
            }
            // До первой базы — заголовок (SET …), его оставляем; дальше — только своя база.
            if self.on == Some(false) { continue; }
            if let (Some(new), Some(true)) = (&self.rename, self.on) {
                let q = format!("`{}`", self.db.replace('`', "``"));
                let nq = format!("`{}`", new.replace('`', "``"));
                let t = s.to_string();
                let t = if t.starts_with("CREATE DATABASE") || t.starts_with("USE ") || t.starts_with("-- Current Database:") { t.replacen(&q, &nq, 1) } else { t };
                self.buf.extend_from_slice(t.as_bytes());
            } else {
                self.buf.extend_from_slice(&line);
            }
            return Ok(true);
        }
    }
}

impl<R: BufRead> Read for DumpFilter<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() && !self.next_line()? { return Ok(0); }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn service_name(cfg: &Value) -> Option<String> {
    cfg_str(cfg, "service").or_else(|| ["mariadb", "mysql", "mysqld"].iter().find(|s| svc::exists(s)).map(|s| s.to_string()))
}

async fn db_size(c: &Conn, log: &LogFn) -> u64 {
    c.query("SELECT COALESCE(SUM(data_length+index_length),0) FROM information_schema.tables", log).await.ok()
        .and_then(|r| r.first().and_then(|x| x.first()).and_then(|s| s.parse::<f64>().ok())).unwrap_or(0.0) as u64
}

#[async_trait]
impl Plugin for Mysql {
    fn id(&self) -> &'static str { "mysql" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let c = Conn::from(cfg, None);
        let v = c.vars(log).await?;
        let dbs = c.query("SELECT table_schema, ROUND(SUM(data_length+index_length)) FROM information_schema.tables WHERE table_schema NOT IN ('information_schema','performance_schema','sys') GROUP BY table_schema", log).await.unwrap_or_default();
        let phys = c.prog(&["mariabackup", "mariadb-backup", "xtrabackup"]).await.ok();
        Ok(json!({
            "version": v.get("version"), "flavor": if is_mariadb(&v) { "MariaDB" } else { "MySQL" },
            "datadir": v.get("datadir"), "log_bin": v.get("log_bin").map(|s| s == "ON" || s == "1"), "gtid_mode": v.get("gtid_mode"),
            "databases": dbs.iter().map(|r| json!({ "name": r[0], "bytes": r.get(1).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as u64 })).collect::<Vec<_>>(),
            "physical_tool": phys, "service": service_name(cfg),
        }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let c = Conn::from(&ctx.config, None);
        let v = c.vars(log).await.context("подключение к серверу MySQL/MariaDB")?;
        let ver = v.get("version").cloned().unwrap_or_default();
        let log_bin = v.get("log_bin").map(|s| s == "ON" || s == "1").unwrap_or(false);
        log(&format!("{} {ver} · datadir {} · binlog {}", if is_mariadb(&v) { "MariaDB" } else { "MySQL" }, v.get("datadir").cloned().unwrap_or_default(), if log_bin { "включён" } else { "выключен" }));
        if ctx.kind == "incr" || ctx.kind == "binlog" { return binlog_backup(ctx, &c, &v).await; }

        let dbs = crate::connectors::net::p_list(&ctx.config, "databases");
        let mut method = cfg_str(&ctx.config, "method").unwrap_or_else(|| "auto".into());
        let phys_tool = if is_mariadb(&v) { c.prog(&["mariabackup", "mariadb-backup"]).await.ok() } else { c.prog(&["xtrabackup"]).await.ok() };
        if method == "auto" {
            let size = db_size(&c, log).await;
            method = if size > 20 << 30 && phys_tool.is_some() && dbs.is_empty() { "physical".into() } else { "dump".into() };
            log(&format!("объём баз {} → {}", crate::util::human_bytes(size), if method == "dump" { "логический дамп" } else { "физическая копия" }));
        }
        let start = if log_bin { c.current_binlog(log).await } else { None };
        let mut w = ctx.repo.writer();
        let df = c.defaults_file()?;
        let mut pos = None;
        let kind;
        if method == "physical" {
            let tool = phys_tool.ok_or_else(|| anyhow!("нет {} — установите её или выберите логический дамп", if is_mariadb(&v) { "mariabackup" } else { "xtrabackup (Percona XtraBackup той же версии, что и сервер)" }))?;
            if !dbs.is_empty() { bail!("физическая копия — всего сервера; для отдельных баз выберите логический дамп"); }
            let tmp = if c.container.is_some() { "/tmp/hopebackup-xb".to_string() } else { WorkDir::new(None, None)?.path() };
            let cmd = c.cmd(&tool, &df).args(["--backup", "--stream=xbstream", &format!("--target-dir={tmp}")]);
            kit::stream_cmd(&mut w, "physical.xbstream", &cmd, log).await?;
            if c.container.is_none() { let _ = std::fs::remove_dir_all(&tmp); }
            kind = "physical";
        } else {
            let dump = c.prog(&["mariadb-dump", "mysqldump"]).await?;
            let help = kit::output(&Cmd::new(&dump).arg("--help").container(c.container.clone()), log).await.unwrap_or_default();
            let mut cmd = c.cmd(&dump, &df).args(["--single-transaction", "--quick", "--routines", "--events", "--triggers", "--hex-blob", "--default-character-set=utf8mb4"]);
            if log_bin {
                cmd = cmd.arg(if help.contains("--source-data") { "--source-data=2" } else { "--master-data=2" });
            }
            if v.get("gtid_mode").map(|s| s == "ON").unwrap_or(false) && help.contains("--set-gtid-purged") { cmd = cmd.arg("--set-gtid-purged=OFF"); }
            cmd = if dbs.is_empty() { cmd.arg("--all-databases") } else { cmd.arg("--databases").args(dbs.clone()) };
            let (_, head) = kit::stream_cmd_head(&mut w, "dump.sql", &cmd, 256 << 10, log).await?;
            pos = dump_binlog_pos(&String::from_utf8_lossy(&head));
            if let Some((f, p)) = &pos { log(&format!("позиция binlog копии: {f}:{p}")); }
            kind = "dump";
        }
        let meta = json!({
            "version": ver, "flavor": if is_mariadb(&v) { "mariadb" } else { "mysql" }, "method": kind, "databases": dbs,
            "datadir": v.get("datadir"), "log_bin": log_bin,
            "binlog": pos.as_ref().map(|(f, p)| json!({ "file": f, "pos": p })),
            // С какого файла начинается архив журнала для этой копии.
            "binlog_next": pos.as_ref().map(|(f, _)| f.clone()).or_else(|| start.as_ref().map(|(f, _)| f.clone())),
        });
        let (header, stats) = w.finish(&ctx.instance, "mysql", kind, json!({ "version": ver, "binlog_next": meta["binlog_next"] }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (h, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let snaps = kit::snapshots_of(&ctx.repo, &ctx.instance).await?;
        // Копия журнала — восстанавливаем последнюю полную до неё и прокручиваем binlog до неё.
        let (base_h, base_m, upto) = if h.kind == "binlog" {
            let b = snaps.iter().rev().find(|s| s.time <= h.time && (s.kind == "dump" || s.kind == "physical")).ok_or_else(|| anyhow!("нет полной копии раньше выбранной копии журнала"))?;
            let (bh, bm) = ctx.repo.manifest(&b.id).await?;
            (bh, bm, Some(h.time))
        } else { (h.clone(), m.clone(), None) };
        let to_time = cfg_str(&ctx.params, "to_time").map(|s| kit::parse_time(&s)).transpose()?;
        let target = Conn::from(&ctx.params, None);
        log(&format!("основа: копия {} ({}, {})", base_h.id, if base_h.kind == "dump" { "дамп" } else { "физическая" }, base_h.time.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M:%S")));
        let mut pos: Option<(String, u64)> = base_m.meta["binlog"]["file"].as_str().map(|f| (f.to_string(), base_m.meta["binlog"]["pos"].as_u64().unwrap_or(4)));
        let mut res = json!({ "base": base_h.id });
        if base_h.kind == "dump" {
            let s = kit::stream(&base_m, "dump.sql")?;
            let df = target.defaults_file()?;
            let cmd = target.cmd(&target.client().await?, &df);
            match cfg_str(&ctx.params, "database") {
                Some(db) => {
                    let rename = cfg_str(&ctx.params, "rename_to");
                    log(&format!("база {db}{}", rename.as_ref().map(|r| format!(" → {r}")).unwrap_or_default()));
                    let (d2, r2) = (db.clone(), rename.clone());
                    kit::feed_filtered(&ctx.repo, s, move |r| Box::new(DumpFilter::new(std::io::BufReader::with_capacity(1 << 20, r), &d2, r2)), &cmd, log).await?;
                    res["database"] = json!(rename.unwrap_or(db));
                }
                None => kit::feed(&ctx.repo, s, &cmd, log).await?,
            }
            log("дамп загружен");
        } else {
            let (p, dir) = physical_restore(ctx, &base_m, log).await?;
            if p.is_some() { pos = p; }
            res["datadir"] = json!(dir);
            if cfg_str(&ctx.params, "target_dir").is_some() {
                log("физическая копия подготовлена в каталоге; сервер не изменялся");
                return Ok(res);
            }
        }
        // Прокрутка binlog: до выбранной копии журнала и/или до момента.
        let stop = match (upto, to_time) { (Some(a), Some(b)) => Some(a.min(b)), (a, b) => b.or(a) };
        if let Some(stop_at) = stop {
            let n = replay_binlogs(ctx, &snaps, &base_h, pos, upto, to_time, &target, log).await?;
            res["binlog_files"] = json!(n);
            res["to_time"] = json!(stop_at);
        }
        Ok(res)
    }
}

/// Копия журнала: закрыть текущий файл binlog и забрать закрытые файлы, которых ещё нет в архиве.
async fn binlog_backup(ctx: &BackupCtx, c: &Conn, v: &std::collections::HashMap<String, String>) -> Result<BackupOutcome> {
    let log = &ctx.log;
    if !v.get("log_bin").map(|s| s == "ON" || s == "1").unwrap_or(false) { bail!("binlog выключен (log_bin = OFF): включите log_bin в конфигурации сервера, иначе восстановление на момент невозможно"); }
    let snaps = kit::snapshots_of(&ctx.repo, &ctx.instance).await?;
    let last = snaps.iter().rev().find(|s| s.extra.get("binlog_next").and_then(|x| x.as_str()).is_some());
    let Some(from) = last.and_then(|s| s.extra["binlog_next"].as_str().map(|x| x.to_string())) else { bail!("сначала нужна полная копия: с неё начинается архив binlog") };
    c.query("FLUSH BINARY LOGS", log).await.context("FLUSH BINARY LOGS (нужна привилегия RELOAD)")?;
    let list = c.query("SHOW BINARY LOGS", log).await.context("SHOW BINARY LOGS (нужна привилегия REPLICATION CLIENT)")?;
    let names: Vec<String> = list.iter().filter_map(|r| r.first().cloned()).collect();
    let Some(current) = names.last().cloned() else { bail!("сервер не вернул список binlog") };
    let gap = !names.iter().any(|n| *n == from);
    if gap { log(&format!("! файл {from} уже удалён с сервера (expire_logs_days / binlog_expire_logs_seconds): в цепочке журнала разрыв — сделайте полную копию")); }
    let take: Vec<String> = names.iter().filter(|n| **n != current && binlog_seq(n) >= binlog_seq(&from)).cloned().collect();
    log(&format!("архив binlog: {} файл(ов) с {from}, текущий {current}", take.len()));
    let mut w = ctx.repo.writer();
    let mb = c.prog(&["mariadb-binlog", "mysqlbinlog"]).await?;
    if !take.is_empty() {
        let df = c.defaults_file()?;
        match &c.container {
            None => {
                let wd = WorkDir::new(None, None)?;
                let cmd = c.cmd(&mb, &df).args(["--read-from-remote-server", "--raw", &format!("--result-file={}/", wd.path())]).args(take.clone());
                kit::run(&cmd, log).await.context("чтение binlog с сервера (нужна привилегия REPLICATION SLAVE)")?;
                for n in &take {
                    let f = std::fs::File::open(wd.join(n)).with_context(|| format!("файл {n}"))?;
                    let e = w.add_stream(&format!("binlog/{n}"), f, None).await?;
                    log(&format!("  {n}: {}", crate::util::human_bytes(e.size)));
                }
            }
            Some(ct) => {
                let dir = format!("/tmp/hopebackup-binlog-{}", crate::util::short_id());
                let ct = Some(ct.clone());
                kit::run(&Cmd::new("mkdir").args(["-p", &dir]).container(ct.clone()), log).await?;
                let r = async {
                    kit::run(&c.cmd(&mb, &None).args(["--read-from-remote-server", "--raw", &format!("--result-file={dir}/")]).args(take.clone()), log).await?;
                    for n in &take { kit::stream_cmd(&mut w, &format!("binlog/{n}"), &Cmd::new("cat").arg(format!("{dir}/{n}")).container(ct.clone()), log).await?; }
                    Ok::<_, anyhow::Error>(())
                }.await;
                let _ = kit::run(&Cmd::new("rm").args(["-rf", &dir]).container(ct.clone()), log).await;
                r?;
            }
        }
    }
    let meta = json!({ "method": "binlog", "files": take, "from": from, "binlog_next": current, "gap": gap });
    let (header, stats) = w.finish(&ctx.instance, "mysql", "binlog", json!({ "binlog_next": current, "files": meta["files"] }), meta).await?;
    Ok(BackupOutcome { header, stats })
}

/// Физическое восстановление: распаковать xbstream, prepare, заменить каталог данных. Возвращает позицию binlog копии.
async fn physical_restore(ctx: &RestoreCtx, m: &crate::store::types::Manifest, log: &LogFn) -> Result<(Option<(String, u64)>, String)> {
    let s = kit::stream(m, "physical.xbstream")?;
    let mariadb = m.meta["flavor"] == "mariadb";
    let (tool, unpack) = if mariadb { (kit::which_any(&["mariabackup", "mariadb-backup"]), kit::which_any(&["mbstream"])) } else { (kit::which_any(&["xtrabackup"]), kit::which_any(&["xbstream"])) };
    let tool = tool.ok_or_else(|| anyhow!("для физического восстановления нужна {} на этой машине", if mariadb { "mariabackup" } else { "xtrabackup" }))?;
    let unpack = unpack.ok_or_else(|| anyhow!("нет {} для распаковки", if mariadb { "mbstream" } else { "xbstream" }))?;
    let target_dir = cfg_str(&ctx.params, "target_dir");
    let datadir = m.meta["datadir"].as_str().unwrap_or("/var/lib/mysql").trim_end_matches('/').to_string();
    let work = match &target_dir {
        Some(d) => { std::fs::create_dir_all(d)?; if std::fs::read_dir(d)?.next().is_some() { bail!("каталог {d} не пуст"); } d.clone() }
        None => {
            let parent = Path::new(&datadir).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/var/lib".into());
            let d = format!("{parent}/.hopebackup-restore-{}", crate::util::short_id());
            std::fs::create_dir_all(&d)?;
            d
        }
    };
    let unpack_cmd = Cmd::new(&unpack).args(["-x", "-C", &work]);
    kit::feed(&ctx.repo, s, &unpack_cmd, log).await?;
    kit::run(&Cmd::new(&tool).args(["--prepare", &format!("--target-dir={work}")]), log).await.context("prepare физической копии")?;
    let info = std::fs::read_to_string(Path::new(&work).join(if mariadb { "mariadb_backup_binlog_info" } else { "xtrabackup_binlog_info" }))
        .or_else(|_| std::fs::read_to_string(Path::new(&work).join("xtrabackup_binlog_info"))).unwrap_or_default();
    let pos = info.split_whitespace().collect::<Vec<_>>();
    let pos = (pos.len() >= 2).then(|| (pos[0].to_string(), pos[1].parse().unwrap_or(4)));
    if target_dir.is_some() { return Ok((pos, work)); }
    // На место: остановить сервер, прежний каталог данных — рядом, перенести подготовленный.
    let service = service_name(&ctx.params).ok_or_else(|| anyhow!("не найдена служба сервера — укажите «Служба»"))?;
    if svc::active(&service) { svc::stop(&service, log).await?; }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let aside = kit::aside_name(&datadir, &ts.to_string());
    if Path::new(&datadir).exists() {
        std::fs::rename(&datadir, &aside).with_context(|| format!("перенос {datadir} → {aside}"))?;
        log(&format!("прежний каталог данных → {aside}"));
    }
    std::fs::create_dir_all(&datadir)?;
    let r = kit::run(&Cmd::new(&tool).args(["--move-back", &format!("--target-dir={work}"), &format!("--datadir={datadir}")]), log).await;
    let _ = std::fs::remove_dir_all(&work);
    r.context("перенос файлов в каталог данных")?;
    if crate::util::user_ids("mysql").is_some() { kit::chown_to(&[datadir.clone()], "mysql", log)?; }
    if kit::bool_param(&ctx.params, "start", true) {
        svc::start(&service, log).await?;
        // Сервер принимает подключения не сразу после запуска службы.
        let c = Conn::from(&ctx.params, None);
        for _ in 0..30 { if c.query("SELECT 1", log).await.is_ok() { break; } tokio::time::sleep(std::time::Duration::from_secs(1)).await; }
    }
    Ok((pos, datadir))
}

/// Прокрутить binlog из архива: от позиции основы до `upto` (копия журнала) и/или `to_time`.
#[allow(clippy::too_many_arguments)]
async fn replay_binlogs(ctx: &RestoreCtx, snaps: &[crate::store::types::SnapshotHeader], base: &crate::store::types::SnapshotHeader, pos: Option<(String, u64)>,
    upto: Option<chrono::DateTime<chrono::Utc>>, to_time: Option<chrono::DateTime<chrono::Utc>>, target: &Conn, log: &LogFn) -> Result<usize> {
    let log = log.clone();
    let Some((start_file, start_pos)) = pos else { bail!("в копии нет позиции binlog (при копии binlog был выключен) — прокрутка невозможна") };
    // Копии журнала после основы — до той, что покрывает момент (её время ≥ момента).
    let mut chain = Vec::new();
    for s in snaps.iter().filter(|s| s.kind == "binlog" && s.time > base.time) {
        if let Some(u) = upto { if s.time > u { break; } }
        chain.push(s.clone());
        if let Some(t) = to_time { if s.time >= t { break; } }
    }
    if chain.is_empty() { bail!("нет копий журнала после основы — момент позже копии не достижим"); }
    if let Some(t) = to_time { if chain.last().map(|s| s.time < t).unwrap_or(true) && upto.is_none() { log(&format!("! архив binlog заканчивается {} — раньше запрошенного момента", chain.last().unwrap().time.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M:%S"))); } }
    let wd = WorkDir::new(None, None)?;
    let mut files: Vec<String> = Vec::new();
    for s in &chain {
        let (_, m) = ctx.repo.manifest(&s.id).await?;
        if m.meta["gap"] == true { log(&format!("! в копии журнала {} отмечен разрыв цепочки", s.id)); }
        for e in m.streams.iter().filter(|e| e.name.starts_with("binlog/")) {
            let n = e.name.trim_start_matches("binlog/").to_string();
            if binlog_seq(&n) < binlog_seq(&start_file) || files.contains(&n) { continue; }
            kit::to_file(&ctx.repo, e, &wd.join(&n)).await?;
            files.push(n);
        }
    }
    files.sort_by_key(|n| binlog_seq(n));
    if files.first().map(|f| *f != start_file).unwrap_or(true) { bail!("в архиве нет файла {start_file}, с которого начинается журнал после копии"); }
    let mb = kit::which_any(&["mariadb-binlog", "mysqlbinlog"]).ok_or_else(|| anyhow!("нет mysqlbinlog на этой машине"))?;
    let mut args = vec![format!("--start-position={start_pos}")];
    if let Some(t) = to_time { args.push(format!("--stop-datetime={}", t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"))); }
    args.extend(files.iter().map(|f| wd.join(f).to_string_lossy().to_string()));
    log(&format!("прокрутка binlog: {} от {start_file}:{start_pos}{}", files.join(", "), to_time.map(|t| format!(" до {}", t.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M:%S"))).unwrap_or_default()));
    let df = target.defaults_file()?;
    let client = target.cmd(&target.client().await?, &df);
    let n = files.len();
    let (mb2, log2) = (mb.clone(), log.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::process::Stdio;
        let mut a = std::process::Command::new(&mb2).args(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("mysqlbinlog")?;
        let mut b = client.std(true).stdin(Stdio::from(a.stdout.take().unwrap())).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("клиент mysql")?;
        let mut ea = String::new();
        let mut eb = String::new();
        let _ = a.stderr.take().unwrap().read_to_string(&mut ea);
        let _ = b.stderr.take().unwrap().read_to_string(&mut eb);
        let (sa, sb) = (a.wait()?, b.wait()?);
        if !sa.success() { bail!("mysqlbinlog: {}", ea.trim()); }
        if !sb.success() { bail!("применение binlog: {}", eb.trim()); }
        if !eb.trim().is_empty() { log2(&format!("  {}", eb.trim())); }
        Ok(())
    }).await??;
    log(&format!("журнал применён ({n} файл(ов))"));
    Ok(n)
}

pub fn restore_keys() -> &'static [&'static str] { &["host", "port", "socket", "user", "password", "container", "service"] }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binlog_pos_and_filter() {
        let h = "-- MariaDB dump\n--\n-- Position to start replication or point-in-time recovery from\n--\n\n-- CHANGE MASTER TO MASTER_LOG_FILE='mysql-bin.000003', MASTER_LOG_POS=385;\n";
        assert_eq!(dump_binlog_pos(h), Some(("mysql-bin.000003".into(), 385)));
        assert_eq!(dump_binlog_pos("-- CHANGE REPLICATION SOURCE TO SOURCE_LOG_FILE='binlog.000012', SOURCE_LOG_POS=157;"), Some(("binlog.000012".into(), 157)));
        assert_eq!(binlog_seq("mysql-bin.000012"), 12);
        let dump = "SET NAMES utf8mb4;\n--\n-- Current Database: `a`\n--\n\nCREATE DATABASE /*!32312 IF NOT EXISTS*/ `a`;\nUSE `a`;\nINSERT a;\n--\n-- Current Database: `b`\n--\nCREATE DATABASE `b`;\nUSE `b`;\nINSERT b;\n";
        let mut out = String::new();
        DumpFilter::new(std::io::Cursor::new(dump), "b", Some("b_copy".into())).read_to_string(&mut out).unwrap();
        assert!(out.starts_with("SET NAMES"));
        assert!(!out.contains("INSERT a"));
        assert!(out.contains("USE `b_copy`;") && out.contains("CREATE DATABASE `b_copy`;") && out.contains("INSERT b"));
    }
}
