//! Обновление программы на месте — общая часть для сервера управления и агента (как в HopeDFS и HopeDC).
//!
//! Пакет проверяется до установки: исполняемый файл Linux нужной архитектуры, пробный запуск `--version`
//! от непривилегированного пользователя, номер версии. Установку выполняет отдельный процесс — копия
//! **прежней** программы с командой `apply-update`, запущенная вне службы (`systemd-run` или `setsid`):
//! перезапуск службы её не останавливает. Установщик сверяет SHA-256, сохраняет прежнюю программу
//! (`<путь>.prev`, комплект сборок — `dist.prev`), ставит новую, перезапускает службу и ждёт, пока она
//! ответит **новой** версией; иначе возвращает прежнюю и перезапускает службу снова (автоматический откат).
//!
//! Ход установки — `status.json` в каталоге обновлений (пишут и служба, и установщик).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Предел размера пакета (архив выпуска со всеми сборками — десятки МБ).
pub const MAX_PACKAGE: u64 = 512 << 20;
/// Самая старая версия, которую можно поставить из панели (в ней есть раздел «Обновления»).
pub const MIN_PANEL: (u32, u32, u32) = (0, 8, 1);

// ---------------------------------------------------------------- версии

/// "1.2.3", "v1.2", "1.2.0-beta" → (1, 2, 3).
pub fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+', ' ']).next()?;
    let mut it = core.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    let c = it.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    Some((a, b, c))
}

pub fn cmp_ver(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_ver(a), parse_ver(b)) {
        // 1.2.0-beta < 1.2.0
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| match (a.contains('-'), b.contains('-')) {
            (false, false) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (true, true) => a.cmp(b),
        }),
        _ => a.cmp(b),
    }
}

// ---------------------------------------------------------------- проверка программы

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop { let n = f.read(&mut buf)?; if n == 0 { break; } h.update(&buf[..n]); }
    Ok(hex::encode(h.finalize()))
}

fn elf_machine(arch: &str) -> u16 { match arch { "x86_64" => 0x3E, "aarch64" => 0xB7, _ => 0 } }

/// Исполняемый файл Linux (ELF, 64 бита) для архитектуры `arch`.
pub fn check_elf(p: &Path, arch: &str) -> Result<()> {
    let mut h = [0u8; 20];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut h)).unwrap_or(0);
    if n < 20 || &h[0..4] != b"\x7fELF" { bail!("файл не является программой Linux — нужен архив выпуска hopebackup-<версия>-linux.tar.gz, hopebackup.gz или распакованный hopebackup"); }
    let machine = u16::from_le_bytes([h[18], h[19]]);
    if h[4] != 2 || h[5] != 1 || machine != elf_machine(arch) { bail!("программа собрана для другой архитектуры процессора (нужна {arch})"); }
    Ok(())
}

/// Пробный запуск `<bin> --version` (от nobody, если мы root) → номер версии.
pub async fn probe(bin: &Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let td = std::env::temp_dir().join(format!("hopebackup-probe-{}", crate::util::short_id()));
    std::fs::create_dir_all(&td)?;
    std::fs::set_permissions(&td, std::fs::Permissions::from_mode(0o755))?;
    let exe = td.join("hopebackup");
    let res = async {
        std::fs::copy(bin, &exe)?;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.arg("--version").current_dir(&td).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").stdin(std::process::Stdio::null()).kill_on_drop(true);
        if unsafe { libc::geteuid() } == 0 { cmd.uid(65534).gid(65534); }
        let out = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await
            .map_err(|_| anyhow::anyhow!("новая программа не отвечает на --version за 15 с"))?
            .context("новая программа не запускается на этой машине")?;
        if !out.status.success() {
            let e = String::from_utf8_lossy(&out.stderr).trim().chars().take(300).collect::<String>();
            bail!("новая программа не запускается на этой машине: {}", if e.is_empty() { format!("код {}", out.status.code().unwrap_or(-1)) } else { e });
        }
        let line = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
        let v = line.strip_prefix("hopebackup ").and_then(|r| r.split_whitespace().next()).map(String::from)
            .ok_or_else(|| anyhow::anyhow!("это не программа HopeBackup (на --version она назвалась «{line}»)"))?;
        if parse_ver(&v).is_none() { bail!("непонятный номер версии «{v}»"); }
        Ok(v)
    }.await;
    let _ = std::fs::remove_dir_all(&td);
    res
}

// ---------------------------------------------------------------- ход установки

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Job {
    /// downloading, verifying, staging, handoff, applying, restarting, rolling_back, done, failed, rolled_back, rollback_failed
    pub phase: String,
    pub progress: u8,
    pub running: bool,
    pub finished: bool,
    pub failed: bool,
    pub message: String,
    pub target_version: String,
    pub from_version: String,
    pub started_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub dismissed: bool,
    /// Итог записан в журнал аудита (сервер) или сообщён серверу (агент).
    #[serde(default)]
    pub reported: bool,
}

pub fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

pub fn load<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> { std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()) }

pub fn save<T: Serialize>(p: &Path, v: &T) {
    if let Some(d) = p.parent() { let _ = ensure_dir(d); }
    let _ = crate::util::write_json(p, v, true);
}

pub fn ensure_dir(d: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(d)?;
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
}

pub fn update_job(p: &Path, f: impl FnOnce(&mut Job)) {
    let mut j: Job = load(p).unwrap_or_default();
    f(&mut j);
    j.updated_at = now();
    save(p, &j);
}

// ---------------------------------------------------------------- установщик

/// Как понять, что новая версия заработала.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Health {
    /// Сервер управления: `/api/health` с заголовком `x-hopebackup-version`.
    Url { url: String },
    /// Агент: после первого удачного опроса сервера пишет файл с версией и временем запуска.
    Marker { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub package: PathBuf,
    pub sha256: String,
    pub target_version: String,
    pub from_version: String,
    /// Путь установленной программы (/opt/hopebackup/hopebackup).
    pub install: PathBuf,
    /// Служба: hopebackup-server или hopebackup-agent.
    pub service: String,
    pub health: Health,
    pub status: PathBuf,
    /// Подготовленный комплект сборок для машин (сервер): заменяет `dist_dir` целиком.
    #[serde(default)]
    pub dist_from: Option<PathBuf>,
    #[serde(default)]
    pub dist_dir: Option<PathBuf>,
    /// Файлы, которые убрать после удачной установки.
    #[serde(default)]
    pub cleanup: Vec<PathBuf>,
    #[serde(default = "d_wait")]
    pub wait_secs: u64,
}
fn d_wait() -> u64 { 90 }

/// Путь программы текущего процесса (без « (deleted)», если файл уже заменяли).
pub fn install_path() -> PathBuf {
    let p = std::fs::read_link("/proc/self/exe").or_else(|_| std::env::current_exe()).unwrap_or_else(|_| PathBuf::from("/opt/hopebackup/hopebackup"));
    let s = p.to_string_lossy().to_string();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s))
}

pub fn has_systemd() -> bool { Path::new("/run/systemd/system").is_dir() && which("systemctl").is_some() }
pub fn has_openrc() -> bool { Path::new("/sbin/openrc-run").exists() && which("rc-service").is_some() }

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = path.split(':').chain(["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"]).map(|d| Path::new(d).join(name)).find(|p| p.is_file());
    found
}

/// Служба, в которой работает процесс (по cgroup или по умолчанию).
pub fn service_name(default: &str) -> String {
    if let Ok(v) = std::env::var("HOPEBACKUP_SERVICE") { if !v.is_empty() { return v; } }
    if let Ok(cg) = std::fs::read_to_string("/proc/self/cgroup") {
        for seg in cg.split(['/', '\n']) {
            if let Some(n) = seg.strip_suffix(".service") { if n.starts_with("hopebackup-") { return n.to_string(); } }
        }
    }
    default.to_string()
}

/// Можно ли установить из панели: служба под systemd или OpenRC.
pub fn installer_ready() -> Result<()> {
    if std::env::var("HOPEBACKUP_UPDATER_FORCE").as_deref() == Ok("1") { return Ok(()); }
    if has_systemd() && which("systemd-run").is_some() { return Ok(()); }
    if has_openrc() && which("setsid").is_some() { return Ok(()); }
    bail!("служба работает не под systemd и не под OpenRC — установка отсюда недоступна; обновите скриптом hopebackup-install.sh")
}

/// Передать задание установщику: копия текущей программы с `apply-update`, вне службы.
pub fn handoff(dir: &Path, req: &Request) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    installer_ready()?;
    ensure_dir(dir)?;
    let applier = dir.join("hopebackup-applier");
    std::fs::copy("/proc/self/exe", &applier).or_else(|_| std::fs::copy(install_path(), &applier)).context("копия установщика")?;
    std::fs::set_permissions(&applier, std::fs::Permissions::from_mode(0o700))?;
    let rp = dir.join("apply.request");
    crate::util::write_json(&rp, req, true).context("задание установщику")?;
    update_job(&req.status, |j| { j.phase = "handoff".into(); j.progress = 75; });
    let forced = std::env::var("HOPEBACKUP_UPDATER_FORCE").as_deref() == Ok("1");
    let out = if has_systemd() && which("systemd-run").is_some() && !forced {
        let unit = format!("hopebackup-update-{}", now());
        std::process::Command::new("systemd-run")
            .args(["--unit", &unit, "--collect", "--quiet", "--description", "HopeBackup update", "-p", "SyslogIdentifier=hopebackup-update"])
            .arg(&applier).arg("apply-update").arg(&rp).stdin(std::process::Stdio::null()).output().context("systemd-run")?
    } else {
        // OpenRC (Alpine) или проверка: отдельный сеанс — остановка службы его не задевает.
        let log = dir.join("apply.log");
        let sh = format!("nohup {} apply-update {} >> {} 2>&1 &", crate::util::shq(&applier.to_string_lossy()), crate::util::shq(&rp.to_string_lossy()), crate::util::shq(&log.to_string_lossy()));
        std::process::Command::new("setsid").args(["sh", "-c", &sh]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().context("setsid")?
    };
    if !out.status.success() { bail!("установщик не запущен: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

fn log(m: &str) { println!("{} {m}", chrono::Local::now().format("%H:%M:%S")); }

fn put(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = PathBuf::from(format!("{}.new", dst.display()));
    std::fs::copy(src, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, dst)
}

fn restart(service: &str) -> std::result::Result<(), String> {
    if std::env::var("HOPEBACKUP_UPDATER_RESTART").map(|c| !c.is_empty()).unwrap_or(false) {
        // Проверочный стенд: своя команда перезапуска.
        let c = std::env::var("HOPEBACKUP_UPDATER_RESTART").unwrap_or_default();
        let o = std::process::Command::new("sh").args(["-c", &c]).output().map_err(|e| e.to_string())?;
        return if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) };
    }
    if has_systemd() {
        let _ = std::process::Command::new("systemctl").args(["reset-failed", service]).output();
        let o = std::process::Command::new("systemctl").args(["restart", service]).output().map_err(|e| e.to_string())?;
        if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
    } else {
        let o = std::process::Command::new("rc-service").args([service, "restart"]).output().map_err(|e| e.to_string())?;
        if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
    }
}

/// Узел хранения на той же машине работает той же программой — после обновления перезапустить и его (если запущен),
/// иначе он останется в памяти прежней версией.
fn restart_siblings(service: &str, log: &dyn Fn(&str)) {
    if std::env::var("HOPEBACKUP_UPDATER_RESTART").map(|c| !c.is_empty()).unwrap_or(false) { return; }
    for sib in ["hopebackup-node"] {
        if sib == service { continue; }
        let active = if has_systemd() {
            std::process::Command::new("systemctl").args(["is-active", "--quiet", sib]).status().map(|s| s.success()).unwrap_or(false)
        } else if has_openrc() {
            std::process::Command::new("rc-service").args([sib, "status"]).output().map(|o| o.status.success()).unwrap_or(false)
        } else { false };
        if !active { continue; }
        match restart(sib) {
            Ok(()) => log(&format!("служба {sib} на этой машине перезапущена новой программой")),
            Err(e) => log(&format!("служба {sib}: перезапуск не удался ({e}) — перезапустите вручную")),
        }
    }
}

/// Ждать ответа новой версии.
fn wait_healthy(rt: &tokio::runtime::Runtime, h: &Health, version: &str, since: u64, secs: u64) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(secs);
    match h {
        Health::Url { url } => {
            let cl = reqwest::Client::builder().danger_accept_invalid_certs(true).timeout(Duration::from_secs(3)).no_proxy().build().unwrap();
            rt.block_on(async {
                // Первые секунды может отвечать ещё прежний процесс — нужна именно новая версия.
                while std::time::Instant::now() < until {
                    if let Ok(r) = cl.get(url).send().await {
                        let v = r.headers().get("x-hopebackup-version").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
                        if r.status().is_success() && v == version { return true; }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                false
            })
        }
        Health::Marker { path } => {
            while std::time::Instant::now() < until {
                if let Some(m) = load::<serde_json::Value>(path) {
                    if m["version"] == version && m["at"].as_u64().unwrap_or(0) >= since { return true; }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            false
        }
    }
}

/// Заменить каталог комплекта сборок: прежний — в `<каталог>.prev`.
fn swap_dist(from: &Path, to: &Path) -> std::io::Result<()> {
    let prev = PathBuf::from(format!("{}.prev", to.display()));
    let _ = std::fs::remove_dir_all(&prev);
    if to.exists() { std::fs::rename(to, &prev)?; }
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() { std::fs::copy(e.path(), to.join(e.file_name()))?; }
    Ok(())
}

fn unswap_dist(to: &Path) {
    let prev = PathBuf::from(format!("{}.prev", to.display()));
    if prev.exists() { let _ = std::fs::remove_dir_all(to); let _ = std::fs::rename(&prev, to); }
}

/// Сам установщик: `hopebackup apply-update <задание>`.
pub fn run(req_path: &str) -> i32 {
    let Some(mut req) = load::<Request>(Path::new(req_path)) else { log("некорректное задание на обновление"); return 2 };
    let _ = std::fs::remove_file(req_path);
    // Проверочный стенд: короче ожидание ответа новой версии.
    if let Some(w) = std::env::var("HOPEBACKUP_UPDATER_WAIT").ok().and_then(|v| v.parse().ok()) { req.wait_secs = w; }
    let st = req.status.clone();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
    let fail = |phase: &str, msg: &str| {
        log(msg);
        update_job(&st, |j| { j.phase = phase.into(); j.running = false; j.finished = true; j.failed = true; j.message = msg.into(); });
    };
    log(&format!("HopeBackup: установка {} → {} ({}, служба {})", req.from_version, req.target_version, req.install.display(), req.service));
    update_job(&st, |j| { j.phase = "applying".into(); j.progress = 82; });
    match sha256_file(&req.package) {
        Ok(h) if h == req.sha256 => {}
        Ok(_) => { fail("failed", "контрольная сумма пакета не совпала при установке — обновление не выполнено"); return 1; }
        Err(_) => { fail("failed", "пакет для установки не найден — обновление не выполнено"); return 1; }
    }
    let prev = PathBuf::from(format!("{}.prev", req.install.display()));
    if let Err(e) = std::fs::copy(&req.install, &prev) { fail("failed", &format!("не удалось сохранить прежнюю программу: {e}")); return 1; }
    if let (Some(f), Some(t)) = (&req.dist_from, &req.dist_dir) {
        if let Err(e) = swap_dist(f, t) { unswap_dist(t); fail("failed", &format!("не удалось обновить комплект сборок {}: {e}", t.display())); return 1; }
        log(&format!("комплект сборок для машин обновлён: {}", t.display()));
    }
    if let Err(e) = put(&req.package, &req.install) {
        if let Some(t) = &req.dist_dir { unswap_dist(t); }
        fail("failed", &format!("не удалось записать новую программу: {e}"));
        return 1;
    }
    update_job(&st, |j| { j.phase = "restarting".into(); j.progress = 92; });
    let since = now();
    log(&format!("перезапуск службы {}", req.service));
    let started = restart(&req.service).map_err(|e| log(&format!("перезапуск: {e}"))).is_ok();
    if started && wait_healthy(&rt, &req.health, &req.target_version, since, req.wait_secs) {
        restart_siblings(&req.service, &|m: &str| log(m));
        for c in &req.cleanup { if c.is_dir() { let _ = std::fs::remove_dir_all(c); } else { let _ = std::fs::remove_file(c); } }
        let msg = format!("обновление до {} установлено", req.target_version);
        log(&msg);
        update_job(&st, |j| { j.phase = "done".into(); j.progress = 100; j.running = false; j.finished = true; j.failed = false; j.message = msg.clone(); });
        return 0;
    }
    log("новая версия не ответила — откат на прежнюю программу");
    update_job(&st, |j| { j.phase = "rolling_back".into(); j.progress = 95; });
    if let Some(t) = &req.dist_dir { unswap_dist(t); }
    let since = now();
    let ok = put(&prev, &req.install).is_ok() && restart(&req.service).is_ok() && wait_healthy(&rt, &req.health, &req.from_version, since, req.wait_secs);
    let journal = if has_systemd() { format!("journalctl -u {}", req.service) } else { format!("/var/log/{}.log", req.service) };
    if ok {
        fail("rolled_back", &format!("новая версия {} не запустилась — восстановлена прежняя {}, служба работает. Причина — в журнале: {journal}", req.target_version, req.from_version));
        1
    } else {
        fail("rollback_failed", &format!("новая версия не запустилась, и откат не удался — нужна проверка на машине: {journal}; прежняя программа — {}", prev.display()));
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert_eq!(parse_ver("v0.8"), Some((0, 8, 0)));
        assert_eq!(parse_ver("1.10.3-beta"), Some((1, 10, 3)));
        assert_eq!(parse_ver("abc"), None);
        assert_eq!(cmp_ver("0.10.0", "0.9.9"), std::cmp::Ordering::Greater);
        assert_eq!(cmp_ver("0.9.0-beta", "0.9.0"), std::cmp::Ordering::Less);
        assert_eq!(cmp_ver("0.8.1", "0.8.1"), std::cmp::Ordering::Equal);
    }
}
