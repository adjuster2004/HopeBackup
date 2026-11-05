//! Реестры артефактов: Sonatype Nexus Repository 3 и Harbor.
//!
//! Nexus: каталог данных sonatype-work/nexus3 (встроенная БД OrientDB/H2, настройки, ключи) — согласованно
//! (снимок ФС или короткая остановка), затем хранилище blob — на ходу (blob пишутся до ссылок на них в БД).
//! Внешняя PostgreSQL — отдельным экземпляром плагина PostgreSQL. Служба systemd или контейнер (/nexus-data).
//!
//! Harbor: база (pg_dumpall из контейнера harbor-db, без остановки), ключи /data/secret, конфигурация
//! (harbor.yml, common/config), хранилище образов /data/registry (адресуется по содержимому — копируется на
//! ходу после базы). Восстановление: остановка, файлы, база в harbor-db, запуск docker compose.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd};
use super::hope::{self, part, Layout, Part, Phase};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Nexus;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "product": { "type": "string", "title": "Продукт", "enum": ["auto", "nexus", "harbor"], "default": "auto" },
        "service": { "type": "string", "title": "Служба Nexus", "default": "nexus" },
        "container": { "type": "string", "title": "Контейнер Nexus", "description": "Вместо службы: том /nexus-data" },
        "data_dir": { "type": "string", "title": "Каталог данных Nexus", "description": "Пусто — из nexus.vmoptions (-Dkaraf.data)" },
        "blob_dirs": { "type": "array", "items": { "type": "string" }, "title": "Дополнительные хранилища blob", "description": "Каталоги хранилищ blob вне каталога данных" },
        "harbor_dir": { "type": "string", "title": "Каталог установки Harbor", "description": "Где harbor.yml и docker-compose.yml; пусто — /opt/harbor, /root/harbor, /srv/harbor" },
        "include_job_logs": { "type": "boolean", "title": "Harbor: журналы заданий", "default": false },
        "consistency": kit::consistency_schema("Nexus: при остановке служба стоит только на время копии БД и настроек.")
    } })
}

pub fn defaults() -> Value { json!({ "product": "auto", "service": "nexus", "consistency": "auto" }) }

pub fn restore_schema() -> Value {
    let mut m = hope::layout_restore_schema("data, blobs, config; Harbor: secret, registry, config, база");
    m.insert("restore_database".into(), json!({ "type": "boolean", "title": "Harbor: восстановить базу", "default": true }));
    json!({ "type": "object", "properties": Value::Object(m) })
}

fn harbor_dir(cfg: &Value) -> Option<String> {
    cfg_str(cfg, "harbor_dir").or_else(|| ["/opt/harbor", "/root/harbor", "/srv/harbor", "/data/harbor"].iter().find(|d| Path::new(d).join("harbor.yml").exists()).map(|s| s.to_string()))
}

fn product(cfg: &Value) -> &'static str {
    match cfg_str(cfg, "product").as_deref() {
        Some("harbor") => "harbor",
        Some("nexus") => "nexus",
        _ => if cfg_str(cfg, "container").is_none() && kit::unit(&cfg_str(cfg, "service").unwrap_or_else(|| "nexus".into())).is_none() && harbor_dir(cfg).is_some() { "harbor" } else { "nexus" },
    }
}

/// Значение верхнего уровня из harbor.yml (`data_volume: /data`).
fn yml(text: &str, k: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix(&format!("{k}:")).map(|v| v.split('#').next().unwrap_or("").trim().trim_matches('"').to_string())).filter(|v| !v.is_empty())
}

fn nexus_layout(cfg: &Value) -> Result<Layout> {
    let mut l = Layout::default();
    let mut config = Vec::new();
    let data = if let Some(c) = cfg_str(cfg, "container") {
        l.container = c.clone();
        l.version = kit::docker_env(&c, "NEXUS_VERSION").unwrap_or_default();
        cfg_str(cfg, "data_dir").or_else(|| kit::docker_mount(&c, "/nexus-data")).ok_or_else(|| anyhow!("у контейнера {c} нет тома /nexus-data — укажите каталог данных"))?
    } else {
        let svc = cfg_str(cfg, "service").unwrap_or_else(|| "nexus".into());
        let u = kit::unit(&svc).unwrap_or_default();
        l.service = svc.clone();
        l.user = if u.user.is_empty() { "nexus".into() } else { u.user.clone() };
        let bin = hope::first_token(&u.exec);
        let install = Path::new(if bin.is_empty() { "/opt/nexus/bin/nexus" } else { &bin }).parent().and_then(|p| p.parent()).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "/opt/nexus".into());
        let vm = std::fs::read_to_string(format!("{install}/bin/nexus.vmoptions")).unwrap_or_default();
        let karaf = vm.lines().find_map(|x| x.trim().strip_prefix("-Dkaraf.data=")).map(|x| x.to_string());
        config.extend(hope::unit_files(&[&format!("{svc}.service")]));
        for f in [format!("{install}/etc"), format!("{install}/bin/nexus.vmoptions"), format!("{install}/bin/nexus.rc")] { if Path::new(&f).exists() { config.push(f); } }
        l.version = std::fs::read_dir(Path::new(&install).join("system/org/sonatype/nexus/nexus-base")).ok().and_then(|mut r| r.next()).and_then(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).unwrap_or_default();
        cfg_str(cfg, "data_dir").unwrap_or_else(|| match karaf {
            Some(k) if k.starts_with('/') => k,
            Some(k) => { let mut p = std::path::PathBuf::from(&install); for c in Path::new(&k).components() { match c { std::path::Component::ParentDir => { p.pop(); } std::path::Component::CurDir => {} o => p.push(o) } } p.to_string_lossy().to_string() }
            None => "/opt/sonatype-work/nexus3".into(),
        })
    };
    if !Path::new(&data).exists() { bail!("каталог данных Nexus {data} не найден"); }
    let port = std::fs::read_to_string(Path::new(&data).join("etc/nexus.properties")).ok().and_then(|t| t.lines().find_map(|x| x.trim().strip_prefix("application-port=").map(|v| v.trim().to_string()))).unwrap_or_else(|| "8081".into());
    l.health = format!("http://127.0.0.1:{port}/service/rest/v1/status");
    l.parts.push(part("data", "БД, настройки, ключи (sonatype-work)", vec![data.clone()], vec![format!("{data}/blobs"), format!("{data}/log"), format!("{data}/tmp"), format!("{data}/cache"), format!("{data}/elasticsearch"), format!("{data}/generated-bundles"), format!("{data}/clean_cache"), format!("{data}/port")], Phase::Quiesced, &l.user));
    let mut blobs: Vec<String> = vec![format!("{data}/blobs")];
    blobs.extend(crate::connectors::net::p_list(cfg, "blob_dirs"));
    blobs.retain(|b| Path::new(b).exists());
    if !blobs.is_empty() {
        let mut b = part("blobs", "Хранилища blob", blobs, vec![], Phase::Live, &l.user);
        b.replace = false;
        l.parts.push(b);
    }
    if !config.is_empty() { l.parts.push(Part::config(config)); }
    let store = std::fs::read_to_string(Path::new(&data).join("etc/fabric/nexus-store.properties")).unwrap_or_default();
    if store.contains("jdbc:postgresql") { l.notes.push("Nexus работает на внешней PostgreSQL: добавьте её отдельным экземпляром плагина PostgreSQL (копия каталога данных её не содержит)".into()); }
    l.live_without_snapshot = kit::consistency(cfg) == kit::Consistency::Live;
    Ok(l)
}

fn harbor_layout(cfg: &Value) -> Result<(Layout, String, String)> {
    let dir = harbor_dir(cfg).ok_or_else(|| anyhow!("не найден каталог установки Harbor (harbor.yml) — укажите его"))?;
    let yml_text = std::fs::read_to_string(Path::new(&dir).join("harbor.yml")).context("harbor.yml")?;
    let data = yml(&yml_text, "data_volume").unwrap_or_else(|| "/data".into());
    let mut l = Layout { version: std::fs::read_to_string(Path::new(&dir).join("VERSION")).unwrap_or_default().trim().to_string(), ..Default::default() };
    let hostname = yml(&yml_text, "hostname").unwrap_or_else(|| "127.0.0.1".into());
    l.health = format!("https://{hostname}/api/v2.0/ping");
    let p = |n: &str| format!("{data}/{n}");
    let mut sec = part("secret", "Ключи и сертификаты (/data/secret)", vec![p("secret")], vec![], Phase::Live, "");
    sec.replace = true;
    l.parts.push(sec);
    let mut cfgp = Part::config(vec![format!("{dir}/harbor.yml"), format!("{dir}/docker-compose.yml"), format!("{dir}/common/config")]);
    cfgp.paths.retain(|x| Path::new(x).exists());
    l.parts.push(cfgp);
    for (n, t) in [("ca_download", "Сертификат CA для клиентов"), ("chart_storage", "Чарты Helm (ChartMuseum)")] {
        if Path::new(&p(n)).exists() { let mut x = part(n, t, vec![p(n)], vec![], Phase::Live, ""); x.replace = false; l.parts.push(x); }
    }
    if kit::bool_param(cfg, "include_job_logs", false) && Path::new(&p("job_logs")).exists() { let mut x = part("job_logs", "Журналы заданий", vec![p("job_logs")], vec![], Phase::Live, ""); x.replace = false; l.parts.push(x); }
    let mut reg = part("registry", "Хранилище образов (/data/registry)", vec![p("registry")], vec![], Phase::Live, "");
    reg.replace = false;
    if Path::new(&p("registry")).exists() { l.parts.push(reg); } else { l.notes.push("хранилище образов не в /data/registry (S3, Swift…) — копируйте его средствами хранилища".into()); }
    l.live_without_snapshot = true;
    Ok((l, dir, data))
}

fn compose(dir: &str) -> Cmd { Cmd::new("docker").args(["compose", "-f", &format!("{dir}/docker-compose.yml")]) }

#[async_trait]
impl Plugin for Nexus {
    fn id(&self) -> &'static str { "nexus" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        if product(cfg) == "harbor" {
            let (l, dir, data) = harbor_layout(cfg)?;
            return Ok(json!({ "product": "Harbor", "version": l.version, "install_dir": dir, "data_volume": data }));
        }
        let l = nexus_layout(cfg)?;
        Ok(json!({ "product": "Nexus", "version": l.version, "data_dir": l.parts[0].paths[0], "service": l.service, "container": l.container, "notes": l.notes }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        if product(&ctx.config) == "nexus" {
            let l = nexus_layout(&ctx.config)?;
            return hope::layout_backup(ctx, "nexus", "Nexus Repository", &l, json!({ "product": "nexus" })).await;
        }
        let (l, dir, _) = harbor_layout(&ctx.config)?;
        log(&format!("Harbor {} · {dir}", l.version));
        let mut w = ctx.repo.writer();
        // База — первой: хранилище образов, скопированное после неё, содержит все упомянутые в ней слои.
        let dump = Cmd::new("pg_dumpall").args(["-U", "postgres", "--clean", "--if-exists"]).container(Some("harbor-db".into()));
        kit::stream_cmd(&mut w, "database.sql", &dump, log).await.context("дамп базы Harbor (контейнер harbor-db)")?;
        let how = hope::backup_parts(ctx, &l, &mut w).await?;
        let meta = json!({ "product": "Harbor", "layout": l, "consistency": how, "extra": { "product": "harbor", "dir": dir } });
        let (header, stats) = w.finish(&ctx.instance, "nexus", "full", json!({ "version": l.version, "product": "harbor" }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        if m.meta["extra"]["product"] != "harbor" { return hope::layout_restore(ctx).await; }
        let l: Layout = serde_json::from_value(m.meta["layout"].clone())?;
        let dir = m.meta["extra"]["dir"].as_str().unwrap_or("/opt/harbor").to_string();
        let root = cfg_str(&ctx.params, "target_root").unwrap_or_else(|| "/".into());
        let in_place = root == "/";
        if in_place && Path::new(&format!("{dir}/docker-compose.yml")).exists() { kit::run(&compose(&dir).arg("stop"), log).await?; }
        let (_, _, moved, parts) = hope::restore_parts(ctx, &l, &m, |_| true, |_| None).await?;
        let mut db = false;
        if in_place && kit::bool_param(&ctx.params, "restore_database", true) {
            kit::run(&compose(&dir).args(["up", "-d", "postgresql"]), log).await?;
            let ready = Cmd::new("pg_isready").args(["-U", "postgres"]).container(Some("harbor-db".into()));
            let mut ok = false;
            for _ in 0..60 { if kit::ok(&ready).await { ok = true; break; } tokio::time::sleep(std::time::Duration::from_secs(1)).await; }
            if !ok { bail!("база harbor-db не запустилась за минуту"); }
            let s = kit::stream(&m, "database.sql")?;
            kit::feed(&ctx.repo, s, &Cmd::new("psql").args(["-U", "postgres", "-d", "postgres", "-q"]).container(Some("harbor-db".into())), log).await.context("загрузка базы Harbor")?;
            db = true;
        }
        let mut healthy = Value::Null;
        if in_place && kit::bool_param(&ctx.params, "start", true) {
            kit::run(&compose(&dir).args(["up", "-d"]), log).await?;
            healthy = json!(kit::wait_http(&l.health, 120).await);
        }
        Ok(json!({ "parts": parts, "database": db, "moved_aside": moved, "healthy": healthy }))
    }
}
