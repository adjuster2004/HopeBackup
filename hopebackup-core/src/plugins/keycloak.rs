//! Keycloak (Quarkus, 17+): выгрузка realm с пользователями (kc.sh export --users realm_file) — переносимая
//! копия, которую можно загрузить в любой Keycloak той же или новее версии, — и конфигурация (conf/,
//! providers/, themes/, юнит). База Keycloak (PostgreSQL, MariaDB…) — отдельным экземпляром плагина СУБД:
//! это основная копия для восстановления «как было»; встроенная H2 (dev-file) копируется каталогом data/.
//!
//! Восстановление: конфигурация на место, затем kc.sh import --override (служба на время останавливается).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd, WorkDir};
use super::hope::{self, part, Layout, Part, Phase};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Keycloak;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "service": { "type": "string", "title": "Служба", "default": "keycloak" },
        "container": { "type": "string", "title": "Контейнер Docker", "description": "Вместо службы: выгрузка выполняется в контейнере" },
        "home": { "type": "string", "title": "Каталог Keycloak", "description": "Пусто — из юнита (…/bin/kc.sh), иначе /opt/keycloak" },
        "realms": { "type": "array", "items": { "type": "string" }, "title": "Realm", "description": "Пусто — все" },
        "export_mode": { "type": "string", "title": "Выгрузка realm", "enum": ["live", "stop", "none"], "default": "live",
            "description": "live — при работающем сервере (отдельный процесс kc.sh export); stop — служба останавливается на время выгрузки; none — только конфигурация (база — плагином СУБД)" }
    } })
}

pub fn defaults() -> Value { json!({ "service": "keycloak", "export_mode": "live" }) }

pub fn restore_schema() -> Value {
    let mut m = hope::layout_restore_schema("config, data, realms");
    m.insert("import".into(), json!({ "type": "boolean", "title": "Загрузить realm из выгрузки (kc.sh import --override)", "default": true }));
    m.insert("override".into(), json!({ "type": "boolean", "title": "Заменять существующие realm", "default": true }));
    json!({ "type": "object", "properties": Value::Object(m) })
}

struct Kc { l: Layout, home: String, env: Vec<(String, String)>, kc: String }

fn detect(cfg: &Value) -> Result<Kc> {
    let mut l = Layout::default();
    if let Some(c) = cfg_str(cfg, "container") {
        l.container = c.clone();
        l.version = kit::docker_env(&c, "KC_VERSION").unwrap_or_default();
        let home = cfg_str(cfg, "home").unwrap_or_else(|| "/opt/keycloak".into());
        return Ok(Kc { kc: format!("{home}/bin/kc.sh"), home, env: vec![], l });
    }
    let svc = cfg_str(cfg, "service").unwrap_or_else(|| "keycloak".into());
    let u = kit::unit(&svc).unwrap_or_default();
    let bin = hope::first_token(&u.exec);
    let home = cfg_str(cfg, "home").or_else(|| bin.strip_suffix("/bin/kc.sh").map(|s| s.to_string())).unwrap_or_else(|| "/opt/keycloak".into());
    if !Path::new(&home).join("bin/kc.sh").exists() { bail!("нет {home}/bin/kc.sh — укажите каталог Keycloak"); }
    l.service = svc.clone();
    l.user = if u.user.is_empty() { "keycloak".into() } else { u.user.clone() };
    let conf = std::fs::read_to_string(Path::new(&home).join("conf/keycloak.conf")).unwrap_or_default();
    let cv = |k: &str| u.var(&format!("KC_{}", k.to_uppercase().replace('-', "_"))).or_else(|| conf.lines().find_map(|x| x.trim().strip_prefix(&format!("{k}=")).map(|v| v.trim().to_string())));
    let port = cv("http-port").unwrap_or_else(|| "8080".into());
    l.health = format!("http://127.0.0.1:{port}/");
    l.version = std::fs::read_to_string(Path::new(&home).join("version.txt")).unwrap_or_default().trim().to_string();
    let db = cv("db").unwrap_or_else(|| "dev-file".into());
    let mut cfgp: Vec<String> = ["conf", "providers", "themes"].iter().map(|d| format!("{home}/{d}")).filter(|p| Path::new(p).exists()).collect();
    cfgp.extend(hope::unit_files(&[&format!("{svc}.service")]));
    l.parts.push(Part::config(cfgp));
    if db == "dev-file" && Path::new(&home).join("data").exists() {
        l.parts.push(part("data", "Встроенная база H2 (dev-file)", vec![format!("{home}/data")], vec![format!("{home}/data/tmp"), format!("{home}/data/log")], Phase::Quiesced, &l.user));
    } else {
        l.notes.push(format!("база Keycloak — {db}: добавьте её отдельным экземпляром плагина СУБД (выгрузка realm — переносимая копия, но не замена копии базы)"));
    }
    let env: Vec<(String, String)> = u.env.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
    Ok(Kc { kc: format!("{home}/bin/kc.sh"), home, env, l })
}

impl Kc {
    fn cmd(&self, args: &[&str]) -> Cmd {
        let mut c = Cmd::new(&self.kc).args(args.iter().copied()).container((!self.l.container.is_empty()).then(|| self.l.container.clone()));
        if self.l.container.is_empty() { c = c.user(Some(self.l.user.clone())); }
        for (k, v) in &self.env { c = c.env(k, v); }
        c
    }
}

#[async_trait]
impl Plugin for Keycloak {
    fn id(&self) -> &'static str { "keycloak" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let k = detect(cfg)?;
        Ok(json!({ "home": k.home, "version": k.l.version, "service": k.l.service, "container": k.l.container, "active": k.l.svc_active(), "notes": k.l.notes }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let k = detect(&ctx.config)?;
        log(&format!("Keycloak {} · {}", k.l.version, k.home));
        for n in &k.l.notes { log(&format!("  ! {n}")); }
        let mut w = ctx.repo.writer();
        let how = hope::backup_parts(ctx, &k.l, &mut w).await?;
        let mode = cfg_str(&ctx.config, "export_mode").unwrap_or_else(|| "live".into());
        let realms = crate::connectors::net::p_list(&ctx.config, "realms");
        let mut exported = false;
        if mode != "none" {
            let mut args = vec!["export".to_string(), "--users".into(), "realm_file".into()];
            if realms.len() == 1 { args.push("--realm".into()); args.push(realms[0].clone()); }
            if k.l.container.is_empty() {
                let wd = WorkDir::new(None, Some(&k.l.user))?;
                let mut a = args.clone(); a.push("--dir".into()); a.push(wd.path());
                let run = || async { kit::run(&k.cmd(&a.iter().map(|s| s.as_str()).collect::<Vec<_>>()), log).await };
                if mode == "stop" && k.l.svc_exists() { k.l.while_stopped(log, run).await? } else { run().await? };
                let mut files: Vec<String> = std::fs::read_dir(&wd.0)?.flatten().map(|e| e.path().to_string_lossy().to_string()).collect();
                files.sort();
                if !realms.is_empty() { files.retain(|f| realms.iter().any(|r| Path::new(f).file_name().map(|n| n.to_string_lossy().starts_with(&format!("{r}-"))).unwrap_or(false))); }
                if files.is_empty() { bail!("kc.sh export не создал файлов"); }
                kit::tar_paths(&mut w, "realms.tar", &[wd.path()], Default::default(), Default::default(), log).await?;
            } else {
                let dir = format!("/tmp/hopebackup-kc-{}", crate::util::short_id());
                let mut a = args.clone(); a.push("--dir".into()); a.push(dir.clone());
                let ct = Some(k.l.container.clone());
                let r = async {
                    kit::run(&k.cmd(&a.iter().map(|s| s.as_str()).collect::<Vec<_>>()), log).await?;
                    kit::stream_cmd(&mut w, "realms.container.tar", &Cmd::new("tar").args(["-C", &dir, "-cf", "-", "."]).container(ct.clone()), log).await?;
                    kit::stream_cmd(&mut w, "config.container.tar", &Cmd::new("tar").args(["-C", &k.home, "-cf", "-", "conf", "providers", "themes"]).container(ct.clone()), log).await.ok();
                    Ok::<_, anyhow::Error>(())
                }.await;
                let _ = kit::output(&Cmd::new("rm").args(["-rf", &dir]).container(ct.clone()), log).await;
                r?;
            }
            exported = true;
        }
        let meta = json!({ "product": "Keycloak", "layout": k.l, "consistency": how, "home": k.home, "exported": exported, "realms": realms });
        let (header, stats) = w.finish(&ctx.instance, "keycloak", "full", json!({ "version": k.l.version }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let l: Layout = serde_json::from_value(m.meta["layout"].clone())?;
        let home = m.meta["home"].as_str().unwrap_or("/opt/keycloak").to_string();
        let root = cfg_str(&ctx.params, "target_root").unwrap_or_else(|| "/".into());
        let in_place = root == "/";
        let import = kit::bool_param(&ctx.params, "import", true) && in_place;
        let want = crate::connectors::net::p_list(&ctx.params, "parts");
        let realms_wanted = want.is_empty() || want.iter().any(|p| p == "realms");
        let mut res = json!({});
        if l.container.is_empty() {
            let mut parts = Vec::new();
            if !l.parts.is_empty() && (want.is_empty() || want.iter().any(|p| p != "realms")) {
                let (_, _, moved, p) = hope::restore_parts(ctx, &l, &m, |x| want.is_empty() || want.contains(&x.name), |_| None).await?;
                parts = p;
                res["moved_aside"] = json!(moved);
            } else if in_place && l.svc_active() { l.svc_stop(log).await?; }
            if realms_wanted {
                if let Ok(s) = kit::stream(&m, "realms.tar") {
                    let wd = WorkDir::new(None, Some(&l.user))?;
                    kit::untar(&ctx.repo, s, &wd.0, kit::Untar { owner: Some(l.user.clone()), ..Default::default() }, log).await?;
                    // В архиве — полный путь каталога выгрузки; находим сами файлы realm.
                    let dir = find_json_dir(&wd.0).ok_or_else(|| anyhow!("в копии нет файлов realm"))?;
                    if import {
                        let k = Kc { l: l.clone(), home: home.clone(), env: kit::unit(&l.service).map(|u| u.env.into_iter().collect()).unwrap_or_default(), kc: format!("{home}/bin/kc.sh") };
                        kit::chown_to(&[wd.path()], &l.user, log).ok();
                        kit::run(&k.cmd(&["import", "--dir", &dir.to_string_lossy(), "--override", if kit::bool_param(&ctx.params, "override", true) { "true" } else { "false" }]), log).await.context("kc.sh import")?;
                        parts.push("realms".into());
                    } else {
                        let out = Path::new(&root).join("keycloak-realms");
                        std::fs::create_dir_all(&out)?;
                        for e in std::fs::read_dir(&dir)?.flatten() { std::fs::copy(e.path(), out.join(e.file_name()))?; }
                        log(&format!("файлы realm → {}", out.display()));
                    }
                }
            }
            res["healthy"] = hope::finish_restore(ctx, &l, in_place).await?;
            res["parts"] = json!(parts);
        } else {
            let ct = Some(l.container.clone());
            if let (true, Ok(s)) = (realms_wanted, kit::stream(&m, "realms.container.tar")) {
                let dir = format!("/tmp/hopebackup-kc-{}", crate::util::short_id());
                kit::output(&Cmd::new("mkdir").args(["-p", &dir]).container(ct.clone()), log).await?;
                kit::feed(&ctx.repo, s, &Cmd::new("tar").args(["-C", &dir, "-xf", "-"]).container(ct.clone()), log).await?;
                let k = Kc { l: l.clone(), home: home.clone(), env: vec![], kc: format!("{home}/bin/kc.sh") };
                let r = kit::run(&k.cmd(&["import", "--dir", &dir, "--override", if kit::bool_param(&ctx.params, "override", true) { "true" } else { "false" }]), log).await;
                let _ = kit::output(&Cmd::new("rm").args(["-rf", &dir]).container(ct.clone()), log).await;
                r.context("kc.sh import в контейнере")?;
                res["parts"] = json!(["realms"]);
            }
        }
        Ok(res)
    }
}

fn find_json_dir(p: &Path) -> Option<std::path::PathBuf> {
    let rd = std::fs::read_dir(p).ok()?;
    let mut dirs = Vec::new();
    for e in rd.flatten() {
        let q = e.path();
        if q.is_file() && q.extension().map(|x| x == "json").unwrap_or(false) { return Some(p.to_path_buf()); }
        if q.is_dir() { dirs.push(q); }
    }
    dirs.into_iter().find_map(|d| find_json_dir(&d))
}
