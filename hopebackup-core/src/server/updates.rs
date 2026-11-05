//! «Обновления» (как в HopeDFS и HopeInventory): новые версии «Ковчега» ставятся из панели — онлайн по
//! манифесту (каналы stable/beta, SHA-256 обязателен, автопроверка раз в сутки) или загрузкой архива выпуска
//! для закрытого контура. Только администратор.
//!
//! Пакет — архив выпуска `hopebackup-<версия>-linux.tar.gz` (все сборки: x86_64/aarch64, glibc/musl) или
//! одна программа `hopebackup.gz`. Из архива берётся сборка для этой машины, контрольные суммы всех сборок
//! сверяются, комплект обновляется вместе с программой — по нему потом обновляются агенты (и ставятся новые).
//! Установку выполняет копия прежней программы вне службы (`crate::updater`): замена, перезапуск, ожидание
//! ответа новой версии на `/api/health`, иначе откат.
//!
//! Агенты обновляются отсюда же: задание `self_update` — агент сам скачивает свою сборку с сервера, сверяет
//! SHA-256, проверяет пробным запуском и передаёт своему установщику; после перезапуска новая версия
//! отмечается, как только агент снова вышел на связь (иначе — откат на агенте).
//!
//! Файлы — `<данные сервера>/updates/`: `check.json`, `staged.json` + `package.bin` (+ `kit/` — комплект
//! сборок), `status.json`, `apply.request`, `hopebackup-applier`.

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AO};
use std::sync::Arc;

use super::api::{bad, R};
use super::auth::Who;
use super::Server;
use crate::proto::JobSpec;
use crate::updater::{self as up, cmp_ver, parse_ver, Job};

/// Источник обновлений (в настройках сервера).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateSettings {
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub auto_check: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Check {
    pub latest_version: String,
    pub last_checked: u64,
    pub notes: String,
    /// info | warn | error
    pub notes_kind: String,
    pub download_url: String,
    pub download_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Staged {
    pub filename: String,
    pub version: String,
    pub size: u64,
    /// SHA-256 загруженного файла — сверить с контрольной суммой поставщика.
    pub file_sha256: String,
    /// SHA-256 программы для этой машины — сверяет установщик.
    pub bin_sha256: String,
    /// Сборки в комплекте (архив выпуска); пусто — загружена одна программа.
    #[serde(default)]
    pub kit: Vec<String>,
    pub uploaded_at: u64,
    /// offline | online
    pub source: String,
}

static BUSY: AtomicBool = AtomicBool::new(false);
struct Busy;
impl Busy { fn take() -> Option<Busy> { if BUSY.swap(true, AO::SeqCst) { None } else { Some(Busy) } } }
impl Drop for Busy { fn drop(&mut self) { BUSY.store(false, AO::SeqCst); } }
const BUSY_MSG: &str = "идёт другая операция с обновлениями — дождитесь её окончания";

fn dir(s: &Server) -> PathBuf { s.dir.join("updates") }
fn p_check(s: &Server) -> PathBuf { dir(s).join("check.json") }
fn p_staged(s: &Server) -> PathBuf { dir(s).join("staged.json") }
fn p_package(s: &Server) -> PathBuf { dir(s).join("package.bin") }
fn p_kit(s: &Server) -> PathBuf { dir(s).join("kit") }
pub fn p_status(s: &Server) -> PathBuf { dir(s).join("status.json") }

fn load_check(s: &Server) -> Check { up::load(&p_check(s)).unwrap_or_default() }
fn load_staged(s: &Server) -> Option<Staged> { up::load::<Staged>(&p_staged(s)).filter(|_| p_package(s).is_file()) }

/// Ход установки (с учётом зависшей передачи установщику и прерванной подготовки).
fn load_job(s: &Server) -> Option<Job> {
    let p = p_status(s);
    let mut j: Job = up::load(&p)?;
    let now = up::now();
    if j.running && j.phase == "handoff" && now.saturating_sub(j.updated_at) > 60 {
        j.running = false; j.finished = true; j.failed = true; j.phase = "failed".into();
        j.message = "установщик не запустился за 60 секунд — обновление не выполнено (journalctl -t hopebackup-update)".into();
        up::save(&p, &j);
        let _ = std::fs::remove_file(dir(s).join("apply.request"));
    }
    if j.running && ["downloading", "verifying", "staging"].contains(&j.phase.as_str()) && !BUSY.load(AO::SeqCst) && now.saturating_sub(j.updated_at) > 120 {
        j.running = false; j.finished = true; j.failed = true; j.phase = "failed".into();
        j.message = "подготовка обновления прервана (служба перезапускалась) — повторите установку".into();
        up::save(&p, &j);
    }
    Some(j)
}

fn set_phase(s: &Server, phase: &str, progress: u8) { up::update_job(&p_status(s), |j| { j.phase = phase.into(); j.progress = progress; }); }

fn fail_job(s: &Server, msg: &str) {
    up::update_job(&p_status(s), |j| { j.phase = "failed".into(); j.running = false; j.finished = true; j.failed = true; j.message = msg.into(); });
    audit_result(s);
}

// ---------------------------------------------------------------- проверка источника

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(std::time::Duration::from_secs(900)).connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("HopeBackup/{}", crate::VERSION)).build().unwrap_or_default()
}

async fn check(s: &Server) -> Check {
    let cfg = s.store.read(|st| st.settings.updates.clone());
    let ch = if cfg.channel.is_empty() { "stable".to_string() } else { cfg.channel.clone() };
    let mut c = Check { last_checked: up::now(), notes_kind: "info".into(), ..Default::default() };
    match fetch_manifest(&cfg.source_url, &ch).await {
        Ok(Some((v, url, sha, notes))) => { c.latest_version = v; c.download_url = url; c.download_sha256 = sha; c.notes = notes; }
        Ok(None) => { c.notes = "Источник обновлений не задан. Укажите адрес манифеста или загрузите архив выпуска.".into(); c.notes_kind = "warn".into(); }
        Err(e) => { c.notes = e; c.notes_kind = "error".into(); }
    }
    up::save(&p_check(s), &c);
    c
}

/// Манифест: `{ "stable": {version,url,sha256,notes}, "beta": {...} }` или один объект `{version,url,sha256,notes}`.
async fn fetch_manifest(src: &str, channel: &str) -> std::result::Result<Option<(String, String, String, String)>, String> {
    let src = src.trim();
    if src.is_empty() { return Ok(None); }
    let base = reqwest::Url::parse(src).map_err(|e| format!("Неверный адрес манифеста: {e}"))?;
    let r = http().get(base.clone()).timeout(std::time::Duration::from_secs(30)).send().await.map_err(|e| format!("Источник обновлений недоступен: {e}"))?;
    if !r.status().is_success() { return Err(format!("Источник обновлений ответил HTTP {}", r.status().as_u16())); }
    let v: Value = r.json().await.map_err(|_| "Источник вернул не манифест обновлений (нужен JSON с полем \"version\")".to_string())?;
    let entry = v.get(channel).filter(|e| e.is_object()).cloned().or_else(|| v.get("version").is_some().then(|| v.clone()))
        .ok_or_else(|| if v.get("stable").is_some() { format!("В манифесте нет канала «{channel}»") } else { "Источник вернул не манифест обновлений (нужен JSON с полем \"version\")".to_string() })?;
    let ver = match &entry["version"] { Value::String(x) => x.trim().to_string(), Value::Number(n) => n.to_string(), _ => String::new() };
    if parse_ver(&ver).is_none() { return Err("В манифесте неверный номер версии (ожидается вида 0.9.0)".into()); }
    let url = entry["url"].as_str().unwrap_or("").trim().to_string();
    if url.is_empty() { return Err("В манифесте нет ссылки на пакет (поле \"url\")".into()); }
    let sha = entry["sha256"].as_str().unwrap_or("").trim().to_lowercase();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) { return Err("В манифесте нет контрольной суммы пакета (поле \"sha256\") — установка без неё запрещена".into()); }
    let full = base.join(&url).map_err(|e| format!("Неверная ссылка на пакет: {e}"))?;
    Ok(Some((ver, full.to_string(), sha, entry["notes"].as_str().unwrap_or("").to_string())))
}

async fn download(s: &Server, c: &Check) -> Result<Staged> {
    use sha2::Digest;
    use std::io::Write;
    let d = dir(s);
    up::ensure_dir(&d)?;
    let tmp = d.join("download.tmp");
    let mut r = http().get(&c.download_url).send().await.context("скачивание")?;
    if !r.status().is_success() { bail!("скачивание: HTTP {}", r.status().as_u16()); }
    let total = r.content_length().unwrap_or(0);
    if total > up::MAX_PACKAGE { bail!("пакет больше 512 МБ"); }
    let mut f = std::fs::File::create(&tmp)?;
    let (mut got, mut last) = (0u64, 0u8);
    let mut h = sha2::Sha256::new();
    while let Some(chunk) = r.chunk().await.context("скачивание")? {
        got += chunk.len() as u64;
        if got > up::MAX_PACKAGE { let _ = std::fs::remove_file(&tmp); bail!("пакет больше 512 МБ"); }
        h.update(&chunk);
        f.write_all(&chunk)?;
        if total > 0 { let p = (got * 50 / total).min(50) as u8; if p != last { last = p; set_phase(s, "downloading", p.max(3)); } }
    }
    drop(f);
    if hex::encode(h.finalize()) != c.download_sha256 { let _ = std::fs::remove_file(&tmp); bail!("контрольная сумма скачанного пакета не совпадает с манифестом — пакет повреждён или подменён"); }
    let name = reqwest::Url::parse(&c.download_url).ok().and_then(|u| u.path_segments().and_then(|mut x| x.next_back().map(String::from))).filter(|x| !x.is_empty()).unwrap_or_else(|| "hopebackup.tar.gz".into());
    set_phase(s, "verifying", 55);
    prepare(s, &tmp, &name, Some(&c.latest_version), "online").await
}

// ---------------------------------------------------------------- проверка пакета

/// Распаковать и проверить пакет; сохранить как готовый к установке. `raw` удаляется.
async fn prepare(s: &Server, raw: &Path, filename: &str, want: Option<&str>, source: &str) -> Result<Staged> {
    let r = prepare_inner(s, raw, filename, want, source).await;
    let _ = std::fs::remove_file(raw);
    let _ = std::fs::remove_dir_all(dir(s).join("unpack"));
    r
}

fn is_tar_gz(p: &Path) -> bool {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(p) else { return false };
    let mut b = vec![0u8; 512];
    let mut gz = flate2::read::GzDecoder::new(f);
    gz.read_exact(&mut b).is_ok() && &b[257..262] == b"ustar"
}

fn gunzip_to(src: &Path, dst: &Path) -> Result<()> {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(src)?);
    let mut out = std::fs::File::create(dst)?;
    std::io::copy(&mut std::io::Read::take(&mut gz, up::MAX_PACKAGE * 2), &mut out).context("распаковка .gz")?;
    Ok(())
}

async fn prepare_inner(s: &Server, raw: &Path, filename: &str, want: Option<&str>, source: &str) -> Result<Staged> {
    use std::os::unix::fs::PermissionsExt;
    let d = dir(s);
    up::ensure_dir(&d)?;
    let size = std::fs::metadata(raw).map(|m| m.len()).unwrap_or(0);
    if size == 0 { bail!("пустой файл"); }
    let file_sha256 = up::sha256_file(raw)?;
    let mut head = [0u8; 2];
    { use std::io::Read; let _ = std::fs::File::open(raw)?.read(&mut head); }
    let tmp = d.join("package.tmp");
    let me = crate::dist::this_target();
    let mut kit = Vec::new();
    let kit_tmp = d.join("kit.tmp");
    let _ = std::fs::remove_dir_all(&kit_tmp);
    if head == [0x1f, 0x8b] && is_tar_gz(raw) {
        // Архив выпуска: сборки в dist/, у каждой — .sha256.
        let un = d.join("unpack");
        let _ = std::fs::remove_dir_all(&un);
        std::fs::create_dir_all(&un)?;
        let (rp, up2) = (raw.to_path_buf(), un.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut a = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&rp)?));
            for e in a.entries()? {
                let mut e = e?;
                let p = e.path()?.to_path_buf();
                // Только обычные файлы и каталоги, без выхода за пределы каталога распаковки.
                if p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) { continue; }
                if !(e.header().entry_type().is_file() || e.header().entry_type().is_dir()) { continue; }
                e.unpack_in(&up2)?;
            }
            Ok(())
        }).await??;
        let dist = walk_find_dist(&un).ok_or_else(|| anyhow!("в архиве нет каталога dist/ со сборками — это не архив выпуска HopeBackup"))?;
        std::fs::create_dir_all(&kit_tmp)?;
        for t in crate::dist::TARGETS {
            let f = dist.join(format!("hopebackup-{t}.gz"));
            if !f.is_file() { continue; }
            let want_sha = std::fs::read_to_string(dist.join(format!("hopebackup-{t}.gz.sha256"))).ok().and_then(|x| x.split_whitespace().next().map(String::from));
            match want_sha {
                Some(w) if w == up::sha256_file(&f)? => {}
                Some(_) => bail!("контрольная сумма сборки {t} не совпадает — архив повреждён"),
                None => bail!("в архиве нет контрольной суммы сборки {t}"),
            }
            std::fs::copy(&f, kit_tmp.join(format!("hopebackup-{t}.gz")))?;
            std::fs::copy(dist.join(format!("hopebackup-{t}.gz.sha256")), kit_tmp.join(format!("hopebackup-{t}.gz.sha256")))?;
            kit.push(t.to_string());
        }
        let mine = dist.join(format!("hopebackup-{me}.gz"));
        if !mine.is_file() { bail!("в архиве нет сборки для этой машины ({me})"); }
        gunzip_to(&mine, &tmp)?;
    } else if head == [0x1f, 0x8b] {
        gunzip_to(raw, &tmp)?;
    } else {
        std::fs::copy(raw, &tmp)?;
    }
    up::check_elf(&tmp, std::env::consts::ARCH)?;
    let found = up::probe(&tmp).await?;
    let pv = parse_ver(&found).ok_or_else(|| anyhow!("не удалось определить версию пакета"))?;
    if pv < up::MIN_PANEL {
        bail!("пакет версии {found} старше {}.{}.{}: из панели ставятся версии начиная с неё; более старую ставьте скриптом hopebackup-install.sh", up::MIN_PANEL.0, up::MIN_PANEL.1, up::MIN_PANEL.2);
    }
    if let Some(w) = want.map(|w| w.trim().trim_start_matches(['v', 'V'])).filter(|w| !w.is_empty()) {
        if parse_ver(w).is_none() { bail!("некорректный номер версии"); }
        if parse_ver(w) != Some(pv) || (w.contains('-') && w != found) { bail!("в пакете другая версия, а не указанная: {found}"); }
    }
    let bin = p_package(s);
    std::fs::rename(&tmp, &bin)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o600))?;
    let _ = std::fs::remove_dir_all(p_kit(s));
    if !kit.is_empty() { std::fs::rename(&kit_tmp, p_kit(s))?; }
    let st = Staged {
        filename: filename.chars().filter(|c| !c.is_control()).take(120).collect(), version: found, size, file_sha256,
        bin_sha256: up::sha256_file(&bin)?, kit, uploaded_at: up::now(), source: source.into(),
    };
    up::save(&p_staged(s), &st);
    Ok(st)
}

fn walk_find_dist(root: &Path) -> Option<PathBuf> {
    if root.join("dist").is_dir() { return Some(root.join("dist")); }
    for e in std::fs::read_dir(root).ok()?.flatten() {
        if e.path().is_dir() && e.path().join("dist").is_dir() { return Some(e.path().join("dist")); }
    }
    None
}

// ---------------------------------------------------------------- состояние службы

pub fn start(s: Arc<Server>) {
    let a = s.clone();
    tokio::spawn(async move {
        for _ in 0..150 {
            if audit_result(&a) { break; }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            let cfg = s.store.read(|st| st.settings.updates.clone());
            if !cfg.auto_check || cfg.source_url.trim().is_empty() || !s.is_active() { continue; }
            if up::now().saturating_sub(load_check(&s).last_checked) < 24 * 3600 { continue; }
            let Some(_b) = Busy::take() else { continue };
            let c = check(&s).await;
            if c.notes_kind == "error" { tracing::warn!("проверка обновлений: {}", c.notes); }
            else if !c.latest_version.is_empty() && cmp_ver(&c.latest_version, crate::VERSION) == Ordering::Greater {
                tracing::warn!("доступна новая версия HopeBackup {} (установлена {}) — раздел «Обновления»", c.latest_version, crate::VERSION);
                super::notify::emit("updates", format!("Доступна версия {}", c.latest_version), format!("Доступна новая версия HopeBackup {} (установлена {}).{}\n\nУстановить — раздел «Обновления».", c.latest_version, crate::VERSION,
                    if c.notes.trim().is_empty() { String::new() } else { format!("\n\nЧто нового:\n{}", c.notes.trim()) }));
            }
        }
    });
}

/// Итог завершённой установки — в аудит и уведомления (один раз). true — больше нечего.
fn audit_result(s: &Server) -> bool {
    let p = p_status(s);
    let Some(mut j) = up::load::<Job>(&p) else { return true };
    if j.reported { return true; }
    if !j.finished { return !j.running; }
    let ok = !j.failed;
    let detail = if ok { format!("обновление {} → {} установлено", j.from_version, j.target_version) } else { format!("обновление до {} не установлено: {}", j.target_version, j.message) };
    s.audit("system", "update", detail.clone());
    super::notify::emit("updates", if ok { format!("Обновление {} установлено", j.target_version) } else { format!("Обновление до {} не установлено", j.target_version) },
        format!("{}\n\nПодробности — раздел «Обновления» и журнал аудита.", if ok { format!("Сервер управления обновлён: {} → {}.", j.from_version, j.target_version) } else { format!("{detail}.\nСервер работает на версии {}.", j.from_version) }));
    if ok { tracing::info!("{detail}"); } else { tracing::warn!("{detail}"); }
    j.reported = true;
    up::save(&p, &j);
    true
}

// ---------------------------------------------------------------- API

fn running(s: &Server) -> bool { load_job(s).map(|j| j.running).unwrap_or(false) }

/// Сборка агента по его сведениям: архитектура и библиотека C из отчёта (до 0.8.1 — по ОС).
fn agent_target(a: &super::state::Agent) -> String {
    if !a.build.is_empty() { return a.build.clone(); }
    let musl = a.os.to_lowercase().contains("alpine");
    let arch = if a.arch.is_empty() { "x86_64" } else { a.arch.as_str() };
    format!("{arch}-{}", if musl { "musl" } else { "gnu" })
}

pub async fn overview(State(s): State<Arc<Server>>) -> R<Json<Value>> {
    let cfg = s.store.read(|st| st.settings.updates.clone());
    let c = load_check(&s);
    let staged = load_staged(&s);
    let job = load_job(&s).filter(|j| !(j.finished && j.dismissed));
    let cur = crate::VERSION;
    let avail = !c.latest_version.is_empty() && cmp_ver(&c.latest_version, cur) == Ordering::Greater;
    let target = staged.as_ref().map(|x| x.version.clone()).or_else(|| avail.then(|| c.latest_version.clone()));
    let installer = up::installer_ready();
    let kit = crate::dist::available();
    let jobs = s.jobs.list(None, 400);
    let agents: Vec<Value> = s.store.read(|st| st.agents.clone()).iter().map(|a| {
        let t = agent_target(a);
        let last = jobs.iter().find(|j| j.agent_id == a.id && j.kind == "self_update");
        json!({ "id": a.id, "name": a.name, "host": a.host, "os": a.os, "version": a.version, "build": t, "online": super::api::agent_online(a),
            "outdated": !a.version.is_empty() && cmp_ver(&a.version, cur) == Ordering::Less, "newer": !a.version.is_empty() && cmp_ver(&a.version, cur) == Ordering::Greater,
            "kit_has": crate::dist::has(&t), "update": a.update,
            "last_job": last.map(|j| json!({ "id": j.id, "status": j.status, "error": j.error, "created": j.created, "result": j.result })) })
    }).collect();
    Ok(Json(json!({
        "current_version": cur, "build": env!("HOPEBACKUP_BUILD_TIME"), "arch": std::env::consts::ARCH, "target": crate::dist::this_target(), "node": crate::util::hostname(),
        "channel": if cfg.channel.is_empty() { "stable".to_string() } else { cfg.channel.clone() }, "source_url": cfg.source_url, "auto_check": cfg.auto_check,
        "latest_version": c.latest_version, "last_checked": c.last_checked, "notes": c.notes, "notes_kind": if c.notes_kind.is_empty() { "info".to_string() } else { c.notes_kind },
        "update_available": avail, "target_version": target.clone().unwrap_or_default(),
        "target_is_downgrade": target.as_deref().map(|t| cmp_ver(t, cur) == Ordering::Less).unwrap_or(false),
        "target_is_same": target.as_deref().map(|t| cmp_ver(t, cur) == Ordering::Equal).unwrap_or(false),
        "offline_package": staged, "updater_installed": installer.is_ok(), "updater_note": installer.err().map(|e| format!("{e:#}")).unwrap_or_default(),
        "job": job, "min_version": format!("{}.{}.{}", up::MIN_PANEL.0, up::MIN_PANEL.1, up::MIN_PANEL.2),
        "kit": kit, "agents": agents, "role": s.cluster.read().role.clone(),
        "running_jobs": jobs.iter().filter(|j| j.status == "running" || j.status == "queued").count(),
    })))
}

pub async fn do_check(State(s): State<Arc<Server>>) -> R<Json<Value>> {
    let Some(_b) = Busy::take() else { return Err(bad(BUSY_MSG)) };
    let c = check(&s).await;
    let avail = !c.latest_version.is_empty() && cmp_ver(&c.latest_version, crate::VERSION) == Ordering::Greater;
    Ok(Json(json!({ "ok": true, "update_available": avail, "latest_version": c.latest_version, "note": c.notes, "kind": c.notes_kind })))
}

#[derive(Deserialize)]
pub struct ConfigReq { channel: String, #[serde(default)] source_url: String, #[serde(default)] auto_check: bool }

pub async fn config(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Json(r): Json<ConfigReq>) -> R<Json<Value>> {
    let ch = r.channel.trim().to_lowercase();
    if ch != "stable" && ch != "beta" { return Err(bad("канал: stable или beta")); }
    let url = r.source_url.trim().to_string();
    if !url.is_empty() && !(url.starts_with("https://") || url.starts_with("http://")) { return Err(bad("адрес манифеста должен начинаться с https:// или http://")); }
    if !url.is_empty() && reqwest::Url::parse(&url).is_err() { return Err(bad("неверный адрес манифеста")); }
    let prev = s.store.read(|st| st.settings.updates.clone());
    s.store.update(|st| { st.settings.updates = UpdateSettings { channel: ch.clone(), source_url: url.clone(), auto_check: r.auto_check }; Ok(()) })?;
    if prev.source_url != url || (if prev.channel.is_empty() { "stable" } else { prev.channel.as_str() }) != ch { let _ = std::fs::remove_file(p_check(&s)); }
    s.audit(&w.user, "updates", format!("источник обновлений: {} ({ch}), автопроверка {}", if url.is_empty() { "не задан" } else { &url }, if r.auto_check { "вкл" } else { "выкл" }));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct UploadQ { #[serde(default)] version: String, #[serde(default)] filename: String }

pub async fn upload(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Query(q): Query<UploadQ>, body: Bytes) -> R<Json<Value>> {
    if body.is_empty() { return Err(bad("пустой файл")); }
    if body.len() as u64 > up::MAX_PACKAGE { return Err(bad("пакет больше 512 МБ")); }
    if running(&s) { return Err(bad("обновление уже выполняется")); }
    let Some(_b) = Busy::take() else { return Err(bad(BUSY_MSG)) };
    let d = dir(&s);
    up::ensure_dir(&d).map_err(|e| bad(e.to_string()))?;
    let raw = d.join("upload.tmp");
    std::fs::write(&raw, &body).map_err(|e| bad(format!("не удалось записать пакет: {e}")))?;
    drop(body);
    staged_reply(&s, &w, &raw, &q.filename, &q.version).await
}

/// Пакет уже лежит в `raw` (загружен целиком или собран из частей): проверить и подготовить к установке.
async fn staged_reply(s: &Arc<Server>, w: &Who, raw: &Path, filename: &str, version: &str) -> R<Json<Value>> {
    let name = if filename.trim().is_empty() { "hopebackup.tar.gz".to_string() } else { filename.trim().to_string() };
    let st = prepare(s, raw, &name, Some(version).filter(|v| !v.trim().is_empty()), "offline").await?;
    let mut note = match cmp_ver(&st.version, crate::VERSION) {
        Ordering::Less => format!("Это более старая версия, чем установленная ({}).", crate::VERSION),
        Ordering::Equal => "Эта версия уже установлена — будет переустановлена.".to_string(),
        Ordering::Greater => String::new(),
    };
    if st.kit.is_empty() { note = format!("{note} Загружена одна программа: комплект сборок для агентов не обновится — для обновления агентов загрузите архив выпуска hopebackup-{}-linux.tar.gz.", st.version).trim().to_string(); }
    s.audit(&w.user, "updates", format!("загружен пакет обновления {} ({}), SHA-256 {}", st.version, st.filename, st.file_sha256));
    Ok(Json(json!({ "ok": true, "filename": st.filename, "size": st.size, "version": st.version, "file_sha256": st.file_sha256, "kit": st.kit, "note": note })))
}

/// Адрес /api/health этого сервера для установщика.
fn health_url(s: &Server) -> String { format!("https://127.0.0.1:{}/api/health", s.listen_port) }

pub async fn do_apply(State(s): State<Arc<Server>>, Extension(w): Extension<Who>) -> R<Json<Value>> {
    if running(&s) { return Err(bad("обновление уже выполняется")); }
    up::installer_ready()?;
    let Some(busy) = Busy::take() else { return Err(bad(BUSY_MSG)) };
    let staged = load_staged(&s);
    let c = load_check(&s);
    let online = staged.is_none() && !c.latest_version.is_empty() && !c.download_url.is_empty() && cmp_ver(&c.latest_version, crate::VERSION) == Ordering::Greater;
    let target = match (&staged, online) {
        (Some(x), _) => x.version.clone(),
        (None, true) => c.latest_version.clone(),
        _ => return Err(bad("нет пакета для установки: проверьте обновления или загрузите архив выпуска")),
    };
    let now = up::now();
    up::save(&p_status(&s), &Job { phase: if online { "downloading".into() } else { "verifying".into() }, progress: if online { 3 } else { 55 }, running: true,
        target_version: target.clone(), from_version: crate::VERSION.into(), started_at: now, updated_at: now, ..Default::default() });
    s.audit(&w.user, "updates", format!("запущена установка обновления {} → {target}", crate::VERSION));
    launch(s.clone(), busy, staged, c);
    Ok(Json(json!({ "ok": true })))
}

/// Проверить пакет ещё раз, передать установщику (замена программы, перезапуск службы, откат при сбое).
fn launch(s: Arc<Server>, busy: Busy, staged: Option<Staged>, c: Check) {
    tokio::spawn(async move {
        let _busy = busy;
        let staged = match staged {
            Some(x) => x,
            None => match download(&s, &c).await { Ok(x) => x, Err(e) => { fail_job(&s, &format!("{e:#}")); return; } },
        };
        set_phase(&s, "verifying", 60);
        match up::sha256_file(&p_package(&s)) {
            Ok(h) if h == staged.bin_sha256 => {}
            _ => { fail_job(&s, "пакет изменился после проверки — загрузите его заново"); return; }
        }
        set_phase(&s, "staging", 70);
        let install = up::install_path();
        let kit = p_kit(&s);
        let req = up::Request {
            package: p_package(&s), sha256: staged.bin_sha256.clone(), target_version: staged.version.clone(), from_version: crate::VERSION.into(),
            install: install.clone(), service: up::service_name("hopebackup-server"), health: up::Health::Url { url: health_url(&s) }, status: p_status(&s),
            dist_from: kit.is_dir().then(|| kit.clone()), dist_dir: kit.is_dir().then(|| install.parent().map(|p| p.join("dist")).unwrap_or_else(|| PathBuf::from("/opt/hopebackup/dist"))),
            cleanup: vec![p_package(&s), p_staged(&s), kit], wait_secs: 90,
        };
        if let Err(e) = up::handoff(&dir(&s), &req) { tracing::warn!("обновление: {e:#}"); fail_job(&s, &format!("{e:#}")); }
        else { tracing::info!("пакет {} передан установщику: замена программы, перезапуск службы", staged.version); }
    });
}

/// Принимающий узел: архив выпуска от активного узла (подпись кластера уже проверена) — проверить как
/// обычную загрузку и установить. Только более новая версия.
pub async fn apply_from_peer(s: Arc<Server>, body: Bytes) -> Result<String> {
    if running(&s) { bail!("на этом узле уже идёт обновление"); }
    up::installer_ready()?;
    let Some(busy) = Busy::take() else { bail!("{BUSY_MSG}") };
    let d = dir(&s);
    up::ensure_dir(&d)?;
    let raw = d.join("upload.tmp");
    std::fs::write(&raw, &body).context("запись пакета")?;
    drop(body);
    let st = prepare(&s, &raw, "от активного узла.tar.gz", None, "cluster").await?;
    if cmp_ver(&st.version, crate::VERSION) != Ordering::Greater {
        let _ = std::fs::remove_file(p_package(&s));
        let _ = std::fs::remove_file(p_staged(&s));
        let _ = std::fs::remove_dir_all(p_kit(&s));
        bail!("пакет версии {} не новее установленной здесь {}", st.version, crate::VERSION);
    }
    let now = up::now();
    up::save(&p_status(&s), &Job { phase: "verifying".into(), progress: 55, running: true, target_version: st.version.clone(),
        from_version: crate::VERSION.into(), started_at: now, updated_at: now, ..Default::default() });
    s.audit("system", "updates", format!("обновление от активного узла управления: {} → {}", crate::VERSION, st.version));
    let v = st.version.clone();
    launch(s, busy, Some(st), Check::default());
    Ok(v)
}

/// Архив выпуска из того, что работает на этом сервере: его программа и комплект сборок для других машин
/// (в том же виде, что hopebackup-<версия>-linux.tar.gz) — для обновления принимающих узлов.
pub async fn own_release_archive(s: &Server) -> Result<PathBuf> {
    let d = dir(s);
    up::ensure_dir(&d)?;
    let out = d.join("peer-release.tar.gz");
    let o = out.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use sha2::Digest;
        use std::io::Write;
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(&o)?, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let root = format!("hopebackup-{}", crate::VERSION);
        let mut add = |name: String, data: &[u8]| -> Result<()> {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(up::now());
            h.set_cksum();
            tar.append_data(&mut h, format!("{root}/{name}"), data)?;
            Ok(())
        };
        add("VERSION".into(), format!("{}\n", crate::VERSION).as_bytes())?;
        let mut n = 0;
        for t in crate::dist::TARGETS {
            if !crate::dist::has(t) { continue; }
            // Своя сборка — работающая программа; остальные — из комплекта (распаковать и сжать заново: проверка ELF).
            let bin = crate::dist::load(t)?;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&bin)?;
            let gzb = e.finish()?;
            let sum = hex::encode(sha2::Sha256::digest(&gzb));
            add(format!("dist/hopebackup-{t}.gz"), &gzb)?;
            add(format!("dist/hopebackup-{t}.gz.sha256"), format!("{sum}  hopebackup-{t}.gz\n").as_bytes())?;
            n += 1;
        }
        if n == 0 { bail!("нет ни одной сборки для передачи"); }
        tar.into_inner()?.finish()?;
        Ok(())
    })
    .await??;
    Ok(out)
}

pub async fn discard(State(s): State<Arc<Server>>, Extension(w): Extension<Who>) -> R<Json<Value>> {
    if running(&s) { return Err(bad("обновление уже выполняется")); }
    let Some(_b) = Busy::take() else { return Err(bad(BUSY_MSG)) };
    let had = load_staged(&s);
    let _ = std::fs::remove_file(p_package(&s));
    let _ = std::fs::remove_file(p_staged(&s));
    let _ = std::fs::remove_dir_all(p_kit(&s));
    if let Some(x) = had { s.audit(&w.user, "updates", format!("удалён пакет обновления {}", x.version)); }
    Ok(Json(json!({ "ok": true })))
}

pub async fn dismiss(State(s): State<Arc<Server>>) -> R<Json<Value>> {
    let p = p_status(&s);
    if let Some(mut j) = up::load::<Job>(&p) { if j.finished { j.dismissed = true; up::save(&p, &j); } }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- агенты

#[derive(Deserialize)]
pub struct AgentsReq {
    /// Агенты; пусто — все устаревшие и на связи.
    #[serde(default)]
    ids: Vec<String>,
}

pub async fn update_agents(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, Json(q): Json<AgentsReq>) -> R<Json<Value>> {
    if !s.is_active() { return Err(bad("агентов обновляет активный узел управления")); }
    let cur = crate::VERSION;
    let agents = s.store.read(|st| st.agents.clone());
    let pick: Vec<_> = agents.iter().filter(|a| if q.ids.is_empty() { !a.version.is_empty() && cmp_ver(&a.version, cur) == Ordering::Less && super::api::agent_online(a) } else { q.ids.contains(&a.id) }).cloned().collect();
    if pick.is_empty() { return Err(bad("нет агентов для обновления: все на связи и на версии сервера")); }
    let jobs = s.jobs.list(None, 2000);
    let mut out = Vec::new();
    for a in pick {
        let t = agent_target(&a);
        let res: Result<String> = (|| {
            if jobs.iter().any(|j| j.agent_id == a.id && (j.status == "running" || j.status == "queued")) { bail!("на агенте выполняются задания — обновите его позже"); }
            let data = crate::dist::load(&t).with_context(|| format!("сборка {t}"))?;
            let sha = { use sha2::Digest; hex::encode(sha2::Sha256::digest(&data)) };
            let spec = JobSpec { id: String::new(), kind: "self_update".into(), instance: None, site: None, snapshot_id: None, mode: String::new(),
                params: json!({ "version": cur, "target": t, "sha256": sha, "size": data.len() }) };
            Ok(s.jobs.enqueue(spec, format!("Обновление агента «{}» до {cur}", a.name), &a.id, &w.user))
        })();
        match res {
            Ok(j) => { s.audit(&w.user, "updates", format!("обновление агента {} ({}) {} → {cur}", a.name, a.id, a.version)); out.push(json!({ "agent": a.id, "name": a.name, "job_id": j })); }
            Err(e) => out.push(json!({ "agent": a.id, "name": a.name, "error": format!("{e:#}") })),
        }
    }
    Ok(Json(json!({ "jobs": out })))
}

/// Сборка программы для агента (агент скачивает её по заданию `self_update`).
pub async fn agent_package(State(s): State<Arc<Server>>, h: HeaderMap, AxPath(target): AxPath<String>) -> Response {
    if super::agent_api::agent_of(&s, &h).is_err() { return (StatusCode::UNAUTHORIZED, "неизвестный агент").into_response(); }
    if !crate::dist::TARGETS.contains(&target.as_str()) { return (StatusCode::BAD_REQUEST, "неизвестная сборка").into_response(); }
    match tokio::task::spawn_blocking(move || crate::dist::load(&target)).await {
        Ok(Ok(data)) => ([("content-type", "application/octet-stream"), ("x-hopebackup-version", crate::VERSION)], data).into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------- загрузка частями (с продолжением после обрыва)

/// Размер части: укладывается в общий лимит тела запроса и проходит даже нестабильный VPN.
const CHUNK: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UploadSession {
    id: String,
    filename: String,
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    version: String,
    created: u64,
}

fn p_session(s: &Server) -> PathBuf { dir(s).join("upload-session.json") }
fn p_part(s: &Server) -> PathBuf { dir(s).join("upload.part") }

fn session(s: &Server, id: &str) -> R<UploadSession> {
    up::load::<UploadSession>(&p_session(s)).filter(|x| x.id == id)
        .ok_or_else(|| bad("загрузка не найдена (начата заново в другой вкладке или устарела) — выберите файл и загрузите снова"))
}

fn part_len(s: &Server) -> u64 { std::fs::metadata(p_part(s)).map(|m| m.len()).unwrap_or(0) }

#[derive(Deserialize)]
pub struct StartReq {
    filename: String,
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    version: String,
    /// Продолжить прерванную загрузку того же файла.
    #[serde(default)]
    resume: String,
}

/// Начать загрузку пакета частями или продолжить прерванную (тот же файл: имя, размер, SHA-256).
pub async fn upload_start(State(s): State<Arc<Server>>, Json(q): Json<StartReq>) -> R<Json<Value>> {
    if q.size == 0 { return Err(bad("пустой файл")); }
    if q.size > up::MAX_PACKAGE { return Err(bad("пакет больше 512 МБ")); }
    if running(&s) { return Err(bad("обновление уже выполняется")); }
    let sha = q.sha256.trim().to_lowercase();
    if !sha.is_empty() && (sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit())) { return Err(bad("неверная контрольная сумма")); }
    let d = dir(&s);
    up::ensure_dir(&d).map_err(|e| bad(e.to_string()))?;
    if !q.resume.is_empty() {
        if let Some(x) = up::load::<UploadSession>(&p_session(&s)) {
            let have = part_len(&s);
            if x.id == q.resume && x.size == q.size && x.filename == q.filename && x.sha256 == sha && have <= x.size {
                return Ok(Json(json!({ "id": x.id, "received": have, "size": x.size, "chunk": CHUNK, "resumed": true })));
            }
        }
    }
    let x = UploadSession { id: crate::util::short_id(), filename: q.filename.chars().filter(|c| !c.is_control()).take(120).collect(), size: q.size, sha256: sha, version: q.version.trim().to_string(), created: up::now() };
    std::fs::write(p_part(&s), b"").map_err(|e| bad(format!("не удалось создать файл загрузки: {e}")))?;
    up::save(&p_session(&s), &x);
    Ok(Json(json!({ "id": x.id, "received": 0, "size": x.size, "chunk": CHUNK, "resumed": false })))
}

#[derive(Deserialize)]
pub struct ChunkQ { offset: u64 }

/// Часть файла с позиции `offset`. Позиция не совпала (часть уже дошла, а ответ потерялся) — ничего не пишется,
/// в ответе — сколько уже принято, клиент продолжает оттуда.
pub async fn upload_chunk(State(s): State<Arc<Server>>, AxPath(id): AxPath<String>, Query(q): Query<ChunkQ>, body: Bytes) -> R<Json<Value>> {
    let x = session(&s, &id)?;
    let have = part_len(&s);
    if q.offset != have || body.is_empty() { return Ok(Json(json!({ "received": have, "size": x.size }))); }
    if body.len() as u64 > CHUNK || have + body.len() as u64 > x.size { return Err(bad("часть больше ожидаемого")); }
    let p = p_part(&s);
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p)?;
        f.write_all(&body)?;
        f.sync_data()
    }).await.map_err(|e| bad(e.to_string()))?.map_err(|e| bad(format!("запись части: {e}")))?;
    Ok(Json(json!({ "received": part_len(&s), "size": x.size })))
}

/// Все части приняты: сверить размер и SHA-256, проверить пакет как при обычной загрузке.
pub async fn upload_finish(State(s): State<Arc<Server>>, Extension(w): Extension<Who>, AxPath(id): AxPath<String>) -> R<Json<Value>> {
    let x = session(&s, &id)?;
    let have = part_len(&s);
    if have != x.size { return Err(bad(format!("передано {} из {} — продолжите загрузку", have, x.size))); }
    if running(&s) { return Err(bad("обновление уже выполняется")); }
    let Some(_b) = Busy::take() else { return Err(bad(BUSY_MSG)) };
    if !x.sha256.is_empty() {
        let p = p_part(&s);
        let got = tokio::task::spawn_blocking(move || up::sha256_file(&p)).await.map_err(|e| bad(e.to_string()))?.map_err(|e| bad(e.to_string()))?;
        if got != x.sha256 {
            let _ = std::fs::remove_file(p_part(&s));
            let _ = std::fs::remove_file(p_session(&s));
            return Err(bad("файл на сервере не совпал с исходным по контрольной сумме (повреждён при передаче) — загрузите его заново"));
        }
    }
    let raw = dir(&s).join("upload.tmp");
    std::fs::rename(p_part(&s), &raw).map_err(|e| bad(e.to_string()))?;
    let _ = std::fs::remove_file(p_session(&s));
    staged_reply(&s, &w, &raw, &x.filename, &x.version).await
}
