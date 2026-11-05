//! Мелкие утилиты: имя узла, адреса, секретные файлы, случайные токены, запуск команд.

use anyhow::{bail, Context, Result};
use rand::RngCore;
use std::io::Write;
use std::path::Path;

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".into())
}

/// IPv4-адреса интерфейсов (кроме петлевого).
pub fn local_ips() -> Vec<String> {
    let mut v = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 {
            let mut p = ifap;
            while !p.is_null() {
                let a = (*p).ifa_addr;
                if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                    let sin = a as *const libc::sockaddr_in;
                    let ip = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                    if !ip.is_loopback() { v.push(ip.to_string()); }
                }
                p = (*p).ifa_next;
            }
            libc::freeifaddrs(ifap);
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Записать файл с правами 0600 (ключи, токены, пароли).
pub fn write_secret(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("запись {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Записать JSON атомарно (через временный файл).
pub fn write_json<T: serde::Serialize>(path: &Path, v: &T, secret: bool) -> Result<()> {
    let data = serde_json::to_vec_pretty(v)?;
    if secret { return write_secret(path, &data); }
    if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Случайный токен (base64url, 32 байта энтропии). Не начинается с «-»: такой токен в командной строке
/// (`agent enroll --token …`) разбирался бы как ключ команды.
pub fn token() -> String {
    use base64::Engine;
    loop {
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        let t = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        if !t.starts_with('-') { return t; }
    }
}

pub fn short_id() -> String { uuid::Uuid::new_v4().simple().to_string()[..12].to_string() }

pub fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// uid/gid пользователя ОС по имени.
pub fn user_ids(name: &str) -> Option<(u32, u32)> {
    let c = std::ffi::CString::new(name).ok()?;
    unsafe {
        let pw = libc::getpwnam(c.as_ptr());
        if pw.is_null() { None } else { Some(((*pw).pw_uid, (*pw).pw_gid)) }
    }
}

/// Имя пользователя ОС по uid.
pub fn user_name(uid: u32) -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() { return None; }
        Some(std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().to_string())
    }
}

pub fn is_root() -> bool { unsafe { libc::geteuid() == 0 } }

/// Рекурсивно сменить владельца.
pub fn chown_r(path: &Path, uid: u32, gid: u32) -> Result<()> {
    fn walk(p: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
        std::os::unix::fs::lchown(p, Some(uid), Some(gid))?;
        let md = std::fs::symlink_metadata(p)?;
        if md.is_dir() {
            for e in std::fs::read_dir(p)? { walk(&e?.path(), uid, gid)?; }
        }
        Ok(())
    }
    walk(path, uid, gid).with_context(|| format!("смена владельца {}", path.display()))
}

/// Безопасно экранировать строку для оболочки.
pub fn shq(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+".contains(c)) { return s.to_string(); }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Выполнить команду и вернуть stdout; ошибка с stderr при ненулевом коде.
pub async fn run(cmd: &mut tokio::process::Command) -> Result<String> {
    let out = cmd.output().await.with_context(|| format!("запуск {:?}", cmd.as_std().get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let so = String::from_utf8_lossy(&out.stdout);
        bail!("{:?} завершилась с кодом {}: {}{}", cmd.as_std().get_program(), out.status.code().unwrap_or(-1), err.trim(), if err.trim().is_empty() { so.trim().to_string() } else { String::new() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn human_bytes(n: u64) -> String {
    let u = ["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < u.len() - 1 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{n} {}", u[0]) } else { format!("{v:.1} {}", u[i]) }
}

/// Имя пользователя SSH: буквы, цифры, `._-` (не с `-`), чтобы его нельзя было принять за опцию ssh.
pub fn valid_ssh_user(u: &str) -> Result<()> {
    let ok = !u.is_empty() && u.len() <= 64 && !u.starts_with('-') && u.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok { anyhow::bail!("недопустимое имя пользователя SSH «{u}»: только латинские буквы, цифры и . _ - (не с «-»)"); }
    Ok(())
}

/// Адрес узла: имя DNS, IPv4 или IPv6 (без пробелов, `@`, начального `-` и т. п.).
pub fn valid_host(h: &str) -> Result<()> {
    let ok = !h.is_empty() && h.len() <= 253 && !h.starts_with('-') && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:_[]%".contains(c));
    if !ok { anyhow::bail!("недопустимый адрес узла «{h}»"); }
    Ok(())
}

/// Внешняя команда с пределом времени: stdin закрыт, stdout и stderr читаются (не наследуются —
/// иначе вывод команды смешался бы с JSON описи). По истечении `secs` процесс убивается.
pub fn run_timeout(prog: &str, args: &[&str], secs: u64) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut ch = Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let mut so = ch.stdout.take()?;
    let mut se = ch.stderr.take()?;
    let t1 = std::thread::spawn(move || { let mut b = Vec::new(); let _ = so.by_ref().take(64 << 20).read_to_end(&mut b); b });
    let t2 = std::thread::spawn(move || { let mut b = Vec::new(); let _ = se.by_ref().take(1 << 20).read_to_end(&mut b); b });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match ch.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => { let _ = ch.kill(); let _ = ch.wait(); return None; }
        }
    };
    Some(std::process::Output { status, stdout: t1.join().unwrap_or_default(), stderr: t2.join().unwrap_or_default() })
}

/// Процесс выполняет копии и восстановление (агент, `restore-exec`): ждать площадку долго.
pub static PATIENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Сколько ждать возвращения площадки при обрыве связи: у агента — 10 минут (копия переживает короткий
/// обрыв), у сервера управления — 30 с (проверки из панели не должны зависать). На стенде у агента —
/// `HOPEBACKUP_TEST_NET_PATIENCE` секунд при `HOPEBACKUP_TESTING=1`.
/// Память процесса возвращается системе после больших операций (glibc по умолчанию держит её в своих
/// аренах: после копии агент так и оставался бы на сотнях мегабайт). Буферы от 8 МиБ (pack-файлы) — отдельными
/// отображениями (освобождаются сразу), не больше 4 арен на все потоки.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 8 << 20);
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }
}

/// Вернуть системе свободную память куч (после задания).
pub fn trim_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

pub fn net_patience() -> std::time::Duration {
    if !PATIENT.load(std::sync::atomic::Ordering::Relaxed) { return std::time::Duration::from_secs(30); }
    let mut secs = 600u64;
    if std::env::var("HOPEBACKUP_TESTING").as_deref() == Ok("1") {
        if let Some(v) = std::env::var("HOPEBACKUP_TEST_NET_PATIENCE").ok().and_then(|v| v.parse().ok()) { secs = v; }
    }
    std::time::Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_never_start_with_dash() {
        for _ in 0..5000 { let t = super::token(); assert!(!t.starts_with('-'), "{t}"); assert_eq!(t.len(), 43); }
    }
}
