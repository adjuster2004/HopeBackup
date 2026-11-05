//! MongoDB: mongodump одним архивом (--archive) прямо в репозиторий; у набора реплик — с --oplog
//! (согласованный срез на момент окончания копии). Пароль — в файле параметров (--config), не в
//! командной строке. Восстановление: mongorestore --drop (всё или одна база, по желанию под другим
//! именем), --oplogReplay для полной копии набора реплик.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::appkit::{self as kit, Cmd, Secret};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Mongo;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "uri": { "type": "string", "title": "Адрес (URI)", "default": "mongodb://127.0.0.1:27017", "description": "Без пароля: mongodb://host1,host2/?replicaSet=rs0. Лучше — на вторичный узел (readPreference=secondary)" },
        "user": { "type": "string", "title": "Пользователь", "description": "Роль backup (для восстановления — restore) в базе admin" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "auth_db": { "type": "string", "title": "База аутентификации", "default": "admin" },
        "tls": { "type": "boolean", "title": "TLS", "default": false },
        "tls_insecure": { "type": "boolean", "title": "Не проверять сертификат", "default": false },
        "container": { "type": "string", "title": "Контейнер Docker" },
        "databases": { "type": "array", "items": { "type": "string" }, "title": "База", "description": "Одна база; пусто — все" },
        "oplog": { "type": "string", "title": "Срез oplog", "enum": ["auto", "on", "off"], "default": "auto", "description": "on — набор реплик: копия согласована на момент окончания; auto — если в адресе replicaSet или mongosh показывает набор реплик" }
    } })
}

pub fn defaults() -> Value { json!({ "uri": "mongodb://127.0.0.1:27017", "auth_db": "admin", "oplog": "auto" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "database": { "type": "string", "title": "Только база", "description": "Пусто — всё из копии" },
        "rename_to": { "type": "string", "title": "Под именем", "description": "Восстановить базу под другим именем (рядом с рабочей)" },
        "drop": { "type": "boolean", "title": "Удалить коллекции перед восстановлением (--drop)", "default": true },
        "uri": { "type": "string", "title": "Целевой сервер (URI)", "description": "Пусто — сервер экземпляра" },
        "user": { "type": "string", "title": "Пользователь" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["uri", "user", "password", "auth_db", "tls", "tls_insecure", "container"] }

struct Conn { uri: String, user: String, password: String, auth_db: String, tls: bool, insecure: bool, container: Option<String> }

impl Conn {
    fn from(v: &Value) -> Conn {
        Conn {
            uri: cfg_str(v, "uri").unwrap_or_else(|| "mongodb://127.0.0.1:27017".into()),
            user: cfg_str(v, "user").unwrap_or_default(),
            password: cfg_str(v, "password").unwrap_or_default(),
            auth_db: cfg_str(v, "auth_db").unwrap_or_else(|| "admin".into()),
            tls: kit::bool_param(v, "tls", false),
            insecure: kit::bool_param(v, "tls_insecure", false),
            container: cfg_str(v, "container"),
        }
    }
    /// Параметры утилит: адрес и пароль — в YAML (--config), остальное — аргументами.
    async fn tool(&self, prog: &str, log: &LogFn) -> Result<(Cmd, Secret)> {
        let p = kit::find_prog(self.container.as_deref(), &[prog]).await.ok_or_else(|| anyhow!("нет {prog} — установите MongoDB Database Tools"))?;
        let mut y = format!("uri: \"{}\"\n", self.uri.replace('"', "\\\""));
        if !self.password.is_empty() { y.push_str(&format!("password: \"{}\"\n", self.password.replace('\\', "\\\\").replace('"', "\\\""))); }
        let sec = Secret::new(self.container.as_deref(), &y, log).await?;
        let mut c = Cmd::new(p).container(self.container.clone()).arg(format!("--config={}", sec.path()));
        if !self.user.is_empty() { c = c.arg(format!("--username={}", self.user)).arg(format!("--authenticationDatabase={}", self.auth_db)); }
        if self.tls { c = c.arg("--ssl"); if self.insecure { c = c.arg("--tlsInsecure"); } }
        Ok((c, sec))
    }
    /// Набор реплик? (по адресу или через mongosh).
    async fn replica_set(&self, log: &LogFn) -> bool {
        if self.uri.contains("replicaSet=") { return true; }
        let Some(sh) = kit::find_prog(self.container.as_deref(), &["mongosh", "mongo"]).await else { return false };
        // Вход — в самом сценарии (файл 0600), чтобы пароль не попал в командную строку.
        let js_s = |s: &str| serde_json::to_string(s).unwrap_or_default();
        let mut js = String::new();
        if !self.user.is_empty() { js.push_str(&format!("db.getSiblingDB({}).auth({}, {});\n", js_s(&self.auth_db), js_s(&self.user), js_s(&self.password))); }
        js.push_str("print('RS=' + (db.hello().setName || ''));\n");
        let Ok(f) = Secret::with_ext(self.container.as_deref(), &js, ".js", log).await else { return false };
        let c = Cmd::new(sh).container(self.container.clone()).args([self.uri.as_str(), "--quiet", &f.path()]);
        kit::output(&c, log).await.map(|o| o.lines().any(|l| l.starts_with("RS=") && l.len() > 3)).unwrap_or(false)
    }
}

#[async_trait]
impl Plugin for Mongo {
    fn id(&self) -> &'static str { "mongodb" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let c = Conn::from(cfg);
        let (d, _s) = c.tool("mongodump", log).await?;
        let ver = kit::output(&Cmd::new(&d.prog).arg("--version").container(c.container.clone()), log).await.unwrap_or_default();
        Ok(json!({ "tools": ver.lines().next(), "replica_set": c.replica_set(log).await }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let c = Conn::from(&ctx.config);
        let dbs = crate::connectors::net::p_list(&ctx.config, "databases");
        if dbs.len() > 1 { bail!("mongodump копирует одну базу или все: оставьте одну базу или пусто"); }
        let oplog = match cfg_str(&ctx.config, "oplog").as_deref() { Some("on") => true, Some("off") => false, _ => dbs.is_empty() && c.replica_set(log).await };
        if oplog && !dbs.is_empty() { bail!("срез oplog — только для копии всех баз"); }
        let (mut cmd, _sec) = c.tool("mongodump", log).await?;
        cmd = cmd.arg("--archive");
        if oplog { cmd = cmd.arg("--oplog"); }
        if let Some(db) = dbs.first() { cmd = cmd.arg(format!("--db={db}")); }
        let ver = kit::output(&Cmd::new(&cmd.prog).arg("--version").container(c.container.clone()), log).await.unwrap_or_default();
        log(&format!("{} · {}", ver.lines().next().unwrap_or("mongodump"), if oplog { "набор реплик, срез oplog" } else { "без oplog" }));
        let mut w = ctx.repo.writer();
        kit::stream_cmd(&mut w, "dump.archive", &cmd, log).await.context("mongodump")?;
        let meta = json!({ "oplog": oplog, "databases": dbs, "tools": ver.lines().next() });
        let (header, stats) = w.finish(&ctx.instance, "mongodb", "full", json!({ "oplog": oplog }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let s = kit::stream(&m, "dump.archive")?;
        let c = Conn::from(&ctx.params);
        let (mut cmd, _sec) = c.tool("mongorestore", log).await?;
        cmd = cmd.arg("--archive");
        if kit::bool_param(&ctx.params, "drop", true) { cmd = cmd.arg("--drop"); }
        let db = cfg_str(&ctx.params, "database");
        let rename = cfg_str(&ctx.params, "rename_to");
        if rename.is_some() && db.is_none() { bail!("«Под именем» — для одной базы: укажите «Только база»"); }
        match (&db, &rename) {
            (Some(d), Some(r)) => { cmd = cmd.arg(format!("--nsInclude={d}.*")).arg(format!("--nsFrom={d}.*")).arg(format!("--nsTo={r}.*")); }
            (Some(d), None) => { cmd = cmd.arg(format!("--nsInclude={d}.*")); }
            _ => { if m.meta["oplog"] == true { cmd = cmd.arg("--oplogReplay"); } }
        }
        kit::feed(&ctx.repo, s, &cmd, log).await.context("mongorestore")?;
        Ok(json!({ "database": rename.or(db), "oplog_replay": m.meta["oplog"] == true && cfg_str(&ctx.params, "database").is_none() }))
    }
}
