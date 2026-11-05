//! Общий набор для плагинов приложений: команды продукта (от его пользователя или в контейнере),
//! поток вывода утилиты прямо в репозиторий, tar каталогов (из снимка ФС, если он есть), службы
//! (остановить на время копии и обязательно запустить обратно), восстановление в stdin утилиты или
//! в каталог с правильным владельцем, разбор юнитов systemd, проверка HTTP.
//!
//! Плагину остаётся описать продукт: что копировать, какой утилитой, как вернуть.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use super::files::{write_tar_ext, Remap, TarStats};
use super::{cfg_str, LogFn, RestoreCtx};
use crate::exclude::Excludes;
use crate::store::repo::{Repo, SnapshotWriter};
use crate::store::types::{Manifest, StreamEntry};

// ---------------------------------------------------------------- команды

/// Команда продукта: на хосте (при необходимости — от пользователя продукта) или в контейнере Docker.
#[derive(Clone, Debug, Default)]
pub struct Cmd {
    pub prog: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub user: Option<String>,
    pub container: Option<String>,
    pub cwd: Option<String>,
}

impl Cmd {
    pub fn new(prog: impl Into<String>) -> Self { Cmd { prog: prog.into(), ..Default::default() } }
    pub fn arg(mut self, a: impl Into<String>) -> Self { self.args.push(a.into()); self }
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, a: I) -> Self { self.args.extend(a.into_iter().map(Into::into)); self }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self { self.env.push((k.into(), v.into())); self }
    pub fn user(mut self, u: Option<String>) -> Self { self.user = u.filter(|s| !s.is_empty()); self }
    pub fn container(mut self, c: Option<String>) -> Self { self.container = c.filter(|s| !s.is_empty()); self }
    pub fn cwd(mut self, d: impl Into<String>) -> Self { self.cwd = Some(d.into()); self }

    /// Для журнала: программа и аргументы (переменные окружения — с паролями — не показываются).
    pub fn display(&self) -> String {
        let mut s = String::new();
        if let Some(c) = &self.container { s.push_str(&format!("[{c}] ")); }
        if let Some(u) = &self.user { s.push_str(&format!("({u}) ")); }
        s.push_str(&self.prog);
        for a in &self.args { s.push(' '); s.push_str(&crate::util::shq(a)); }
        s
    }

    pub fn std(&self, stdin: bool) -> std::process::Command {
        if let Some(c) = &self.container {
            let mut x = std::process::Command::new("docker");
            x.arg("exec");
            if stdin { x.arg("-i"); }
            if let Some(u) = &self.user { x.arg("-u").arg(u); }
            if let Some(d) = &self.cwd { x.arg("-w").arg(d); }
            // Значения (пароли) — через окружение самого docker, не в его командной строке.
            for (k, v) in &self.env { x.arg("-e").arg(k); x.env(k, v); }
            x.arg(c).arg(&self.prog).args(&self.args);
            return x;
        }
        let mut x = std::process::Command::new(&self.prog);
        x.args(&self.args);
        for (k, v) in &self.env { x.env(k, v); }
        if let Some(d) = &self.cwd { x.current_dir(d); }
        if let Some(u) = self.user.as_deref().filter(|u| *u != "root" && crate::util::is_root()) {
            if let Some((uid, gid)) = crate::util::user_ids(u) {
                let name = std::ffi::CString::new(u).unwrap_or_default();
                let home = home_of(u);
                x.env("HOME", &home).env("USER", u).env("LOGNAME", u);
                if self.cwd.is_none() { x.current_dir(if Path::new(&home).is_dir() { home.as_str() } else { "/" }); }
                unsafe {
                    x.pre_exec(move || {
                        if libc::initgroups(name.as_ptr(), gid) != 0 { return Err(std::io::Error::last_os_error()); }
                        if libc::setgid(gid) != 0 { return Err(std::io::Error::last_os_error()); }
                        if libc::setuid(uid) != 0 { return Err(std::io::Error::last_os_error()); }
                        Ok(())
                    });
                }
            }
        }
        x
    }
}

fn home_of(u: &str) -> String {
    let c = std::ffi::CString::new(u).unwrap_or_default();
    unsafe {
        let pw = libc::getpwnam(c.as_ptr());
        if pw.is_null() || (*pw).pw_dir.is_null() { return "/".into(); }
        std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().to_string()
    }
}

/// Последние строки stderr — в журнал и в текст ошибки.
fn drain_stderr(err: impl Read + Send + 'static, log: LogFn, echo: bool) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        for l in std::io::BufReader::new(err).lines().map_while(|l| l.ok()) {
            if l.trim().is_empty() { continue; }
            if echo { log(&format!("  {l}")); }
            tail.push(l);
            if tail.len() > 12 { tail.remove(0); }
        }
        tail
    })
}

/// Выполнить и вернуть stdout (до 64 МиБ). Ошибка — с концом stderr.
pub async fn output(c: &Cmd, log: &LogFn) -> Result<String> {
    let (c, log) = (c.clone(), log.clone());
    tokio::task::spawn_blocking(move || -> Result<String> {
        let mut x = c.std(false);
        x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
        let et = drain_stderr(ch.stderr.take().unwrap(), log, false);
        let mut out = Vec::new();
        ch.stdout.take().unwrap().take(64 << 20).read_to_end(&mut out)?;
        let st = ch.wait()?;
        let tail = et.join().unwrap_or_default();
        if !st.success() { bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.join(" | ")); }
        Ok(String::from_utf8_lossy(&out).to_string())
    })
    .await?
}

/// Выполнить с выводом в журнал задания.
pub async fn run(c: &Cmd, log: &LogFn) -> Result<()> {
    log(&format!("$ {}", c.display()));
    let (c, log) = (c.clone(), log.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut x = c.std(false);
        x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
        let et = drain_stderr(ch.stderr.take().unwrap(), log.clone(), true);
        for l in std::io::BufReader::new(ch.stdout.take().unwrap()).lines().map_while(|l| l.ok()) {
            if !l.trim().is_empty() { log(&format!("  {l}")); }
        }
        let st = ch.wait()?;
        let tail = et.join().unwrap_or_default();
        if !st.success() { bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.last().cloned().unwrap_or_default()); }
        Ok(())
    })
    .await?
}

/// Есть ли программа (в PATH или по абсолютному пути).
pub fn which(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') { return Path::new(prog).is_file().then(|| PathBuf::from(prog)); }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(prog)).find(|f| f.is_file()))
        .or_else(|| ["/usr/sbin", "/sbin", "/usr/local/sbin", "/usr/local/bin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(prog)).find(|f| f.is_file()))
}

/// Первая найденная программа из списка.
pub fn which_any(progs: &[&str]) -> Option<String> { progs.iter().find(|p| which(p).is_some()).map(|p| p.to_string()) }

// ---------------------------------------------------------------- копия

/// Поток stdout команды — в репозиторий под именем `name`.
pub async fn stream_cmd(w: &mut SnapshotWriter, name: &str, c: &Cmd, log: &LogFn) -> Result<StreamEntry> {
    log(&format!("$ {}", c.display()));
    let mut x = c.std(false);
    x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
    let et = drain_stderr(ch.stderr.take().unwrap(), log.clone(), false);
    let out = ch.stdout.take().unwrap();
    let res = w.add_stream(name, out, None).await;
    let st = tokio::task::spawn_blocking(move || ch.wait()).await??;
    let tail = et.join().unwrap_or_default();
    if !st.success() {
        for l in &tail { log(&format!("  {l}")); }
        bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.last().cloned().unwrap_or_default());
    }
    let e = res?;
    log(&format!("  {name}: {}", crate::util::human_bytes(e.size)));
    Ok(e)
}

/// То же, а первые `head` байт вывода (заголовок дампа: позиция журнала и т. п.) — ещё и в ответ.
pub async fn stream_cmd_head(w: &mut SnapshotWriter, name: &str, c: &Cmd, head: usize, log: &LogFn) -> Result<(StreamEntry, Vec<u8>)> {
    log(&format!("$ {}", c.display()));
    let mut x = c.std(false);
    x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
    let et = drain_stderr(ch.stderr.take().unwrap(), log.clone(), false);
    let buf = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let tap = HeadTap { inner: ch.stdout.take().unwrap(), buf: buf.clone(), limit: head };
    let res = w.add_stream(name, tap, None).await;
    let st = tokio::task::spawn_blocking(move || ch.wait()).await??;
    let tail = et.join().unwrap_or_default();
    if !st.success() {
        for l in &tail { log(&format!("  {l}")); }
        bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.last().cloned().unwrap_or_default());
    }
    let e = res?;
    log(&format!("  {name}: {}", crate::util::human_bytes(e.size)));
    let h = std::mem::take(&mut *buf.lock());
    Ok((e, h))
}

/// Читатель, запоминающий начало потока.
pub struct HeadTap<R> { pub inner: R, pub buf: Arc<parking_lot::Mutex<Vec<u8>>>, pub limit: usize }

impl<R: Read> Read for HeadTap<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        let mut g = self.buf.lock();
        if g.len() < self.limit { let k = n.min(self.limit - g.len()); g.extend_from_slice(&b[..k]); }
        Ok(n)
    }
}

/// Выполнилась ли команда успешно (проверка наличия возможности, без журнала).
pub async fn ok(c: &Cmd) -> bool {
    let c = c.clone();
    tokio::task::spawn_blocking(move || c.std(false).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)).await.unwrap_or(false)
}

/// Первая программа из списка, которая есть на хосте или в контейнере.
pub async fn find_prog(container: Option<&str>, progs: &[&str]) -> Option<String> {
    match container {
        None => which_any(progs),
        Some(ct) => {
            let script = progs.iter().map(|p| format!("command -v {p} >/dev/null 2>&1 && {{ echo {p}; exit 0; }}")).collect::<Vec<_>>().join("; ");
            let c = Cmd::new("sh").args(["-c", &format!("{script}; exit 1")]).container(Some(ct.to_string()));
            let x = c.std(false).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            let s = String::from_utf8_lossy(&x.stdout).trim().to_string();
            (x.status.success() && !s.is_empty()).then_some(s)
        }
    }
}

/// Каталоги и файлы — tar-потоком (из снимка ФС, если он сделан: `remap`).
pub async fn tar_paths(w: &mut SnapshotWriter, name: &str, paths: &[String], ex: Excludes, remap: Remap, log: &LogFn) -> Result<TarStats> {
    tar_paths_x(w, name, paths, ex, remap, false, log).await
}

/// То же с расширенными атрибутами файлов (`xattrs`).
pub async fn tar_paths_x(w: &mut SnapshotWriter, name: &str, paths: &[String], ex: Excludes, remap: Remap, xattrs: bool, log: &LogFn) -> Result<TarStats> {
    let paths: Vec<String> = paths.iter().filter(|p| std::fs::symlink_metadata(remap.phys(p)).is_ok() || Path::new(p).exists()).cloned().collect();
    if paths.is_empty() { bail!("{name}: нет ни одного из путей"); }
    let (r, wp) = std::io::pipe()?;
    let pp = paths.clone();
    let prod = std::thread::spawn(move || write_tar_ext(wp, &pp, &ex, &std::collections::HashSet::new(), true, &remap, xattrs));
    let res = w.add_stream(name, r, None).await;
    let tj = prod.join();
    res?;
    let ts = tj.map_err(|_| anyhow!("поток tar аварийно завершился"))??;
    log(&format!("  {name}: {} ({})", ts.summary(), paths.join(", ")));
    for e in ts.errors.iter().take(20) { log(&format!("  пропущено: {e}")); }
    Ok(ts)
}

/// Небольшие данные (JSON описания, конфигурация, полученная по API) — отдельным потоком.
pub async fn add_bytes(w: &mut SnapshotWriter, name: &str, data: Vec<u8>) -> Result<StreamEntry> {
    w.add_stream(name, std::io::Cursor::new(data), None).await
}

// ---------------------------------------------------------------- восстановление

pub fn stream<'a>(m: &'a Manifest, name: &str) -> Result<&'a StreamEntry> {
    m.streams.iter().find(|s| s.name == name).ok_or_else(|| anyhow!("в копии нет части «{name}»"))
}

/// Потоки копии с именем, начинающимся на `prefix`.
pub fn streams_with<'a>(m: &'a Manifest, prefix: &str) -> Vec<&'a StreamEntry> { m.streams.iter().filter(|s| s.name.starts_with(prefix)).collect() }

/// Поток копии — в stdin команды.
pub async fn feed(repo: &Arc<Repo>, s: &StreamEntry, c: &Cmd, log: &LogFn) -> Result<()> {
    log(&format!("$ {} < {}", c.display(), s.name));
    let mut x = c.std(true);
    x.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
    let et = drain_stderr(ch.stderr.take().unwrap(), log.clone(), false);
    let ot = drain_stderr(ch.stdout.take().unwrap(), log.clone(), false);
    let stdin = ch.stdin.take().unwrap();
    let r = repo.restore_stream(s, stdin).await;
    let st = tokio::task::spawn_blocking(move || ch.wait()).await??;
    let tail = et.join().unwrap_or_default();
    let _ = ot.join();
    if !st.success() {
        for l in &tail { log(&format!("  {l}")); }
        bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.last().cloned().unwrap_or_default());
    }
    r.with_context(|| format!("передача {}", s.name))?;
    Ok(())
}

/// Произвольный поток (например, отфильтрованный дамп) — в stdin команды.
pub async fn feed_read(r: Box<dyn Read + Send>, label: &str, c: &Cmd, log: &LogFn) -> Result<()> {
    log(&format!("$ {} < {label}", c.display()));
    let (c, log) = (c.clone(), log.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut r = r;
        let mut x = c.std(true);
        x.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = x.spawn().with_context(|| format!("запуск: {}", c.display()))?;
        let et = drain_stderr(ch.stderr.take().unwrap(), log.clone(), false);
        let ot = drain_stderr(ch.stdout.take().unwrap(), log.clone(), false);
        let mut stdin = ch.stdin.take().unwrap();
        let cr = std::io::copy(&mut r, &mut stdin);
        drop(stdin);
        // Остаток входа дочитываем, чтобы поставщик (поток из репозитория) не завис на записи.
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        let st = ch.wait()?;
        let tail = et.join().unwrap_or_default();
        let _ = ot.join();
        if !st.success() {
            for l in &tail { log(&format!("  {l}")); }
            bail!("{}: код {} {}", c.display(), st.code().unwrap_or(-1), tail.last().cloned().unwrap_or_default());
        }
        cr.context("передача данных")?;
        Ok(())
    })
    .await?
}

/// Поток копии через фильтр строк — в stdin команды.
pub async fn feed_filtered(repo: &Arc<Repo>, s: &StreamEntry, filter: impl FnOnce(Box<dyn Read + Send>) -> Box<dyn Read + Send>, c: &Cmd, log: &LogFn) -> Result<()> {
    let (reader, fetch) = repo.stream_reader(s)?;
    let r = filter(Box::new(reader));
    let (a, b) = tokio::join!(feed_read(r, &s.name, c, log), fetch);
    a?;
    match b { Ok(Ok(_)) => Ok(()), Ok(Err(e)) => Err(e), Err(e) => Err(e.into()) }
}

/// Рабочий каталог на диске (не в памяти: файлы копий бывают большими), удаляется вместе со значением.
pub struct WorkDir(pub PathBuf);

impl WorkDir {
    /// В `base` (если задан) или в /var/tmp/hopebackup; владелец — `owner`.
    pub fn new(base: Option<&str>, owner: Option<&str>) -> Result<WorkDir> {
        let root = base.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/tmp/hopebackup"));
        std::fs::create_dir_all(&root).with_context(|| format!("создание {}", root.display()))?;
        let p = root.join(format!("hb-{}", crate::util::short_id()));
        std::fs::create_dir(&p)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700))?;
        if let Some((uid, gid)) = owner.filter(|u| *u != "root").and_then(crate::util::user_ids) { let _ = std::os::unix::fs::chown(&p, Some(uid), Some(gid)); }
        Ok(WorkDir(p))
    }
    pub fn path(&self) -> String { self.0.to_string_lossy().to_string() }
    pub fn join(&self, n: &str) -> PathBuf { self.0.join(n) }
}

impl Drop for WorkDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// Снапшоты экземпляра по времени (старые — первыми).
pub async fn snapshots_of(repo: &Arc<Repo>, instance: &str) -> Result<Vec<crate::store::types::SnapshotHeader>> {
    let mut v = repo.backend.list_snapshots(Some(instance)).await?;
    v.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(v)
}

/// Момент восстановления из параметра (RFC 3339 или «ГГГГ-ММ-ДД ЧЧ:ММ[:СС]» — местное время).
pub fn parse_time(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) { return Ok(t.with_timezone(&chrono::Utc)); }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s, f) {
            if let Some(t) = chrono::Local.from_local_datetime(&n).single() { return Ok(t.with_timezone(&chrono::Utc)); }
        }
    }
    bail!("момент восстановления «{s}»: нужен формат ГГГГ-ММ-ДД ЧЧ:ММ:СС")
}

/// Поток копии целиком в память (описания, небольшие файлы).
pub async fn read_small(repo: &Arc<Repo>, s: &StreamEntry) -> Result<Vec<u8>> {
    if s.size > 256 << 20 { bail!("{}: слишком велик для чтения в память", s.name); }
    let buf = Arc::new(parking_lot::Mutex::new(Vec::new()));
    struct W(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for W {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    repo.restore_stream(s, W(buf.clone())).await?;
    let v = std::mem::take(&mut *buf.lock());
    Ok(v)
}

/// Поток копии — в файл.
pub async fn to_file(repo: &Arc<Repo>, s: &StreamEntry, path: &Path) -> Result<u64> {
    if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
    let f = std::fs::File::create(path).with_context(|| format!("создание {}", path.display()))?;
    repo.restore_stream(s, std::io::BufWriter::with_capacity(1 << 20, f)).await
}

/// Как распаковать tar копии.
#[derive(Default, Clone)]
pub struct Untar {
    /// Брать только записи под этими путями (как в архиве, без ведущего «/»); пусто — всё.
    pub only: Vec<String>,
    /// Заменить начало пути: (было, стало) — например, восстановить /var/lib/x в /srv/restore/x.
    pub relocate: Vec<(String, String)>,
    /// Владелец восстановленных файлов (пользователь продукта на целевой машине).
    pub owner: Option<String>,
}

/// Распаковать tar-поток копии в корень `root` («/» — на место).
pub async fn untar(repo: &Arc<Repo>, s: &StreamEntry, root: &Path, opt: Untar, log: &LogFn) -> Result<u64> {
    let (reader, fetch) = repo.stream_reader(s)?;
    let root_disp = root.display().to_string();
    let root = root.to_path_buf();
    let o2 = opt.clone();
    let unpack = tokio::task::spawn_blocking(move || -> Result<(u64, Vec<PathBuf>, u64)> {
        let mut a = tar::Archive::new(reader);
        let (mut n, mut tops, mut xfail) = (0u64, Vec::<PathBuf>::new(), 0u64);
        for e in a.entries()? {
            let mut e = e?;
            let p = e.path()?.to_string_lossy().trim_start_matches("./").trim_start_matches('/').to_string();
            if !o2.only.is_empty() && !o2.only.iter().any(|o| { let o = o.trim_matches('/'); p == o || p.starts_with(&format!("{o}/")) }) { continue; }
            let mut rel = p.clone();
            for (from, to) in &o2.relocate {
                let f = from.trim_matches('/');
                if rel == f || rel.starts_with(&format!("{f}/")) { rel = format!("{}{}", to.trim_matches('/'), &rel[f.len()..]); break; }
            }
            if rel.split('/').any(|c| c == "..") { continue; }
            let dst = root.join(&rel);
            if let Some(par) = dst.parent() { std::fs::create_dir_all(par)?; }
            e.set_preserve_permissions(true);
            e.set_preserve_mtime(true);
            // Расширенные атрибуты — сами: ФС цели может не поддерживать часть из них, это не повод
            // прерывать восстановление.
            e.set_unpack_xattrs(false);
            let xs: Vec<(Vec<u8>, Vec<u8>)> = e.pax_extensions().ok().flatten().map(|x| x.filter_map(|r| r.ok()).filter_map(|r| r.key_bytes().strip_prefix(b"SCHILY.xattr.").map(|k| (k.to_vec(), r.value_bytes().to_vec()))).collect()).unwrap_or_default();
            e.unpack(&dst).with_context(|| format!("распаковка {}", dst.display()))?;
            for (k, v) in &xs { if !set_xattr(&dst, k, v) { xfail += 1; } }
            if crate::util::is_root() && o2.owner.is_none() {
                if let (Ok(u), Ok(g)) = (e.header().uid(), e.header().gid()) { let _ = std::os::unix::fs::lchown(&dst, Some(u as u32), Some(g as u32)); }
            }
            if o2.owner.is_some() && !tops.iter().any(|t| dst.starts_with(t)) {
                let top = root.join(rel.split('/').next().unwrap_or(""));
                tops.push(if top == root { dst.clone() } else { top });
            }
            n += 1;
        }
        let mut rest = a.into_inner();
        let _ = std::io::copy(&mut rest, &mut std::io::sink());
        Ok((n, tops, xfail))
    });
    let (u, f) = tokio::join!(unpack, fetch);
    let (n, _tops, xfail) = u??;
    f??;
    if xfail > 0 { log(&format!("  {}: расширенных атрибутов не установлено {xfail} (ФС цели их не поддерживает?)", s.name)); }
    log(&format!("  {}: восстановлено объектов {n} в {root_disp}", s.name));
    Ok(n)
}

fn set_xattr(p: &Path, k: &[u8], v: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let (Ok(cp), Ok(ck)) = (std::ffi::CString::new(p.as_os_str().as_bytes()), std::ffi::CString::new(k)) else { return false };
    unsafe { libc::lsetxattr(cp.as_ptr(), ck.as_ptr(), v.as_ptr() as *const libc::c_void, v.len(), 0) == 0 }
}

/// Сменить владельца каталогов на пользователя продукта (после распаковки на другой машине uid могут
/// отличаться).
pub fn chown_to(paths: &[String], user: &str, log: &LogFn) -> Result<()> {
    if !crate::util::is_root() { return Ok(()); }
    let (uid, gid) = crate::util::user_ids(user).ok_or_else(|| anyhow!("нет пользователя ОС «{user}» — установите продукт на цели"))?;
    for p in paths.iter().filter(|p| Path::new(p).exists()) { crate::util::chown_r(Path::new(p), uid, gid)?; }
    log(&format!("  владелец {user}: {}", paths.join(", ")));
    Ok(())
}

/// Свободное имя «<путь>.before-restore-<время>[-N]» для прежнего содержимого.
pub fn aside_name(path: &str, ts: &str) -> String {
    let mut a = format!("{path}.before-restore-{ts}");
    let mut n = 1;
    while std::fs::symlink_metadata(&a).is_ok() { n += 1; a = format!("{path}.before-restore-{ts}-{n}"); }
    a
}

/// Безопасный путь из копии: абсолютный, без «..».
pub fn safe_abs(p: &str) -> Result<String> {
    if !p.starts_with('/') || p.split('/').any(|c| c == "..") { bail!("недопустимый путь «{p}»"); }
    Ok(p.trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------- службы

/// Службы ОС: systemd или OpenRC.
pub mod svc {
    use super::*;

    fn systemd() -> bool { which("systemctl").is_some() && Path::new("/run/systemd/system").is_dir() }

    pub fn exists(name: &str) -> bool {
        if systemd() {
            return ["/etc/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system", "/run/systemd/system"].iter().any(|d| Path::new(d).join(format!("{name}.service")).exists())
                || std::process::Command::new("systemctl").args(["cat", name]).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        }
        Path::new("/etc/init.d").join(name).exists()
    }

    pub fn active(name: &str) -> bool {
        if systemd() { return std::process::Command::new("systemctl").args(["is-active", "--quiet", name]).status().map(|s| s.success()).unwrap_or(false); }
        if which("rc-service").is_some() { return std::process::Command::new("rc-service").args([name, "status"]).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false); }
        false
    }

    async fn ctl(name: &str, verb: &str, log: &LogFn) -> Result<()> {
        let c = if systemd() { Cmd::new("systemctl").args([verb, name]) } else if which("rc-service").is_some() { Cmd::new("rc-service").args([name, verb]) } else { bail!("нет systemd и OpenRC — службой «{name}» управлять нечем") };
        super::run(&c, log).await
    }

    pub async fn stop(name: &str, log: &LogFn) -> Result<()> { ctl(name, "stop", log).await }

    pub async fn start(name: &str, log: &LogFn) -> Result<()> {
        ctl(name, "start", log).await?;
        for _ in 0..30 {
            if active(name) { return Ok(()); }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("служба {name} не запустилась")
    }

    /// Выполнить `f` при остановленных службах; службы, которые работали, запускаются обратно в любом
    /// случае (и при ошибке `f`).
    pub async fn while_stopped<T, F, Fut>(names: &[String], log: &LogFn, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut stopped: Vec<String> = Vec::new();
        let t0 = std::time::Instant::now();
        for n in names {
            if active(n) {
                if let Err(e) = stop(n, log).await {
                    for s in stopped.iter().rev() { let _ = start(s, log).await; }
                    return Err(e);
                }
                stopped.push(n.clone());
            }
        }
        let r = f().await;
        let mut start_err = None;
        for s in stopped.iter().rev() {
            if let Err(e) = start(s, log).await { start_err = Some(e); }
        }
        if !stopped.is_empty() { log(&format!("службы {} были недоступны {:.1} с", stopped.join(", "), t0.elapsed().as_secs_f64())); }
        let v = r?;
        if let Some(e) = start_err { return Err(e.context("копия снята, но служба не запустилась обратно")); }
        Ok(v)
    }
}

// ---------------------------------------------------------------- Docker

/// Путь на хосте, смонтированный в контейнер в `dest` (том или каталог хоста).
pub fn docker_mount(container: &str, dest: &str) -> Option<String> {
    let o = std::process::Command::new("docker").args(["inspect", "-f", "{{json .Mounts}}", container]).output().ok()?;
    if !o.status.success() { return None; }
    let v: Value = serde_json::from_slice(&o.stdout).ok()?;
    v.as_array()?.iter().find(|m| m["Destination"].as_str().map(|d| d.trim_end_matches('/') == dest.trim_end_matches('/')).unwrap_or(false))
        .and_then(|m| m["Source"].as_str().map(|s| s.to_string()))
}

/// Переменная окружения контейнера.
pub fn docker_env(container: &str, k: &str) -> Option<String> {
    let o = std::process::Command::new("docker").args(["inspect", "-f", "{{json .Config.Env}}", container]).output().ok()?;
    let v: Vec<String> = serde_json::from_slice(&o.stdout).ok()?;
    v.into_iter().find_map(|e| e.strip_prefix(&format!("{k}=")).map(|x| x.to_string()))
}

// ---------------------------------------------------------------- юниты systemd

/// Что известно о службе из её юнита (без работающего systemd: читаем файлы).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Unit {
    pub name: String,
    pub path: String,
    pub exec: String,
    pub user: String,
    pub working_dir: String,
    pub env: HashMap<String, String>,
    pub creds: Vec<(String, String)>,
}

impl Unit {
    pub fn var(&self, k: &str) -> Option<String> { self.env.get(k).cloned().filter(|v| !v.is_empty()) }
    /// Путь относительно WorkingDirectory службы.
    pub fn resolve(&self, p: &str) -> String {
        if p.starts_with('/') || self.working_dir.is_empty() { return p.to_string(); }
        let base = PathBuf::from(&self.working_dir);
        let mut out = base.clone();
        for c in Path::new(p).components() {
            match c { std::path::Component::CurDir => {} std::path::Component::ParentDir => { out.pop(); } other => out.push(other) }
        }
        out.to_string_lossy().to_string()
    }
}

/// Прочитать юнит и его дополнения (`<имя>.service.d/*.conf`), файлы окружения.
pub fn unit(name: &str) -> Option<Unit> {
    let dirs = ["/etc/systemd/system", "/run/systemd/system", "/usr/local/lib/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system"];
    let file = dirs.iter().map(|d| Path::new(d).join(format!("{name}.service"))).find(|p| p.exists())?;
    let mut u = Unit { name: name.into(), path: file.to_string_lossy().to_string(), ..Default::default() };
    let mut texts = vec![std::fs::read_to_string(&file).ok()?];
    for d in dirs {
        let dd = Path::new(d).join(format!("{name}.service.d"));
        if let Ok(rd) = std::fs::read_dir(&dd) {
            let mut fs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "conf").unwrap_or(false)).collect();
            fs.sort();
            for f in fs { if let Ok(t) = std::fs::read_to_string(&f) { texts.push(t); } }
        }
    }
    for t in texts {
        for l in t.lines() {
            let l = l.trim();
            let Some((k, v)) = l.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "ExecStart" if !v.is_empty() => u.exec = v.trim_start_matches(['-', '@', '+', '!', ':']).to_string(),
                "User" => u.user = v.to_string(),
                "WorkingDirectory" => u.working_dir = v.trim_start_matches('-').to_string(),
                "Environment" => for (k2, v2) in parse_env_line(v) { u.env.insert(k2, v2); },
                "EnvironmentFile" => {
                    let f = v.trim_start_matches('-');
                    if let Ok(t) = std::fs::read_to_string(f) {
                        for l in t.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
                            if let Some((a, b)) = l.trim_start_matches("export ").split_once('=') { u.env.insert(a.trim().into(), b.trim().trim_matches('"').trim_matches('\'').into()); }
                        }
                    }
                }
                "LoadCredential" => if let Some((a, b)) = v.split_once(':') { u.creds.push((a.into(), b.into())); },
                _ => {}
            }
        }
    }
    Some(u)
}

/// `Environment=A=1 "B=2 3"` → [(A,1), (B,"2 3")].
fn parse_env_line(v: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    for ch in v.chars().chain(std::iter::once(' ')) {
        match ch {
            '"' => q = !q,
            ' ' if !q => {
                if let Some((a, b)) = cur.split_once('=') { out.push((a.to_string(), b.to_string())); }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- снимок ФС или остановка

/// Как получить согласованную копию файлов работающего продукта.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consistency {
    /// Снимок ФС (LVM, ZFS, btrfs); если невозможен — короткая остановка служб.
    Auto,
    Snapshot,
    Stop,
    /// «На ходу» (продукт сам пишет атомарно или копия нужна без простоя).
    Live,
}

pub fn consistency(cfg: &Value) -> Consistency {
    match cfg_str(cfg, "consistency").as_deref() {
        Some("snapshot") => Consistency::Snapshot,
        Some("stop") => Consistency::Stop,
        Some("live") => Consistency::Live,
        _ => Consistency::Auto,
    }
}

pub fn consistency_schema(default_desc: &str) -> Value {
    json!({ "type": "string", "title": "Согласованность копии", "enum": ["auto", "snapshot", "stop", "live"], "default": "auto",
        "description": format!("auto — снимок ФС (LVM/ZFS/btrfs), если невозможен — короткая остановка службы; snapshot — только снимок; stop — остановить службу на время копии; live — на ходу. {default_desc}") })
}

/// Скопировать `paths` tar-потоком `name` согласованно: из снимка ФС или при остановленных службах.
#[allow(clippy::too_many_arguments)]
pub async fn consistent_tar(w: &mut SnapshotWriter, name: &str, paths: &[String], ex: Excludes, services: &[String], cfg: &Value, tag: &str, log: &LogFn) -> Result<(TarStats, &'static str)> {
    consistent_tar_x(w, name, paths, ex, services, cfg, tag, false, log).await
}

#[allow(clippy::too_many_arguments)]
pub async fn consistent_tar_x(w: &mut SnapshotWriter, name: &str, paths: &[String], ex: Excludes, services: &[String], cfg: &Value, tag: &str, xattrs: bool, log: &LogFn) -> Result<(TarStats, &'static str)> {
    let mode = consistency(cfg);
    let existing: Vec<String> = paths.iter().filter(|p| Path::new(p).exists()).cloned().collect();
    if existing.is_empty() { bail!("{name}: нет ни одного из путей {}", paths.join(", ")); }
    if matches!(mode, Consistency::Auto | Consistency::Snapshot) {
        let mut c2 = cfg.clone();
        if let Some(o) = c2.as_object_mut() { o.insert("snapshot".into(), json!("required")); }
        match super::files::snapshot_for(&existing, &c2, tag, log.clone()).await {
            Ok(snaps) => {
                let ts = tar_paths_x(w, name, &existing, ex, snaps.remap.clone(), xattrs, log).await?;
                snaps.check()?;
                drop(snaps);
                return Ok((ts, "снимок ФС"));
            }
            Err(e) if mode == Consistency::Snapshot => return Err(e),
            Err(e) => log(&format!("снимок ФС невозможен ({e:#}) — служба будет остановлена на время копии")),
        }
    }
    if matches!(mode, Consistency::Auto | Consistency::Stop) && !services.is_empty() {
        let ts = svc::while_stopped(services, log, || tar_paths_x(w, name, &existing, ex, Remap::default(), xattrs, log)).await?;
        return Ok((ts, "служба остановлена"));
    }
    Ok((tar_paths_x(w, name, &existing, ex, Remap::default(), xattrs, log).await?, "на ходу"))
}

// ---------------------------------------------------------------- секреты во временных файлах

/// Временный файл 0600 (параметры утилиты с паролем), удаляется вместе со значением.
pub struct SecretFile(pub PathBuf);

impl SecretFile {
    pub fn new(content: &str, owner: Option<&str>) -> Result<SecretFile> { Self::with_ext(content, owner, "") }
    pub fn with_ext(content: &str, owner: Option<&str>, ext: &str) -> Result<SecretFile> {
        let dir = Path::new("/run/hopebackup/tmp");
        let dir = if std::fs::create_dir_all(dir).is_ok() { dir.to_path_buf() } else { std::env::temp_dir() };
        let p = dir.join(format!("hb-{}{ext}", crate::util::short_id()));
        crate::util::write_secret(&p, content.as_bytes())?;
        if let Some((uid, gid)) = owner.filter(|u| *u != "root").and_then(crate::util::user_ids) {
            let _ = std::os::unix::fs::chown(&p, Some(uid), Some(gid));
            let _ = std::os::unix::fs::chown(&dir, Some(0), Some(0));
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711));
        }
        Ok(SecretFile(p))
    }
    pub fn path(&self) -> String { self.0.to_string_lossy().to_string() }
}

impl Drop for SecretFile {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

/// Временный файл с секретом внутри контейнера (параметры утилиты), удаляется вместе со значением.
pub struct ContainerSecret { pub container: String, pub path: String }

impl ContainerSecret {
    pub async fn new(container: &str, content: &str, log: &LogFn) -> Result<ContainerSecret> { Self::with_ext(container, content, "", log).await }
    pub async fn with_ext(container: &str, content: &str, ext: &str, log: &LogFn) -> Result<ContainerSecret> {
        let path = format!("/tmp/.hb-{}{ext}", crate::util::short_id());
        let c = Cmd::new("sh").args(["-c", &format!("umask 077; cat > {path}")]).container(Some(container.to_string()));
        feed_read(Box::new(std::io::Cursor::new(content.as_bytes().to_vec())), "параметры", &c, log).await?;
        Ok(ContainerSecret { container: container.into(), path })
    }
}

impl Drop for ContainerSecret {
    fn drop(&mut self) { let _ = std::process::Command::new("docker").args(["exec", &self.container, "rm", "-f", &self.path]).stdout(Stdio::null()).stderr(Stdio::null()).status(); }
}

/// Файл параметров с секретом — на хосте или в контейнере (путь — как его видит утилита).
pub enum Secret { Host(SecretFile), Container(ContainerSecret) }

impl Secret {
    pub async fn new(container: Option<&str>, content: &str, log: &LogFn) -> Result<Secret> { Self::with_ext(container, content, "", log).await }
    /// С расширением имени (утилиты, различающие файлы по нему: «.js», «.xml»).
    pub async fn with_ext(container: Option<&str>, content: &str, ext: &str, log: &LogFn) -> Result<Secret> {
        Ok(match container { Some(c) => Secret::Container(ContainerSecret::with_ext(c, content, ext, log).await?), None => Secret::Host(SecretFile::with_ext(content, None, ext)?) })
    }
    pub fn path(&self) -> String { match self { Secret::Host(f) => f.path(), Secret::Container(c) => c.path.clone() } }
}

// ---------------------------------------------------------------- HTTP

/// Клиент HTTP(S) к продукту: самоподписанные сертификаты локальных служб допускаются, если задан отпечаток
/// или явно разрешено (`insecure`).
pub fn http(fingerprint: &str, insecure: bool, timeout_s: u64) -> Result<reqwest::Client> {
    if !crate::tlsutil::normalize_fp(fingerprint).is_empty() { return crate::tlsutil::client(Some(fingerprint)); }
    Ok(reqwest::Client::builder().danger_accept_invalid_certs(insecure).timeout(Duration::from_secs(timeout_s)).build()?)
}

/// Дождаться ответа службы по HTTP (любой код ответа — служба жива).
pub async fn wait_http(url: &str, secs: u64) -> bool {
    let Ok(c) = http("", true, 5) else { return false };
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs() < secs {
        if c.get(url).send().await.is_ok() { return true; }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    false
}

/// Параметр восстановления, по умолчанию — из экземпляра (мастер передаёт их вместе).
pub fn restore_param(ctx: &RestoreCtx, k: &str) -> Option<String> { cfg_str(&ctx.params, k) }

pub fn bool_param(v: &Value, k: &str, def: bool) -> bool { v.get(k).and_then(|x| x.as_bool()).unwrap_or(def) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_and_paths() {
        let e = parse_env_line(r#"A=1 "B=2 3" C="#);
        assert_eq!(e, vec![("A".into(), "1".into()), ("B".into(), "2 3".into()), ("C".into(), "".into())]);
        let u = Unit { working_dir: "/var/lib/hope-gateway".into(), ..Default::default() };
        assert_eq!(u.resolve("./msg_local_data"), "/var/lib/hope-gateway/msg_local_data");
        assert_eq!(u.resolve("/abs"), "/abs");
        assert!(safe_abs("/var/../etc").is_err() && safe_abs("rel").is_err());
    }
}
