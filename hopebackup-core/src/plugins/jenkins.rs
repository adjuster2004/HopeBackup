//! Jenkins: JENKINS_HOME (задания, настройки, учётные данные с secrets/, плагины, история сборок) без
//! рабочих каталогов, кэшей и распакованного war; служба systemd или контейнер Docker (том
//! /var/jenkins_home). Jenkins пишет XML атомарно — по умолчанию копия на ходу без простоя (или снимок ФС);
//! consistency = stop — с короткой остановкой.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit};
use super::hope::{self, part, Layout, Part, Phase};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Jenkins;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "service": { "type": "string", "title": "Служба", "default": "jenkins" },
        "container": { "type": "string", "title": "Контейнер Docker", "description": "Вместо службы: каталог — том /var/jenkins_home контейнера" },
        "home": { "type": "string", "title": "JENKINS_HOME", "description": "Пусто — из юнита службы, /etc/default/jenkins или /var/lib/jenkins" },
        "consistency": kit::consistency_schema("По умолчанию для Jenkins — снимок ФС, без него на ходу (stop — короткая остановка)."),
        "include_builds": { "type": "boolean", "title": "История сборок (журналы, результаты тестов)", "default": true },
        "include_artifacts": { "type": "boolean", "title": "Артефакты сборок (builds/*/archive)", "default": false },
        "include_logs": { "type": "boolean", "title": "Журналы Jenkins (logs/)", "default": false },
        "exclude": { "type": "array", "items": { "type": "string" }, "title": "Исключения" }
    } })
}

pub fn defaults() -> Value { json!({ "service": "jenkins", "consistency": "auto", "include_builds": true }) }

pub fn restore_schema() -> Value { json!({ "type": "object", "properties": Value::Object(hope::layout_restore_schema("home, config")) }) }

/// Значение из файла переменных (/etc/default/jenkins: JENKINS_HOME=…).
fn sh_var(file: &str, k: &str) -> Option<String> {
    std::fs::read_to_string(file).ok()?.lines().filter_map(|l| l.trim().strip_prefix(&format!("{k}="))).last().map(|v| v.trim_matches('"').trim_matches('\'').to_string()).filter(|v| !v.is_empty() && !v.contains('$'))
}

pub fn layout(cfg: &Value) -> Result<Layout> {
    let mut l = Layout { live_without_snapshot: kit::consistency(cfg) != kit::Consistency::Stop, ..Default::default() };
    let mut ex = vec!["workspace".to_string(), "**/jobs/*/workspace*".into(), "caches".into(), ".cache".into(), "war".into(), "tmp".into(), "*.tmp".into(), ".lastStarted".into()];
    if !kit::bool_param(cfg, "include_builds", true) { ex.push("**/jobs/*/builds".into()); }
    if !kit::bool_param(cfg, "include_artifacts", false) { ex.push("**/builds/*/archive".into()); }
    if !kit::bool_param(cfg, "include_logs", false) { ex.push("logs".into()); }
    ex.extend(crate::connectors::net::p_list(cfg, "exclude"));
    let mut config = Vec::new();
    let home;
    if let Some(c) = cfg_str(cfg, "container") {
        home = cfg_str(cfg, "home").or_else(|| kit::docker_mount(&c, "/var/jenkins_home")).ok_or_else(|| anyhow::anyhow!("у контейнера {c} нет тома /var/jenkins_home — укажите JENKINS_HOME на хосте"))?;
        l.container = c.clone();
        l.health = String::new();
        l.version = kit::docker_env(&c, "JENKINS_VERSION").unwrap_or_default();
    } else {
        let svc = cfg_str(cfg, "service").unwrap_or_else(|| "jenkins".into());
        let u = kit::unit(&svc).unwrap_or_default();
        home = cfg_str(cfg, "home").or_else(|| u.var("JENKINS_HOME")).or_else(|| sh_var("/etc/default/jenkins", "JENKINS_HOME")).or_else(|| sh_var("/etc/sysconfig/jenkins", "JENKINS_HOME")).unwrap_or_else(|| "/var/lib/jenkins".into());
        l.user = if u.user.is_empty() { "jenkins".into() } else { u.user.clone() };
        let port = u.var("JENKINS_PORT").or_else(|| sh_var("/etc/default/jenkins", "HTTP_PORT")).unwrap_or_else(|| "8080".into());
        l.health = format!("http://127.0.0.1:{port}/login");
        l.service = svc.clone();
        config.extend(hope::unit_files(&[&format!("{svc}.service")]));
        for f in ["/etc/default/jenkins", "/etc/sysconfig/jenkins"] { if Path::new(f).exists() { config.push(f.into()); } }
        l.version = std::fs::read_to_string(Path::new(&home).join("config.xml")).ok().and_then(|x| x.split("<version>").nth(1).map(|v| v.split('<').next().unwrap_or("").to_string())).unwrap_or_default();
    }
    if !Path::new(&home).join("config.xml").exists() && !Path::new(&home).join("jobs").exists() { bail!("{home} не похож на JENKINS_HOME (нет config.xml и jobs/)"); }
    l.parts.push(part("home", "JENKINS_HOME: задания, настройки, секреты, плагины", vec![home.clone()], ex, Phase::Quiesced, &l.user));
    if !config.is_empty() { l.parts.push(Part::config(config)); }
    l.notes.push("secrets/ и credentials.xml — ключи учётных данных Jenkins: копия зашифрована паролем репозитория".into());
    Ok(l)
}

#[async_trait]
impl Plugin for Jenkins {
    fn id(&self) -> &'static str { "jenkins" }
    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let l = layout(cfg)?;
        Ok(json!({ "home": l.parts[0].paths[0], "version": l.version, "service": l.service, "container": l.container, "active": l.svc_active() }))
    }
    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let l = layout(&ctx.config)?;
        hope::layout_backup(ctx, "jenkins", "Jenkins", &l, Value::Null).await
    }
    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> { hope::layout_restore(ctx).await }
}
