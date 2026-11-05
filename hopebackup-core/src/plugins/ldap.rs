//! Каталоги LDAP: OpenLDAP (slapcat конфигурации cn=config и каждой базы в LDIF; восстановление —
//! slapadd в чистые каталоги, прежние остаются рядом) и FreeIPA (штатный ipa-backup — полная копия или
//! данные на ходу; восстановление — ipa-restore).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, svc, Cmd};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Ldap;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "mode": { "type": "string", "title": "Каталог", "enum": ["auto", "openldap", "freeipa"], "default": "auto" },
        "config_dir": { "type": "string", "title": "OpenLDAP: каталог cn=config", "description": "Пусто — /etc/ldap/slapd.d или /etc/openldap/slapd.d" },
        "service": { "type": "string", "title": "OpenLDAP: служба", "description": "Пусто — slapd" },
        "ipa_backup": { "type": "string", "title": "FreeIPA: вид копии", "enum": ["data", "full"], "default": "data",
            "description": "data — данные каталога на ходу (ipa-backup --data --online); full — сервер целиком, службы IPA останавливаются на время копии" }
    } })
}

pub fn defaults() -> Value { json!({ "mode": "auto", "ipa_backup": "data" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "databases": { "type": "array", "items": { "type": "string" }, "title": "OpenLDAP: базы (суффиксы)", "description": "Пусто — все базы копии" },
        "restore_config": { "type": "boolean", "title": "OpenLDAP: восстановить cn=config", "default": false, "description": "Схемы, ACL, модули, параметры баз; нужно на новой машине" },
        "target_dir": { "type": "string", "title": "Только файлы LDIF в каталог", "description": "Сервер не трогать" },
        "dm_password": { "type": "string", "format": "secret", "title": "FreeIPA: пароль Directory Manager", "description": "Для ipa-restore" },
        "start": { "type": "boolean", "title": "Запустить службу после восстановления", "default": true }
    } })
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Db { n: i64, kind: String, suffix: String, dir: String }

fn conf_dir(cfg: &Value) -> Option<String> {
    cfg_str(cfg, "config_dir").or_else(|| ["/etc/ldap/slapd.d", "/etc/openldap/slapd.d"].iter().find(|d| Path::new(d).join("cn=config").is_dir()).map(|s| s.to_string()))
}

fn mode(cfg: &Value) -> &'static str {
    match cfg_str(cfg, "mode").as_deref() {
        Some("freeipa") => "freeipa",
        Some("openldap") => "openldap",
        _ => if kit::which("ipa-backup").is_some() && Path::new("/etc/ipa/default.conf").exists() { "freeipa" } else { "openldap" },
    }
}

/// Базы из cn=config: olcDatabase={N}mdb.ldif → номер, тип, суффикс, каталог.
fn databases(conf: &str) -> Vec<Db> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(Path::new(conf).join("cn=config")) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("olcDatabase={").and_then(|r| r.strip_suffix(".ldif")) else { continue };
        let Some((n, kind)) = rest.split_once('}') else { continue };
        let Ok(n) = n.parse::<i64>() else { continue };
        if n < 1 || !["mdb", "hdb", "bdb"].contains(&kind) { continue; }
        let t = std::fs::read_to_string(e.path()).unwrap_or_default();
        let v = |k: &str| t.lines().find_map(|l| l.strip_prefix(&format!("{k}: ")).map(|x| x.trim().to_string())).unwrap_or_default();
        out.push(Db { n, kind: kind.into(), suffix: v("olcSuffix"), dir: v("olcDbDirectory") });
    }
    out.sort_by_key(|d| d.n);
    out
}

fn slapd_user() -> String {
    let d = std::fs::read_to_string("/etc/default/slapd").unwrap_or_default();
    d.lines().find_map(|l| l.trim().strip_prefix("SLAPD_USER=").map(|v| v.trim_matches('"').to_string())).filter(|v| !v.is_empty())
        .or_else(|| ["openldap", "ldap"].iter().find(|u| crate::util::user_ids(u).is_some()).map(|s| s.to_string())).unwrap_or_else(|| "root".into())
}

fn service(cfg: &Value) -> String { cfg_str(cfg, "service").unwrap_or_else(|| "slapd".into()) }

#[async_trait]
impl Plugin for Ldap {
    fn id(&self) -> &'static str { "freeipa" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        if mode(cfg) == "freeipa" { return Ok(json!({ "mode": "FreeIPA", "realm": std::fs::read_to_string("/etc/ipa/default.conf").unwrap_or_default().lines().find_map(|l| l.strip_prefix("realm = ").map(|x| x.to_string())) })); }
        let conf = conf_dir(cfg).ok_or_else(|| anyhow!("не найден каталог cn=config (slapd.d)"))?;
        Ok(json!({ "mode": "OpenLDAP", "config_dir": conf, "databases": databases(&conf), "service": service(cfg), "active": svc::active(&service(cfg)) }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let mut w = ctx.repo.writer();
        if mode(&ctx.config) == "freeipa" {
            let full = cfg_str(&ctx.config, "ipa_backup").as_deref() == Some("full");
            let base = Path::new("/var/lib/ipa/backup");
            let before: Vec<String> = std::fs::read_dir(base).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
            let c = if full { Cmd::new("ipa-backup") } else { Cmd::new("ipa-backup").args(["--data", "--online"]) };
            if full { log("! полная копия FreeIPA останавливает службы IPA на время копии"); }
            kit::run(&c, log).await.context("ipa-backup")?;
            let new = std::fs::read_dir(base)?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !before.contains(n)).max().ok_or_else(|| anyhow!("ipa-backup не создал каталог в {}", base.display()))?;
            let dir = base.join(&new).to_string_lossy().to_string();
            let r = kit::tar_paths(&mut w, "ipa-backup.tar", &[dir.clone()], Default::default(), Default::default(), log).await;
            let _ = std::fs::remove_dir_all(&dir);
            r?;
            let meta = json!({ "mode": "freeipa", "name": new, "full": full });
            let (header, stats) = w.finish(&ctx.instance, "freeipa", "full", json!({ "mode": "freeipa", "full": full }), meta).await?;
            return Ok(BackupOutcome { header, stats });
        }
        let conf = conf_dir(&ctx.config).ok_or_else(|| anyhow!("не найден каталог cn=config (slapd.d) — укажите его"))?;
        let dbs = databases(&conf);
        if dbs.is_empty() { bail!("в {conf} нет баз mdb/hdb"); }
        log(&format!("OpenLDAP · {conf} · базы: {}", dbs.iter().map(|d| format!("{} ({})", d.suffix, d.kind)).collect::<Vec<_>>().join(", ")));
        if dbs.iter().any(|d| d.kind != "mdb") && svc::active(&service(&ctx.config)) { log("! базы hdb/bdb при работающем slapd могут выгрузиться несогласованно — переведите их на mdb"); }
        kit::stream_cmd(&mut w, "config.ldif", &Cmd::new("slapcat").args(["-F", &conf, "-n", "0"]), log).await.context("slapcat cn=config")?;
        for d in &dbs { kit::stream_cmd(&mut w, &format!("db-{}.ldif", d.n), &Cmd::new("slapcat").args(["-F", &conf, "-n", &d.n.to_string()]), log).await.with_context(|| format!("slapcat {}", d.suffix))?; }
        let etc = Path::new(&conf).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/etc/ldap".into());
        kit::tar_paths(&mut w, "etc.tar", &[etc], Default::default(), Default::default(), log).await?;
        let meta = json!({ "mode": "openldap", "config_dir": conf, "databases": dbs, "user": slapd_user() });
        let (header, stats) = w.finish(&ctx.instance, "freeipa", "full", json!({ "mode": "openldap", "databases": dbs.len() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        if m.meta["mode"] == "freeipa" {
            let name = m.meta["name"].as_str().unwrap_or("ipa-backup").to_string();
            if let Some(d) = cfg_str(&ctx.params, "target_dir") {
                kit::untar(&ctx.repo, kit::stream(&m, "ipa-backup.tar")?, Path::new(&d), Default::default(), log).await?;
                return Ok(json!({ "target_dir": d }));
            }
            let pw = cfg_str(&ctx.params, "dm_password").ok_or_else(|| anyhow!("укажите пароль Directory Manager для ipa-restore"))?;
            kit::untar(&ctx.repo, kit::stream(&m, "ipa-backup.tar")?, Path::new("/"), Default::default(), log).await?;
            let dir = format!("/var/lib/ipa/backup/{name}");
            // ipa-restore принимает пароль только аргументом; в журнал задания он не попадает.
            let r = kit::output(&Cmd::new("ipa-restore").args(["--unattended", "--password", &pw, &dir]), log).await;
            let _ = std::fs::remove_dir_all(&dir);
            let out = r.context("ipa-restore")?;
            for l in out.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev() { log(&format!("  {l}")); }
            return Ok(json!({ "restored": name }));
        }
        let conf = cfg_str(&ctx.params, "config_dir").or_else(|| m.meta["config_dir"].as_str().map(|s| s.to_string())).unwrap_or_else(|| "/etc/ldap/slapd.d".into());
        let dbs: Vec<Db> = serde_json::from_value(m.meta["databases"].clone()).unwrap_or_default();
        let want = crate::connectors::net::p_list(&ctx.params, "databases");
        let sel: Vec<&Db> = dbs.iter().filter(|d| want.is_empty() || want.iter().any(|w| w.eq_ignore_ascii_case(&d.suffix))).collect();
        if sel.is_empty() { bail!("нет таких баз в копии: есть {}", dbs.iter().map(|d| d.suffix.as_str()).collect::<Vec<_>>().join(", ")); }
        if let Some(d) = cfg_str(&ctx.params, "target_dir") {
            std::fs::create_dir_all(&d)?;
            kit::to_file(&ctx.repo, kit::stream(&m, "config.ldif")?, &Path::new(&d).join("config.ldif")).await?;
            for x in &sel { kit::to_file(&ctx.repo, kit::stream(&m, &format!("db-{}.ldif", x.n))?, &Path::new(&d).join(format!("db-{}.ldif", x.n))).await?; }
            log(&format!("LDIF → {d}"));
            return Ok(json!({ "target_dir": d }));
        }
        let user = m.meta["user"].as_str().map(|s| s.to_string()).filter(|u| crate::util::user_ids(u).is_some()).unwrap_or_else(slapd_user);
        let service = service(&ctx.params);
        if svc::active(&service) { svc::stop(&service, log).await?; }
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut moved = Vec::new();
        let wd = kit::WorkDir::new(None, None)?;
        if kit::bool_param(&ctx.params, "restore_config", false) {
            let f = wd.join("config.ldif");
            kit::to_file(&ctx.repo, kit::stream(&m, "config.ldif")?, &f).await?;
            if Path::new(&conf).exists() { let a = kit::aside_name(&conf, &ts); std::fs::rename(&conf, &a)?; moved.push(a); }
            std::fs::create_dir_all(&conf)?;
            kit::run(&Cmd::new("slapadd").args(["-F", &conf, "-n", "0", "-l", &f.to_string_lossy()]), log).await.context("slapadd cn=config")?;
            kit::chown_to(&[conf.clone()], &user, log)?;
        }
        for d in &sel {
            let f = wd.join(&format!("db-{}.ldif", d.n));
            kit::to_file(&ctx.repo, kit::stream(&m, &format!("db-{}.ldif", d.n))?, &f).await?;
            let dir = kit::safe_abs(&d.dir)?;
            if Path::new(&dir).exists() { let a = kit::aside_name(&dir, &ts); std::fs::rename(&dir, &a)?; log(&format!("  прежняя база {} → {a}", d.suffix)); moved.push(a); }
            std::fs::create_dir_all(&dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
            kit::run(&Cmd::new("slapadd").args(["-q", "-F", &conf, "-n", &d.n.to_string(), "-l", &f.to_string_lossy()]), log).await.with_context(|| format!("slapadd {}", d.suffix))?;
            kit::chown_to(&[dir.clone()], &user, log)?;
        }
        let mut active = Value::Null;
        if kit::bool_param(&ctx.params, "start", true) && svc::exists(&service) {
            svc::start(&service, log).await?;
            active = json!(svc::active(&service));
        }
        Ok(json!({ "databases": sel.iter().map(|d| d.suffix.clone()).collect::<Vec<_>>(), "moved_aside": moved, "active": active }))
    }
}
