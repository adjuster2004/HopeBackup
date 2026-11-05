//! Комплект программ для машин: x86_64 и aarch64 (ARM64), glibc ≥ 2.17 и musl (статическая сборка — для
//! Alpine без gcompat и любых Linux, где glibc старше 2.17 или её нет).
//!
//! Сервер управления берёт программу для машины так: своя (если совпадают архитектура и библиотека C), иначе
//! из каталога комплекта — `$HOPEBACKUP_DIST`, `<каталог программы>/dist`, `/opt/hopebackup/dist`,
//! `<данные>/dist`. Файлы: `hopebackup-<арх>-<libc>` или `hopebackup-<арх>-<libc>.gz`, например
//! `hopebackup-aarch64-gnu.gz`, `hopebackup-x86_64-musl.gz` (так они лежат в `bin/dist` комплекта).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// Сборка, которая сейчас работает: `x86_64-gnu`, `aarch64-musl`…
pub fn this_target() -> String {
    let arch = std::env::consts::ARCH;
    let libc = if cfg!(target_env = "musl") { "musl" } else { "gnu" };
    format!("{arch}-{libc}")
}

/// Все сборки, о которых знает комплект.
pub const TARGETS: &[&str] = &["x86_64-gnu", "x86_64-musl", "aarch64-gnu", "aarch64-musl"];

pub fn dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("HOPEBACKUP_DIST") { v.push(PathBuf::from(d)); }
    if let Ok(e) = std::env::current_exe() { if let Some(p) = e.parent() { v.push(p.join("dist")); } }
    v.push(PathBuf::from("/opt/hopebackup/dist"));
    v.push(crate::agent::data_dir().join("dist"));
    v
}

fn file_of(target: &str) -> Option<PathBuf> {
    for d in dirs() {
        for name in [format!("hopebackup-{target}"), format!("hopebackup-{target}.gz")] {
            let p = d.join(&name);
            if p.is_file() { return Some(p); }
        }
    }
    None
}

/// Есть ли сборка (своя или в комплекте).
pub fn has(target: &str) -> bool { target == this_target() || file_of(target).is_some() }

/// Какие сборки доступны серверу.
pub fn available() -> Vec<String> { TARGETS.iter().filter(|t| has(t)).map(|t| t.to_string()).collect() }

/// Программа для сборки `target` (распакованная).
pub fn load(target: &str) -> Result<Vec<u8>> {
    if target == this_target() { return std::fs::read(std::env::current_exe()?).context("своя программа"); }
    let Some(p) = file_of(target) else { bail!("нет сборки {target} в комплекте ({})", dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")) };
    let raw = std::fs::read(&p).with_context(|| p.display().to_string())?;
    let data = if p.extension().map(|e| e == "gz").unwrap_or(false) {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).with_context(|| format!("распаковка {}", p.display()))?;
        out
    } else { raw };
    if data.len() < 4 || &data[..4] != b"\x7fELF" { bail!("{} — не программа ELF", p.display()); }
    Ok(data)
}

/// Библиотека C машины.
#[derive(Clone, Debug, PartialEq)]
pub enum Libc {
    /// glibc, второе число версии (2.17 → 17).
    Glibc(u32),
    Musl,
    Unknown,
}

/// Команда sh для определения архитектуры и библиотеки C (вывод разбирает `parse_probe`).
pub const PROBE: &str = "uname -m; echo \"==\"; getconf GNU_LIBC_VERSION 2>/dev/null; (ldd --version 2>&1 | head -2); ls /lib/ld-musl-* /usr/lib/ld-musl-* 2>/dev/null | head -1; true";

/// (архитектура в обозначениях сборок, библиотека C).
pub fn parse_probe(out: &str) -> (String, Libc) {
    let mut parts = out.splitn(2, "==");
    let m = parts.next().unwrap_or("").trim().to_string();
    let rest = parts.next().unwrap_or("").to_lowercase();
    let arch = match m.as_str() { "x86_64" | "amd64" => "x86_64".to_string(), "aarch64" | "arm64" | "armv8l" | "aarch64_be" => if m == "aarch64_be" { m.clone() } else { "aarch64".to_string() }, _ => m.clone() };
    let glibc = rest.lines().find(|l| l.contains("glibc") || l.contains("gnu libc") || l.contains("gnu c library")).and_then(|l| {
        // «glibc 2.28», «ldd (GNU libc) 2.17», «ldd (Ubuntu GLIBC 2.39-0ubuntu8) 2.39».
        l.split(|c: char| !(c.is_ascii_digit() || c == '.')).filter(|w| w.starts_with("2.")).last().and_then(|v| v.split('.').nth(1)).and_then(|x| x.parse::<u32>().ok())
    });
    let libc = if let Some(v) = glibc { Libc::Glibc(v) } else if rest.contains("musl") { Libc::Musl } else { Libc::Unknown };
    (arch, libc)
}

/// Сборки для машины по порядку предпочтения: glibc-сборка, если glibc ≥ 2.17 (быстрее), иначе
/// статическая musl.
pub fn candidates(arch: &str, libc: &Libc) -> Vec<String> {
    let (gnu, musl) = (format!("{arch}-gnu"), format!("{arch}-musl"));
    match libc {
        Libc::Glibc(v) if *v >= 17 => vec![gnu, musl],
        Libc::Glibc(_) | Libc::Musl => vec![musl],
        Libc::Unknown => vec![musl, gnu],
    }
}

/// Подобрать и загрузить программу для машины: (сборка, программа).
pub fn pick(arch: &str, libc: &Libc) -> Result<(String, Vec<u8>)> {
    let c = candidates(arch, libc);
    for t in &c {
        if has(t) { return Ok((t.clone(), load(t)?)); }
    }
    bail!("нет сборки HopeBackup для {arch} ({}): нужна одна из {} — положите её в каталог комплекта {}", label_libc(libc), c.join(", "), dirs().first().map(|d| d.display().to_string()).unwrap_or_default())
}

pub fn label_libc(l: &Libc) -> String {
    match l { Libc::Glibc(v) => format!("glibc 2.{v}"), Libc::Musl => "musl".into(), Libc::Unknown => "библиотека C не определена".into() }
}

/// Подпись сборки для журнала: «aarch64 · glibc», «x86_64 · musl (статическая)».
pub fn label(target: &str) -> String {
    let (a, l) = target.split_once('-').unwrap_or((target, ""));
    format!("{a} · {}", if l == "musl" { "musl (статическая)" } else { "glibc" })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe() {
        let (a, l) = parse_probe("x86_64\n==\nglibc 2.39\nldd (Ubuntu GLIBC 2.39-0ubuntu8.9) 2.39\n");
        assert_eq!((a.as_str(), l.clone()), ("x86_64", Libc::Glibc(39)));
        assert_eq!(candidates(&a, &l), vec!["x86_64-gnu", "x86_64-musl"]);
        let (a, l) = parse_probe("aarch64\n==\nmusl libc (aarch64)\nVersion 1.2.4\n/lib/ld-musl-aarch64.so.1\n");
        assert_eq!((a.as_str(), l.clone()), ("aarch64", Libc::Musl));
        assert_eq!(candidates(&a, &l), vec!["aarch64-musl"]);
        let (_, l) = parse_probe("x86_64\n==\nldd (GNU libc) 2.12\n");
        assert_eq!(l, Libc::Glibc(12));
        assert_eq!(candidates("x86_64", &l), vec!["x86_64-musl"]);
        let (a, l) = parse_probe("arm64\n==\n");
        assert_eq!((a.as_str(), l), ("aarch64", Libc::Unknown));
        assert!(has(&this_target()));
    }
}
