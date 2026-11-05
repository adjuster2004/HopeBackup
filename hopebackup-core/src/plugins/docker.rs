//! Docker: именованные тома (с паузой или остановкой контейнеров, которые их используют, — только на
//! время копии своего тома), проекты docker compose (каталог проекта с файлами compose и .env), описание
//! контейнеров (docker inspect) и по желанию образы (docker save). Восстановление: том создаётся заново
//! с теми же драйвером и метками, прежнее содержимое остаётся рядом; проект — на место, docker compose up.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Docker;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "volumes": { "type": "array", "items": { "type": "string" }, "title": "Тома", "description": "Пусто — все локальные тома" },
        "exclude_volumes": { "type": "array", "items": { "type": "string" }, "title": "Кроме томов", "description": "Имена или шаблоны (buildx_*, *_cache)" },
        "quiesce": { "type": "string", "title": "Контейнеры на время копии тома", "enum": ["pause", "stop", "none"], "default": "pause",
            "description": "pause — заморозить (секунды, файлы как при снимке); stop — остановить (СУБД сбросят данные на диск); none — на ходу" },
        "compose": { "type": "boolean", "title": "Проекты docker compose", "default": true, "description": "Каталоги проектов: compose.yml, .env, конфигурация" },
        "images": { "type": "string", "title": "Образы", "enum": ["none", "used", "all"], "default": "none", "description": "used — образы контейнеров (docker save); пригодится без доступа к реестру" }
    } })
}

pub fn defaults() -> Value { json!({ "quiesce": "pause", "compose": true, "images": "none" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "volumes": { "type": "array", "items": { "type": "string" }, "title": "Тома", "description": "Пусто — все тома копии" },
        "projects": { "type": "array", "items": { "type": "string" }, "title": "Проекты compose", "description": "Пусто — все" },
        "up": { "type": "boolean", "title": "Запустить проекты (docker compose up -d)", "default": true },
        "load_images": { "type": "boolean", "title": "Загрузить образы из копии", "default": true },
        "target_dir": { "type": "string", "title": "Только распаковать в каталог", "description": "Docker не трогать" }
    } })
}

fn d(args: &[&str]) -> Cmd { Cmd::new("docker").args(args.iter().copied()) }

async fn json_out(c: &Cmd, log: &LogFn) -> Result<Value> {
    let o = kit::output(c, log).await?;
    let t = o.trim();
    if t.is_empty() { return Ok(json!([])); }
    // docker compose ls --format json — массив; некоторые версии — по объекту в строке.
    serde_json::from_str(t).or_else(|_| Ok(Value::Array(t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())))
}

fn glob(p: &str, s: &str) -> bool {
    match (p.strip_prefix('*'), p.strip_suffix('*')) {
        (Some(suf), _) if !suf.contains('*') => s.ends_with(suf),
        (_, Some(pre)) if !pre.contains('*') => s.starts_with(pre),
        _ => p == s,
    }
}

fn safe(n: &str) -> String { n.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect() }

/// Контейнеры, использующие том: (имя, состояние: running, restarting, exited…).
async fn users_of(vol: &str, log: &LogFn) -> Vec<(String, String)> {
    kit::output(&d(&["ps", "-a", "--filter", &format!("volume={vol}"), "--format", "{{.Names}}|{{.State}}"]), log).await.unwrap_or_default()
        .lines().filter_map(|l| l.split_once('|')).map(|(n, s)| (n.to_string(), s.trim().to_lowercase())).collect()
}

/// Контейнер в цикле перезапусков нельзя приостановить (Docker: «is restarting, wait until the container is running»),
/// и копия из-за него падать не должна: его данные копируются без паузы, администратору — предупреждение.
fn warn_restarting(names: &[String], what: &str, log: &LogFn) {
    for n in names {
        log(&format!("  внимание: контейнер {n} ({what}) перезапускается в цикле (restarting) — приостановить его нельзя, данные копируются без паузы. Проверьте контейнер: docker logs --tail 50 {n}"));
    }
}

async fn state_of(name: &str, log: &LogFn) -> String {
    kit::output(&d(&["inspect", "-f", "{{.State.Status}}", name]), log).await.map(|s| s.trim().to_lowercase()).unwrap_or_default()
}

/// Приостановить (pause/stop) контейнеры на время `f` и обязательно вернуть.
/// Контейнер, который к моменту паузы уже не работает (ушёл в перезапуск, остановился), пропускается
/// с предупреждением; ошибка — только если не удалось приостановить работающий контейнер.
async fn quiesced<T, F, Fut>(names: &[String], how: &str, log: &LogFn, f: F) -> Result<T>
where F: FnOnce() -> Fut, Fut: std::future::Future<Output = Result<T>> {
    if names.is_empty() || how == "none" { return f().await; }
    let (on, off) = if how == "stop" { ("stop", "start") } else { ("pause", "unpause") };
    let t0 = std::time::Instant::now();
    let mut done: Vec<String> = names.to_vec();
    if let Err(e) = kit::run(&d(&[on]).args(names.iter().cloned()), log).await {
        // docker pause обрабатывает контейнеры по одному: часть уже на паузе — разбираемся, кто нет и почему.
        let target = if how == "stop" { "exited" } else { "paused" };
        done.clear();
        let mut stuck = Vec::new();
        for n in names {
            let st = state_of(n, log).await;
            if st == target { done.push(n.clone()); }
            else if st == "running" { stuck.push(n.clone()); }
            else if st == "restarting" { warn_restarting(std::slice::from_ref(n), "во время копии", log); }
            else { log(&format!("  внимание: контейнер {n} в состоянии «{}» — копируется без паузы", if st.is_empty() { "не найден" } else { &st })); }
        }
        if !stuck.is_empty() {
            if !done.is_empty() { let _ = kit::run(&d(&[off]).args(done.iter().cloned()), log).await; }
            return Err(e.context(format!("не удалось выполнить docker {on} для {}", stuck.join(", "))));
        }
    }
    if done.is_empty() { return f().await; }
    let r = f().await;
    let back = kit::run(&d(&[off]).args(done.iter().cloned()), log).await;
    log(&format!("  {} ({}) {:.1} с", done.join(", "), if how == "stop" { "остановлены" } else { "на паузе" }, t0.elapsed().as_secs_f64()));
    if let Err(e) = back { return Err(e.context(format!("копия снята, но контейнеры не возвращены ({off})"))); }
    r
}

#[async_trait]
impl Plugin for Docker {
    fn id(&self) -> &'static str { "docker" }

    async fn discover(&self, _cfg: &Value, log: &LogFn) -> Result<Value> {
        let v = kit::output(&d(&["version", "--format", "{{.Server.Version}}"]), log).await?;
        let vols = kit::output(&d(&["volume", "ls", "-q"]), log).await.unwrap_or_default();
        let proj = json_out(&d(&["compose", "ls", "-a", "--format", "json"]), log).await.unwrap_or_default();
        Ok(json!({ "version": v.trim(), "volumes": vols.lines().collect::<Vec<_>>(), "projects": proj }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let ver = kit::output(&d(&["version", "--format", "{{.Server.Version}}"]), log).await.context("Docker недоступен (docker version)")?;
        let want = crate::connectors::net::p_list(&ctx.config, "volumes");
        let skip = crate::connectors::net::p_list(&ctx.config, "exclude_volumes");
        let how = cfg_str(&ctx.config, "quiesce").unwrap_or_else(|| "pause".into());
        let all: Vec<String> = kit::output(&d(&["volume", "ls", "-q"]), log).await?.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        for w in &want { if !all.contains(w) { bail!("нет тома {w}"); } }
        let vols: Vec<String> = all.into_iter().filter(|v| (want.is_empty() || want.contains(v)) && !skip.iter().any(|p| glob(p, v))).collect();
        log(&format!("Docker {} · томов {} · контейнеры на время копии: {how}", ver.trim(), vols.len()));
        let mut w = ctx.repo.writer();
        let mut vmeta = Vec::new();
        for v in &vols {
            let info = json_out(&d(&["volume", "inspect", v]), log).await?;
            let i = info.get(0).cloned().unwrap_or_default();
            let mp = i["Mountpoint"].as_str().unwrap_or("").to_string();
            if i["Driver"] != "local" { log(&format!("  том {v}: драйвер {} — пропущен (данные не на этой машине)", i["Driver"])); continue; }
            if !Path::new(&mp).is_dir() { log(&format!("  том {v}: каталог {mp} недоступен с этой машины — пропущен")); continue; }
            let users = users_of(v, log).await;
            let running: Vec<String> = users.iter().filter(|(_, st)| st == "running").map(|(n, _)| n.clone()).collect();
            let looping: Vec<String> = users.iter().filter(|(_, st)| st == "restarting").map(|(n, _)| n.clone()).collect();
            warn_restarting(&looping, &format!("том {v}"), log);
            let name = format!("volumes/{}.tar", safe(v));
            let w2 = &mut w;
            let mp2 = mp.clone();
            quiesced(&running, &how, log, || async move { kit::tar_paths(w2, &name, &[mp2], Default::default(), Default::default(), log).await }).await.with_context(|| format!("том {v}"))?;
            vmeta.push(json!({ "name": v, "driver": i["Driver"], "labels": i["Labels"], "options": i["Options"], "mountpoint": mp, "users": users.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>() }));
        }
        let mut pmeta = Vec::new();
        if kit::bool_param(&ctx.config, "compose", true) {
            for p in json_out(&d(&["compose", "ls", "-a", "--format", "json"]), log).await.unwrap_or_default().as_array().cloned().unwrap_or_default() {
                let name = p["Name"].as_str().unwrap_or("").to_string();
                let files: Vec<String> = p["ConfigFiles"].as_str().unwrap_or("").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                let Some(dir) = files.first().and_then(|f| Path::new(f).parent()).map(|p| p.to_string_lossy().to_string()) else { continue };
                if !Path::new(&dir).is_dir() { log(&format!("  проект {name}: каталог {dir} недоступен — пропущен")); continue; }
                // Только работающие: «docker ps» без фильтра показывает и контейнеры в цикле перезапусков.
                let by_state = |st: &'static str| { let name = name.clone(); async move {
                    kit::output(&d(&["ps", "-a", "--filter", &format!("label=com.docker.compose.project={name}"), "--filter", &format!("status={st}"), "--format", "{{.Names}}"]), log).await
                        .unwrap_or_default().lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect::<Vec<String>>() } };
                let running = by_state("running").await;
                warn_restarting(&by_state("restarting").await, &format!("проект {name}"), log);
                let sname = format!("compose/{}.tar", safe(&name));
                let w2 = &mut w;
                let dir2 = dir.clone();
                // Каталог проекта может содержать данные контейнеров (bind) — так же на паузе.
                quiesced(&running, &how, log, || async move { kit::tar_paths(w2, &sname, &[dir2], Default::default(), Default::default(), log).await }).await.with_context(|| format!("проект {name}"))?;
                pmeta.push(json!({ "name": name, "dir": dir, "files": files, "status": p["Status"] }));
            }
        }
        let ids: Vec<String> = kit::output(&d(&["ps", "-aq"]), log).await.unwrap_or_default().lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect();
        if !ids.is_empty() {
            let insp = kit::output(&d(&["inspect"]).args(ids.clone()), log).await.unwrap_or_default();
            kit::add_bytes(&mut w, "containers.json", insp.into_bytes()).await?;
        }
        let mut imeta = Vec::new();
        let images = cfg_str(&ctx.config, "images").unwrap_or_else(|| "none".into());
        if images != "none" {
            let list: Vec<String> = if images == "all" {
                kit::output(&d(&["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"]), log).await?.lines().filter(|l| !l.contains("<none>")).map(|s| s.to_string()).collect()
            } else {
                let mut l: Vec<String> = kit::output(&d(&["ps", "-a", "--format", "{{.Image}}"]), log).await?.lines().map(|s| s.to_string()).collect();
                l.sort(); l.dedup(); l
            };
            for im in list.iter().filter(|s| !s.is_empty()) {
                kit::stream_cmd(&mut w, &format!("images/{}.tar", safe(im)), &d(&["save", im]), log).await.with_context(|| format!("образ {im}"))?;
                imeta.push(json!(im));
            }
        }
        let meta = json!({ "version": ver.trim(), "volumes": vmeta, "projects": pmeta, "images": imeta, "quiesce": how });
        let (header, stats) = w.finish(&ctx.instance, "docker", "full", json!({ "volumes": vmeta.len(), "projects": pmeta.len() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let want_v = crate::connectors::net::p_list(&ctx.params, "volumes");
        let want_p = crate::connectors::net::p_list(&ctx.params, "projects");
        let vols: Vec<Value> = m.meta["volumes"].as_array().cloned().unwrap_or_default().into_iter().filter(|v| want_v.is_empty() || want_v.iter().any(|w| v["name"] == w.as_str())).collect();
        let projs: Vec<Value> = m.meta["projects"].as_array().cloned().unwrap_or_default().into_iter().filter(|p| want_p.is_empty() || want_p.iter().any(|w| p["name"] == w.as_str())).collect();
        if let Some(t) = cfg_str(&ctx.params, "target_dir") {
            for s in m.streams.iter().filter(|s| s.name.ends_with(".tar") && !s.name.starts_with("images/")) { kit::untar(&ctx.repo, s, Path::new(&t), Default::default(), log).await?; }
            return Ok(json!({ "target_dir": t }));
        }
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let (mut done_v, mut moved) = (Vec::new(), Vec::new());
        for v in &vols {
            let name = v["name"].as_str().unwrap_or("").to_string();
            let s = kit::stream(&m, &format!("volumes/{}.tar", safe(&name)))?;
            let exists = kit::ok(&d(&["volume", "inspect", &name])).await;
            if !exists {
                let mut c = d(&["volume", "create", "--driver", v["driver"].as_str().unwrap_or("local")]);
                for (k, x) in v["labels"].as_object().into_iter().flatten() { c = c.arg("--label").arg(format!("{k}={}", x.as_str().unwrap_or(""))); }
                for (k, x) in v["options"].as_object().into_iter().flatten() { c = c.arg("--opt").arg(format!("{k}={}", x.as_str().unwrap_or(""))); }
                kit::output(&c.arg(&name), log).await.with_context(|| format!("создание тома {name}"))?;
            }
            let mp = json_out(&d(&["volume", "inspect", &name]), log).await?.get(0).and_then(|i| i["Mountpoint"].as_str().map(|s| s.to_string())).ok_or_else(|| anyhow!("том {name}: нет каталога"))?;
            // При восстановлении тома останавливаем и контейнеры в цикле перезапусков: иначе они пишут в том во время распаковки.
            let running: Vec<String> = users_of(&name, log).await.into_iter().filter(|(_, st)| st == "running" || st == "restarting").map(|(n, _)| n).collect();
            if !running.is_empty() { kit::run(&d(&["stop"]).args(running.clone()), log).await?; }
            let r = async {
                if Path::new(&mp).exists() && std::fs::read_dir(&mp)?.next().is_some() {
                    let a = kit::aside_name(&mp, &ts);
                    std::fs::rename(&mp, &a)?;
                    log(&format!("  прежнее содержимое тома {name} → {a}"));
                    moved.push(a);
                }
                std::fs::create_dir_all(&mp)?;
                let old = v["mountpoint"].as_str().unwrap_or("").trim_start_matches('/').to_string();
                kit::untar(&ctx.repo, s, Path::new("/"), kit::Untar { relocate: vec![(old, mp.trim_start_matches('/').to_string())], ..Default::default() }, log).await?;
                Ok::<_, anyhow::Error>(())
            }.await;
            if !running.is_empty() { kit::run(&d(&["start"]).args(running.clone()), log).await?; }
            r.with_context(|| format!("том {name}"))?;
            done_v.push(name);
        }
        if kit::bool_param(&ctx.params, "load_images", true) {
            for s in m.streams.iter().filter(|s| s.name.starts_with("images/")) { kit::feed(&ctx.repo, s, &d(&["load"]), log).await?; }
        }
        let mut done_p = Vec::new();
        for p in &projs {
            let name = p["name"].as_str().unwrap_or("").to_string();
            let s = kit::stream(&m, &format!("compose/{}.tar", safe(&name)))?;
            kit::untar(&ctx.repo, s, Path::new("/"), Default::default(), log).await?;
            if kit::bool_param(&ctx.params, "up", true) {
                let mut c = d(&["compose", "-p", &name]);
                for f in p["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) { c = c.arg("-f").arg(f); }
                kit::run(&c.args(["up", "-d"]), log).await.with_context(|| format!("docker compose up ({name})"))?;
            }
            done_p.push(name);
        }
        Ok(json!({ "volumes": done_v, "projects": done_p, "moved_aside": moved }))
    }
}
