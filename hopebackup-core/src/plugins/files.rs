//! Плагин файлов и каталогов. Пресеты: nginx, HAProxy, OpenVPN — пути, проверка конфигурации, перезагрузка.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::{BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::exclude::Excludes;

pub struct Files;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": { "type": "array", "items": { "type": "string" }, "title": "Пути", "description": "Каталоги и файлы для копирования" },
            "exclude": { "type": "array", "items": { "type": "string" }, "title": "Исключения", "description": "/var/log — путь целиком; *.log, node_modules — по имени на любой глубине" },
            "use_global_exclude": { "type": "boolean", "title": "Общие исключения сервера", "default": true, "description": "Добавить список из «Настройки → Исключения»" },
            "max_file_mb": { "type": "integer", "title": "Не копировать файлы больше, МБ", "description": "Пусто — без ограничения" },
            "snapshot": { "type": "string", "title": "Снимок файловой системы", "enum": ["auto", "required", "off"], "default": "auto", "description": "auto — читать из снимка LVM/ZFS/btrfs, где возможно (все файлы в одном состоянии), иначе «на ходу»; required — без снимка не копировать; off — без снимков" },
            "snapshot_size_pct": { "type": "integer", "title": "Размер снимка LVM, % тома", "default": 10, "description": "Место под изменения на время копии; не меньше 1 ГиБ. Свободное место нужно в группе томов (анализ машины подскажет, сколько)" },
            "pre_snapshot": { "type": "string", "title": "Команда перед снимком", "description": "Например, сбросить буферы приложения на диск" },
            "post_snapshot": { "type": "string", "title": "Команда после снимка", "description": "Выполняется сразу после создания снимков (даже при ошибке)" },
            "verify_cmd": { "type": "string", "title": "Проверка после восстановления", "description": "Например: nginx -t" },
            "reload_cmd": { "type": "string", "title": "Применить после восстановления", "description": "Например: systemctl reload nginx" }
        }
    })
}

pub fn preset(id: &str) -> Value {
    match id {
        "nginx" => json!({ "preset": "nginx", "paths": ["/etc/nginx"], "exclude": [], "verify_cmd": "nginx -t", "reload_cmd": "systemctl reload nginx" }),
        "haproxy" => json!({ "preset": "haproxy", "paths": ["/etc/haproxy", "/etc/keepalived"], "exclude": [], "verify_cmd": "haproxy -c -f /etc/haproxy/haproxy.cfg", "reload_cmd": "systemctl reload haproxy" }),
        "openvpn" => json!({ "preset": "openvpn", "paths": ["/etc/openvpn"], "exclude": [], "verify_cmd": "", "reload_cmd": "systemctl restart openvpn-server@server" }),
        _ => json!({ "preset": "custom", "paths": [] }),
    }
}

#[derive(Deserialize)]
struct Conf {
    #[serde(default)]
    paths: Vec<String>,
}

/// Итог записи tar: сколько файлов, сколько пропущено (исчезли, нет доступа, по размеру).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TarStats {
    pub files: u64,
    pub bytes: u64,
    pub excluded: u64,
    pub too_big: u64,
    pub skipped: u64,
    /// Первые ошибки чтения (остальные только считаются).
    pub errors: Vec<String>,
}

impl TarStats {
    pub fn summary(&self) -> String {
        let mut s = format!("файлов {}, {}", self.files, crate::util::human_bytes(self.bytes));
        if self.excluded > 0 { s += &format!(", исключено {}", self.excluded); }
        if self.too_big > 0 { s += &format!(", пропущено больших файлов {}", self.too_big); }
        if self.skipped > 0 { s += &format!(", не прочитано {}", self.skipped); }
        s
    }
}

/// Убрать пути, вложенные в другие из списка (иначе файлы попадут в архив дважды).
pub fn top_level(paths: &[String]) -> Vec<String> {
    let mut v: Vec<String> = paths.iter().map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| !p.is_empty()).collect();
    v.sort();
    v.dedup();
    let mut out: Vec<String> = Vec::new();
    for p in v {
        if out.iter().any(|o| p.starts_with(&format!("{o}/"))) { continue; }
        out.push(p);
    }
    out
}

/// Откуда читать путь: логический путь (как он попадёт в копию) → физический (например, внутри снимка ФС).
/// Выбирается самая длинная подходящая точка; пустая таблица — читать как есть.
#[derive(Clone, Debug, Default)]
pub struct Remap(Vec<(String, PathBuf)>);

impl Remap {
    pub fn add(&mut self, from: &str, to: PathBuf) {
        let from = if from == "/" { "/".to_string() } else { from.trim_end_matches('/').to_string() };
        self.0.retain(|(f, _)| *f != from);
        self.0.push((from, to));
        self.0.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    pub fn phys(&self, logical: &str) -> PathBuf {
        for (from, to) in &self.0 {
            if from == "/" { return to.join(logical.trim_start_matches('/')); }
            if logical == from { return to.clone(); }
            if let Some(rest) = logical.strip_prefix(&format!("{from}/")) { return to.join(rest); }
        }
        PathBuf::from(logical)
    }
}

/// Записать пути в tar (симлинки — как симлинки, без перехода по ним).
pub fn write_tar<W: std::io::Write>(w: W, paths: &[String], ex: &Excludes) -> Result<TarStats> {
    write_tar_opts(w, paths, ex, &std::collections::HashSet::new(), true)
}

/// `skip_dirs` — каталоги, в которые не заходить (точки монтирования других ФС); `strict` — отсутствие
/// пути из списка — ошибка (иначе — пропуск).
pub fn write_tar_opts<W: std::io::Write>(w: W, paths: &[String], ex: &Excludes, skip_dirs: &std::collections::HashSet<String>, strict: bool) -> Result<TarStats> {
    write_tar_remap(w, paths, ex, skip_dirs, strict, &Remap::default())
}

/// То же, но файлы читаются через `remap` (из снимков ФС); имена в архиве — исходные пути.
pub fn write_tar_remap<W: std::io::Write>(w: W, paths: &[String], ex: &Excludes, skip_dirs: &std::collections::HashSet<String>, strict: bool, remap: &Remap) -> Result<TarStats> {
    write_tar_ext(w, paths, ex, skip_dirs, strict, remap, false)
}

/// Расширенные атрибуты файла (ACL, метки, `user.*`) — `llistxattr`/`lgetxattr`.
pub fn read_xattrs(p: &Path) -> Vec<(String, Vec<u8>)> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return vec![] };
    let mut out = Vec::new();
    unsafe {
        let n = libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0);
        if n <= 0 { return out; }
        let mut names = vec![0u8; n as usize];
        let n = libc::llistxattr(c.as_ptr(), names.as_mut_ptr() as *mut libc::c_char, names.len());
        if n <= 0 { return out; }
        for name in names[..n as usize].split(|b| *b == 0).filter(|x| !x.is_empty()) {
            let Ok(cn) = std::ffi::CString::new(name) else { continue };
            let sz = libc::lgetxattr(c.as_ptr(), cn.as_ptr(), std::ptr::null_mut(), 0);
            if sz < 0 { continue; }
            let mut v = vec![0u8; sz as usize];
            let sz = libc::lgetxattr(c.as_ptr(), cn.as_ptr(), v.as_mut_ptr() as *mut libc::c_void, v.len());
            if sz < 0 { continue; }
            v.truncate(sz as usize);
            out.push((String::from_utf8_lossy(name).to_string(), v));
        }
    }
    out
}

/// Записи PAX `SCHILY.xattr.*` перед элементом архива (их понимают GNU tar, bsdtar и крейт tar).
fn append_xattrs<W: std::io::Write>(b: &mut tar::Builder<W>, name: &str, xs: &[(String, Vec<u8>)]) -> Result<()> {
    if xs.is_empty() { return Ok(()); }
    let mut body = Vec::new();
    for (k, v) in xs {
        let key = format!("SCHILY.xattr.{k}");
        // Длина записи включает собственные цифры: «<len> <key>=<value>\n».
        let base = 1 + key.len() + 1 + v.len() + 1;
        let mut len = base + base.to_string().len();
        if len.to_string().len() != base.to_string().len() { len = base + len.to_string().len(); }
        body.extend_from_slice(format!("{len} {key}=").as_bytes());
        body.extend_from_slice(v);
        body.push(b'\n');
    }
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::XHeader);
    h.set_mode(0o644);
    h.set_size(body.len() as u64);
    let short: String = name.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
    h.set_path(format!("PaxHeaders/{}", short.trim_start_matches('/'))).or_else(|_| h.set_path("PaxHeaders/x"))?;
    h.set_cksum();
    b.append(&h, body.as_slice())?;
    Ok(())
}

/// tar путей; `xattrs` — с расширенными атрибутами (NTFS-права HopeDFS, метки Astra, ACL POSIX).
pub fn write_tar_ext<W: std::io::Write>(w: W, paths: &[String], ex: &Excludes, skip_dirs: &std::collections::HashSet<String>, strict: bool, remap: &Remap, xattrs: bool) -> Result<TarStats> {
    let mut b = tar::Builder::new(w);
    b.follow_symlinks(false);
    let mut st = TarStats::default();
    fn note(st: &mut TarStats, msg: String) {
        st.skipped += 1;
        if st.errors.len() < 20 { st.errors.push(msg); }
    }
    #[allow(clippy::too_many_arguments)]
    fn walk<W: std::io::Write>(b: &mut tar::Builder<W>, p: &Path, ex: &Excludes, skip: &std::collections::HashSet<String>, st: &mut TarStats, root: &str, rm: &Remap, xa: bool) -> Result<()> {
        let s = p.to_string_lossy().to_string();
        let top = s == root;
        if ex.excluded_under(&s, root) { st.excluded += 1; return Ok(()); }
        // Каталог снимков btrfs — служебный.
        if p.file_name().map(|n| n == crate::fssnap::BTRFS_DIR).unwrap_or(false) { return Ok(()); }
        let phys = rm.phys(&s);
        let md = match std::fs::symlink_metadata(&phys) {
            Ok(m) => m,
            Err(e) => { note(st, format!("{s}: {e}")); return Ok(()); }
        };
        let name = s.trim_start_matches('/');
        if name.is_empty() { bail!("копировать корень / целиком нельзя — перечислите каталоги"); }
        if xa { append_xattrs(b, name, &read_xattrs(&phys))?; }
        if md.is_dir() {
            // Заголовок — из уже прочитанных метаданных: каталог мог исчезнуть после stat.
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&md);
            h.set_size(0);
            b.append_data(&mut h, format!("{name}/"), std::io::empty())?;
            if !top && skip.contains(&s) { return Ok(()); }
            let rd = match std::fs::read_dir(&phys) {
                Ok(r) => r,
                Err(e) => { note(st, format!("{s}: {e}")); return Ok(()); }
            };
            let mut ents: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| p.join(e.file_name()))).collect();
            ents.sort();
            for e in ents { walk(b, &e, ex, skip, st, root, rm, xa)?; }
        } else if md.is_file() {
            if ex.too_big(md.len()) { st.too_big += 1; return Ok(()); }
            let f = match std::fs::File::open(&phys) {
                Ok(f) => f,
                Err(e) => { note(st, format!("{s}: {e}")); return Ok(()); }
            };
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&md);
            // Файл мог измениться между stat и чтением: пишем ровно заявленный размер.
            let mut r = std::io::Read::take(f, md.len());
            let mut buf = Vec::new();
            if md.len() <= 8 * 1024 * 1024 {
                if let Err(e) = std::io::Read::read_to_end(&mut r, &mut buf) { note(st, format!("{s}: {e}")); return Ok(()); }
                if (buf.len() as u64) < md.len() { note(st, format!("{s}: файл укоротился во время чтения (дополнен нулями)")); }
                buf.resize(md.len() as usize, 0);
                b.append_data(&mut h, name, buf.as_slice())?;
            } else {
                let mut pad = PadTo { r, left: md.len(), err: None };
                b.append_data(&mut h, name, &mut pad)?;
                if let Some(e) = pad.err { note(st, format!("{s}: {e} — файл в копии неполный (дополнен нулями)")); }
            }
            st.files += 1;
            st.bytes += md.len();
        } else if md.file_type().is_symlink() {
            let target = match std::fs::read_link(&phys) {
                Ok(t) => t,
                Err(e) => { note(st, format!("{s}: {e}")); return Ok(()); }
            };
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&md);
            h.set_size(0);
            b.append_link(&mut h, name, &target)?;
            st.files += 1;
        }
        Ok(())
    }
    for p in top_level(paths) {
        // Псевдо-файловые системы и корень целиком не копируются даже явно (/proc/kcore — 128 ТБ нулей).
        if p == "/" || ["/proc", "/sys", "/dev", "/run"].iter().any(|d| p == *d || p.starts_with(&format!("{d}/"))) {
            if strict { bail!("путь {p} нельзя копировать — это служебная файловая система или корень"); }
            note(&mut st, format!("{p}: служебная файловая система — пропущено"));
            continue;
        }
        let pp = Path::new(&p);
        if std::fs::symlink_metadata(remap.phys(&p)).is_err() {
            if strict { bail!("нет пути {p}"); }
            continue; // необязательный путь (нет на этом сервере)
        }
        walk(&mut b, pp, ex, skip_dirs, &mut st, &p, remap, xattrs)?;
    }
    b.finish()?;
    Ok(st)
}

/// Чтение ровно `left` байт: если файл укоротился во время чтения — добивается нулями.
struct PadTo<R> {
    r: R,
    left: u64,
    /// Первая ошибка чтения (после неё — нули, файл помечается неполным).
    err: Option<String>,
}

impl<R: std::io::Read> std::io::Read for PadTo<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 { return Ok(0); }
        let want = buf.len().min(self.left as usize);
        let n = if self.err.is_some() {
            buf[..want].fill(0);
            want
        } else {
            loop {
                match self.r.read(&mut buf[..want]) {
                    Ok(0) => { self.err = Some("файл укоротился во время чтения".into()); buf[..want].fill(0); break want; }
                    Ok(n) => break n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => { self.err = Some(e.to_string()); buf[..want].fill(0); break want; }
                }
            }
        };
        self.left -= n as u64;
        Ok(n)
    }
}

async fn sh(cmd: &str, log: &LogFn) -> Result<()> {
    log(&format!("$ {cmd}"));
    let out = tokio::process::Command::new("sh").arg("-c").arg(cmd).output().await?;
    for l in String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out.stderr).lines()) {
        if !l.trim().is_empty() { log(&format!("  {l}")); }
    }
    if !out.status.success() { bail!("команда «{cmd}» завершилась с кодом {}", out.status.code().unwrap_or(-1)); }
    Ok(())
}

/// Снимки ФС под пути (в отдельном потоке: команды lvcreate/zfs/btrfs синхронные).
pub async fn snapshot_for(paths: &[String], cfg: &Value, tag: &str, log: LogFn) -> Result<crate::fssnap::SnapSet> {
    let o = crate::fssnap::Opts::from_config(cfg);
    let (paths, tag) = (paths.to_vec(), tag.to_string());
    tokio::task::spawn_blocking(move || crate::fssnap::SnapSet::prepare(&paths, &o, &tag, log)).await.map_err(|e| anyhow::anyhow!("снимок: {e}"))?
}

#[async_trait]
impl Plugin for Files {
    fn id(&self) -> &'static str { "files" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let c: Conf = serde_json::from_value(cfg.clone())?;
        let found: Vec<Value> = c.paths.iter().map(|p| json!({ "path": p, "exists": Path::new(p).exists() })).collect();
        Ok(json!({ "paths": found }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let c: Conf = serde_json::from_value(ctx.config.clone())?;
        if c.paths.is_empty() { bail!("не указаны пути"); }
        ctx.log.as_ref()(&format!("файлы: {}", c.paths.join(", ")));
        let ex = Excludes::from_config(&ctx.config);
        if !ex.list().is_empty() { ctx.log.as_ref()(&format!("исключения: {}", ex.list().join(", "))); }
        // Снимок ФС: все файлы копии — в одном состоянии.
        let snaps = snapshot_for(&c.paths, &ctx.config, &ctx.job_tag(), ctx.log.clone()).await?;
        let (r, w) = std::io::pipe()?;
        let paths = c.paths.clone();
        let remap = snaps.remap.clone();
        let producer = std::thread::spawn(move || write_tar_remap(w, &paths, &ex, &std::collections::HashSet::new(), true, &remap));
        let mut wr = ctx.repo.writer();
        let res = wr.add_stream("files.tar", r, None).await;
        let tj = producer.join();
        // Сначала ошибка записи на площадку: «обрыв канала» у чтения файлов — лишь её следствие.
        res?;
        let ts = tj.map_err(|_| anyhow::anyhow!("поток tar аварийно завершился"))??;
        snaps.check()?;
        let snapshot_note = if snaps.is_empty() { "нет" } else { "да" };
        drop(snaps);
        ctx.log.as_ref()(&format!("архив: {}", ts.summary()));
        for e in &ts.errors { ctx.log.as_ref()(&format!("  пропущено: {e}")); }
        let meta = json!({ "paths": c.paths, "files": ts.files, "tar": ts, "fs_snapshot": snapshot_note });
        let (header, stats) = wr.finish(&ctx.instance, "files", "files", json!({ "files": ts.files, "skipped": ts.skipped }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let root = super::cfg_str(&ctx.params, "root").unwrap_or_else(|| "/".into());
        let verify = ctx.params.get("verify").and_then(|v| v.as_bool()).unwrap_or(true);
        let reload = ctx.params.get("reload").and_then(|v| v.as_bool()).unwrap_or(false);
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let s = m.streams.iter().find(|s| s.name == "files.tar").ok_or_else(|| anyhow::anyhow!("в копии нет файлов"))?;
        std::fs::create_dir_all(&root)?;
        ctx.log.as_ref()(&format!("распаковка в {root}"));
        let (reader, fetch) = ctx.repo.stream_reader(s)?;
        let dst = PathBuf::from(&root);
        let unpack = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut a = tar::Archive::new(reader);
            a.set_preserve_permissions(true);
            a.set_preserve_ownerships(crate::util::is_root());
            a.unpack(&dst)?;
            let mut rest = a.into_inner();
            let _ = std::io::copy(&mut rest, &mut std::io::sink());
            Ok(())
        });
        let (u, f) = tokio::join!(unpack, fetch);
        u??;
        f??;
        let vcmd = super::cfg_str(&ctx.params, "verify_cmd");
        let rcmd = super::cfg_str(&ctx.params, "reload_cmd");
        if root == "/" {
            if verify { if let Some(v) = &vcmd { sh(v, &ctx.log).await?; } }
            if reload { if let Some(r) = &rcmd { sh(r, &ctx.log).await?; } }
        }
        Ok(json!({ "root": root }))
    }
}
