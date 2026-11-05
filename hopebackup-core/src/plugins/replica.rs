//! Блочная реплика дисков машины — основа модуля «Переезд».
//!
//! Источник (агент на машине): диски читаются блоками по 4 МиБ, для каждого блока считается BLAKE3;
//! в копию попадают карта блоков диска и поток изменений — только блоки, чьи суммы отличаются от карты
//! прошлой синхронизации (первая — все ненулевые). Так каждая синхронизация передаёт только изменения,
//! а реплика в репозитории — цепочка «полная + изменения».
//!
//! Цель (агент-приёмник рядом с целевой платформой): применяет к томам цели изменения синхронизаций,
//! которые ещё не применены, — запись по смещениям в файл, блочное устройство или через imageio.
//!
//! Переключение: последняя синхронизация при остановленных службах и, по желанию, с заморозкой файловых
//! систем (fsfreeze) на время чтения; заморозку страхует отдельный процесс, который снимет её сам, если
//! агент не успеет.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use super::appkit::{self as kit, svc, Cmd};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::store::repo::Repo;
use crate::store::types::{Manifest, StreamEntry};

pub struct Replica;

pub const BLOCK: u64 = 4 << 20;
const HASH: usize = 16;
const MAGIC: &[u8; 8] = b"HBDELTA1";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "disks": { "type": "array", "items": { "type": "string" }, "title": "Диски", "description": "Блочные устройства (/dev/sda); пусто — все диски машины" },
        "final_freeze": { "type": "boolean", "title": "Заморозка ФС при переключении", "default": true, "description": "fsfreeze на время последней синхронизации — данные в одном состоянии" },
        "freeze_timeout": { "type": "integer", "title": "Предел заморозки, с", "default": 600 },
        "stop_services": { "type": "array", "items": { "type": "string" }, "title": "Остановить при переключении", "description": "Службы приложений (СУБД и т. п.) — останавливаются перед последней синхронизацией" },
        "source": { "type": "string", "enum": ["", "vsphere"], "title": "Источник", "description": "Пусто — агент на самой машине; vsphere — ВМ VMware без агента (снимок и экспорт)" },
        "url": { "type": "string", "title": "vCenter / ESXi", "description": "https://vcenter.corp" },
        "user": { "type": "string", "title": "Пользователь vSphere" },
        "password": { "type": "string", "title": "Пароль vSphere", "format": "password" },
        "insecure": { "type": "boolean", "title": "Не проверять сертификат", "default": false },
        "vm": { "type": "string", "title": "ВМ vSphere" },
        "quiesce": { "type": "boolean", "title": "Заморозка ФС через VMware Tools", "default": true },
        "shutdown_timeout": { "type": "integer", "title": "Ожидание выключения гостевой ОС, с", "default": 600 },
        "hard_poweroff": { "type": "boolean", "title": "Жёсткое выключение, если гостевая ОС не выключилась", "default": false },
        "guest_user": { "type": "string", "title": "Пользователь гостевой ОС", "description": "Для подготовки к KVM без агента (root или с sudo без пароля)" },
        "guest_password": { "type": "string", "title": "Пароль гостевой ОС", "format": "password" }
    } })
}

pub fn defaults() -> Value { json!({ "final_freeze": true, "freeze_timeout": 600 }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "targets": { "type": "object", "title": "Тома цели", "description": "Диск источника → том цели: путь к файлу или устройству, или imageio:<адрес передачи>" },
        "applied": { "type": "string", "title": "Уже применена синхронизация", "description": "Пусто — применить всю цепочку с первой синхронизации" },
        "platform": { "type": "string", "enum": ["", "proxmox", "ovirt"], "title": "Платформа цели", "description": "Создать ВМ и тома на платформе (вместо явных томов)" },
        "storage": { "type": "string", "title": "Хранилище", "description": "Proxmox — хранилище (local-lvm), oVirt — домен хранения" },
        "cluster": { "type": "string", "title": "Кластер oVirt" },
        "networks": { "type": "object", "title": "Сети", "description": "Интерфейс источника (или «*») → мост Proxmox или профиль сети oVirt" },
        "name": { "type": "string", "title": "Имя ВМ", "description": "По умолчанию — имя машины источника" },
        "start": { "type": "boolean", "title": "Запустить ВМ после применения", "default": false },
        "state": { "type": "object", "title": "Состояние цели", "description": "Из прошлого применения: ВМ и её тома" }
    } })
}

// ---------------------------------------------------------------- карта и поток изменений

/// Сумма блока; у нулевого блока — нули (чтобы такие блоки не передавать вовсе).
pub fn block_hash(b: &[u8]) -> [u8; HASH] {
    if b.iter().all(|x| *x == 0) { return [0; HASH]; }
    let h = blake3::hash(b);
    let mut o = [0u8; HASH];
    o.copy_from_slice(&h.as_bytes()[..HASH]);
    o
}

/// Итог чтения диска.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScanStats { pub size: u64, pub blocks: u64, pub changed: u64, pub changed_bytes: u64, pub zero: u64 }

/// Прочитать диск `src` (размер `size`), сравнить с картой `prev` и записать изменения в `out`.
/// Возвращает новую карту. Изменения — записи: смещение (u64), длина (u32), вид (0 — данные, 1 — нули).
pub fn scan<R: Read, W: Write>(mut src: R, size: u64, prev: &[u8], mut out: W, stats: &mut ScanStats) -> Result<Vec<u8>> {
    out.write_all(MAGIC)?;
    out.write_all(&size.to_le_bytes())?;
    out.write_all(&(BLOCK as u32).to_le_bytes())?;
    let n = size.div_ceil(BLOCK);
    let mut map = Vec::with_capacity(n as usize * HASH);
    let mut buf = vec![0u8; BLOCK as usize];
    stats.size = size;
    stats.blocks = n;
    for i in 0..n {
        let len = BLOCK.min(size - i * BLOCK) as usize;
        let b = &mut buf[..len];
        let mut got = 0;
        while got < len {
            let k = src.read(&mut b[got..])?;
            if k == 0 { bail!("диск короче ожидаемого: {} из {size} байт", i * BLOCK + got as u64); }
            got += k;
        }
        let h = block_hash(b);
        let zero = h == [0; HASH];
        if zero { stats.zero += 1; }
        let old = prev.get(i as usize * HASH..(i as usize + 1) * HASH);
        // Первая синхронизация: нулевые блоки не передаются (цель создаётся пустой). Дальше — любые изменения.
        let changed = match old { Some(o) => o != h, None => !zero };
        if changed {
            out.write_all(&(i * BLOCK).to_le_bytes())?;
            out.write_all(&(len as u32).to_le_bytes())?;
            if zero { out.write_all(&[1])?; } else { out.write_all(&[0])?; out.write_all(b)?; }
            stats.changed += 1;
            stats.changed_bytes += len as u64;
        }
        map.extend_from_slice(&h);
    }
    out.flush()?;
    Ok(map)
}

/// Куда применять изменения.
pub trait BlockSink {
    fn write_at(&mut self, off: u64, data: &[u8]) -> Result<()>;
    fn zero_at(&mut self, off: u64, len: u64) -> Result<()>;
    fn finish(&mut self) -> Result<()> { Ok(()) }
    /// Обнулить первые `size` байт тома.
    fn zero_all(&mut self, size: u64) -> Result<()> {
        let mut off = 0;
        while off < size { let n = BLOCK.min(size - off); self.zero_at(off, n)?; off += n; }
        Ok(())
    }
}

/// Применить поток изменений. Возвращает размер диска и число записанных блоков.
pub fn apply<R: Read>(mut r: R, sink: &mut dyn BlockSink) -> Result<(u64, u64)> {
    let mut m = [0u8; 8];
    r.read_exact(&mut m).context("поток изменений пуст")?;
    if &m != MAGIC { bail!("не поток изменений реплики"); }
    let mut b8 = [0u8; 8];
    let mut b4 = [0u8; 4];
    r.read_exact(&mut b8)?;
    let size = u64::from_le_bytes(b8);
    r.read_exact(&mut b4)?;
    let mut buf = vec![0u8; BLOCK as usize];
    let mut n = 0;
    loop {
        match r.read_exact(&mut b8) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break, Err(e) => return Err(e.into()) }
        let off = u64::from_le_bytes(b8);
        r.read_exact(&mut b4)?;
        let len = u32::from_le_bytes(b4) as usize;
        if len > buf.len() || off + len as u64 > size { bail!("повреждённая запись изменений: {off}+{len} при размере {size}"); }
        let mut kind = [0u8; 1];
        r.read_exact(&mut kind)?;
        if kind[0] == 1 { sink.zero_at(off, len as u64)?; } else { r.read_exact(&mut buf[..len])?; sink.write_at(off, &buf[..len])?; }
        n += 1;
    }
    sink.finish()?;
    Ok((size, n))
}

/// Файл образа или блочное устройство.
pub struct FileSink { f: std::fs::File, zeros: Vec<u8>, is_dev: bool }

impl FileSink {
    pub fn open(path: &str, size: u64) -> Result<FileSink> {
        let p = Path::new(path);
        let is_dev = std::fs::metadata(p).map(|m| { use std::os::unix::fs::FileTypeExt; m.file_type().is_block_device() }).unwrap_or(false);
        let f = std::fs::OpenOptions::new().write(true).create(!is_dev).truncate(false).open(p).with_context(|| format!("том цели {path}"))?;
        if !is_dev {
            if f.metadata()?.len() < size { f.set_len(size)?; }
        } else {
            let mut g = std::fs::File::open(p)?;
            let dev = g.seek(SeekFrom::End(0))?;
            if dev < size { bail!("том цели {path} меньше диска источника: {} < {}", crate::util::human_bytes(dev), crate::util::human_bytes(size)); }
        }
        Ok(FileSink { f, zeros: vec![0u8; BLOCK as usize], is_dev })
    }
}

impl BlockSink for FileSink {
    fn write_at(&mut self, off: u64, data: &[u8]) -> Result<()> { use std::os::unix::fs::FileExt; self.f.write_all_at(data, off)?; Ok(()) }
    fn zero_at(&mut self, off: u64, len: u64) -> Result<()> { use std::os::unix::fs::FileExt; self.f.write_all_at(&self.zeros[..len as usize], off)?; Ok(()) }
    fn finish(&mut self) -> Result<()> { self.f.sync_all()?; Ok(()) }
    fn zero_all(&mut self, size: u64) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        if self.is_dev {
            // BLKZEROOUT: хранилище обнуляет само (WRITE SAME / разреженно), без передачи нулей.
            let range: [u64; 2] = [0, size & !511];
            if unsafe { libc::ioctl(self.f.as_raw_fd(), 0x127f /* BLKZEROOUT */, range.as_ptr()) } == 0 {
                if size % 512 != 0 { self.zero_at(size & !511, size % 512)?; }
                return Ok(());
            }
        } else {
            // Файл: пробить дыру — нули без записи.
            if unsafe { libc::fallocate(self.f.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 0, size as i64) } == 0 { return Ok(()); }
        }
        let mut off = 0;
        while off < size { let n = BLOCK.min(size - off); self.zero_at(off, n)?; off += n; }
        Ok(())
    }
}

/// Запись через imageio (oVirt, zVirt, РОСА): PUT с Content-Range, нули — PATCH zero. Запросы — из потока
/// применения (не из рантайма), через его дескриптор.
pub struct ImageioSink { c: reqwest::Client, rt: tokio::runtime::Handle, url: String, zero_op: bool }

impl ImageioSink {
    pub fn new(url: &str, insecure: bool, rt: tokio::runtime::Handle) -> Result<ImageioSink> {
        let c = reqwest::Client::builder().danger_accept_invalid_certs(insecure).timeout(std::time::Duration::from_secs(600)).build()?;
        let zero_op = rt.block_on(async { c.request(reqwest::Method::OPTIONS, url).send().await.ok()?.json::<Value>().await.ok() })
            .map(|v| v["features"].as_array().map(|a| a.iter().any(|x| x == "zero")).unwrap_or(false)).unwrap_or(false);
        Ok(ImageioSink { c, rt, url: url.into(), zero_op })
    }
    fn send(&self, rb: reqwest::RequestBuilder, what: &str) -> Result<()> {
        let r = self.rt.block_on(rb.send()).with_context(|| format!("imageio {what}"))?;
        if !r.status().is_success() { bail!("imageio {what}: {}", r.status()); }
        Ok(())
    }
}

impl BlockSink for ImageioSink {
    fn write_at(&mut self, off: u64, data: &[u8]) -> Result<()> {
        let rb = self.c.put(format!("{}?flush=n", self.url)).header("Content-Range", format!("bytes {}-{}/*", off, off + data.len() as u64 - 1)).body(data.to_vec());
        self.send(rb, &format!("PUT {off}"))
    }
    fn zero_at(&mut self, off: u64, len: u64) -> Result<()> {
        if !self.zero_op { return self.write_at(off, &vec![0u8; len as usize]); }
        let rb = self.c.patch(&self.url).json(&json!({ "op": "zero", "offset": off, "size": len, "flush": false }));
        self.send(rb, &format!("zero {off}"))
    }
    fn finish(&mut self) -> Result<()> { let rb = self.c.patch(&self.url).json(&json!({ "op": "flush" })); self.send(rb, "flush") }
    fn zero_all(&mut self, size: u64) -> Result<()> {
        if !self.zero_op { let mut off = 0; while off < size { let n = BLOCK.min(size - off); self.zero_at(off, n)?; off += n; } return Ok(()); }
        let rb = self.c.patch(&self.url).json(&json!({ "op": "zero", "offset": 0, "size": size, "flush": false }));
        self.send(rb, "zero")
    }
}

// ---------------------------------------------------------------- источник

/// Диски машины (lsblk): целые диски, не съёмные, без loop/ram/CD.
fn all_disks() -> Result<Vec<String>> {
    let o = std::process::Command::new("lsblk").args(["-dnbo", "NAME,TYPE,RM,SIZE"]).output().context("lsblk")?;
    Ok(String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 4 && f[1] == "disk" && f[2] == "0" && f[3] != "0").then(|| format!("/dev/{}", f[0]))
    }).collect())
}

/// Чтение диска мимо кэша (O_DIRECT): данные файлов ФС пишутся на устройство в обход его страничного кэша,
/// и обычное чтение устройства могло бы вернуть устаревшие страницы. Буфер выровнен по 4 КиБ; если
/// O_DIRECT не поддерживается (файл образа на tmpfs) — обычное чтение со сбросом кэша.
pub struct DirectReader { f: std::fs::File, buf: *mut u8, cap: usize, pos: usize, len: usize, direct: bool }

unsafe impl Send for DirectReader {}

impl DirectReader {
    pub fn open(path: &str) -> Result<DirectReader> {
        use std::os::unix::fs::OpenOptionsExt;
        let cap = BLOCK as usize;
        let buf = unsafe { std::alloc::alloc(std::alloc::Layout::from_size_align(cap, 4096).unwrap()) };
        if buf.is_null() { bail!("нет памяти под буфер чтения"); }
        let (f, direct) = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path) {
            Ok(f) => (f, true),
            Err(_) => {
                let f = std::fs::File::open(path).with_context(|| format!("диск {path}"))?;
                unsafe { libc::ioctl(std::os::unix::io::AsRawFd::as_raw_fd(&f), 0x1261 /* BLKFLSBUF */); libc::posix_fadvise(std::os::unix::io::AsRawFd::as_raw_fd(&f), 0, 0, libc::POSIX_FADV_DONTNEED); }
                (f, false)
            }
        };
        Ok(DirectReader { f, buf, cap, pos: 0, len: 0, direct })
    }
}

impl Read for DirectReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len {
            let b = unsafe { std::slice::from_raw_parts_mut(self.buf, self.cap) };
            let n = match self.f.read(b) {
                Ok(n) => n,
                // Хвост устройства, не кратный сектору, с O_DIRECT не читается — дочитываем обычным чтением.
                Err(e) if self.direct && e.raw_os_error() == Some(libc::EINVAL) => {
                    let off = self.f.stream_position()?;
                    let mut g = std::fs::File::open(format!("/proc/self/fd/{}", std::os::unix::io::AsRawFd::as_raw_fd(&self.f)))?;
                    g.seek(SeekFrom::Start(off))?;
                    let n = g.read(b)?;
                    self.f.seek(SeekFrom::Start(off + n as u64))?;
                    n
                }
                Err(e) => return Err(e),
            };
            if n == 0 { return Ok(0); }
            self.pos = 0;
            self.len = n;
        }
        let k = out.len().min(self.len - self.pos);
        out[..k].copy_from_slice(unsafe { std::slice::from_raw_parts(self.buf.add(self.pos), k) });
        self.pos += k;
        Ok(k)
    }
}

impl Drop for DirectReader {
    fn drop(&mut self) { unsafe { std::alloc::dealloc(self.buf, std::alloc::Layout::from_size_align(self.cap, 4096).unwrap()); } }
}

fn dev_size(path: &str) -> Result<u64> {
    let mut f = std::fs::File::open(path).with_context(|| format!("диск {path}"))?;
    Ok(f.seek(SeekFrom::End(0))?)
}

fn disk_id(path: &str) -> String { path.trim_start_matches("/dev/").replace('/', "_") }

/// Сведения о машине для создания ВМ на цели: процессоры, память, сеть (MAC — сохранить на цели).
pub(super) fn machine_info() -> Value {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().find_map(|l| l.strip_prefix("MemTotal:").map(|x| x.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) * 1024)).unwrap_or(0);
    let mut nics = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n == "lo" || !e.path().join("device").exists() { continue; }
            let mac = std::fs::read_to_string(e.path().join("address")).unwrap_or_default().trim().to_string();
            nics.push(json!({ "name": n, "mac": mac }));
        }
    }
    let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default().lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|x| x.trim_matches('"').to_string())).unwrap_or_default();
    let efi = Path::new("/sys/firmware/efi").exists();
    json!({ "hostname": crate::util::hostname(), "os": os, "cpus": cpus, "memory": mem, "nics": nics, "firmware": if efi { "uefi" } else { "bios" } })
}

/// Смонтированные ФС, лежащие на этих дисках (для заморозки), — корень последним (его морозим позже всех).
fn mounts_on(disks: &[String]) -> Vec<String> {
    let o = std::process::Command::new("lsblk").args(["-nlpo", "NAME,MOUNTPOINT"]).args(disks).output();
    let mut v: Vec<String> = o.map(|o| String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.split_once(' ').map(|(_, m)| m.trim().to_string())).filter(|m| m.starts_with('/') && !m.starts_with("/boot/efi") && m != "[SWAP]").collect()).unwrap_or_default();
    v.sort();
    v.dedup();
    v.sort_by_key(|m| if m == "/" { 1 } else { 0 });
    v
}

/// Заморозка ФС на время чтения; страховочный процесс снимет её сам через `timeout` секунд.
struct Freeze { mounts: Vec<String>, guard: Option<std::process::Child> }

impl Freeze {
    fn new(mounts: Vec<String>, timeout: u64, log: &LogFn) -> Result<Freeze> {
        let thaw: String = mounts.iter().map(|m| format!("fsfreeze -u {} 2>/dev/null; ", crate::util::shq(m))).collect();
        let guard = std::process::Command::new("setsid").args(["sh", "-c", &format!("sleep {timeout}; {thaw}")]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok();
        let mut done: Vec<String> = Vec::new();
        for m in &mounts {
            let st = std::process::Command::new("fsfreeze").args(["-f", m]).status();
            if !st.map(|s| s.success()).unwrap_or(false) {
                for d in done.iter().rev() { let _ = std::process::Command::new("fsfreeze").args(["-u", d.as_str()]).status(); }
                bail!("fsfreeze {m} не удался");
            }
            done.push(m.clone());
        }
        log(&format!("файловые системы заморожены: {} (снимется не позже чем через {timeout} с)", mounts.join(", ")));
        Ok(Freeze { mounts: done, guard })
    }
}

impl Drop for Freeze {
    fn drop(&mut self) {
        for m in self.mounts.iter().rev() { let _ = std::process::Command::new("fsfreeze").args(["-u", m]).status(); }
        if let Some(mut g) = self.guard.take() { let _ = g.kill(); let _ = g.wait(); }
    }
}

pub(super) async fn prev_maps(repo: &Arc<Repo>, instance: &str) -> Result<(Option<String>, Manifest)> {
    let snaps = kit::snapshots_of(repo, instance).await?;
    match snaps.last() {
        Some(h) => { let (_, m) = repo.manifest(&h.id).await?; Ok((Some(h.id.clone()), m)) }
        None => Ok((None, Manifest { streams: vec![], meta: Value::Null, images: vec![] })),
    }
}

#[async_trait]
impl Plugin for Replica {
    fn id(&self) -> &'static str { "replica" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        if cfg_str(cfg, "source").as_deref() == Some("vsphere") {
            let vs = super::vm::vmware::Vsphere::login(cfg, log).await?;
            let vm = vs.find_vm(&cfg_str(cfg, "vm").unwrap_or_default()).await?;
            let (m, power) = super::replica_vs::machine(&vs, &vm).await?;
            return Ok(json!({ "machine": m, "power": power, "source": "vsphere" }));
        }
        let disks = { let d = crate::connectors::net::p_list(cfg, "disks"); if d.is_empty() { all_disks()? } else { d } };
        let list: Vec<Value> = disks.iter().map(|d| json!({ "disk": d, "size": dev_size(d).unwrap_or(0) })).collect();
        Ok(json!({ "disks": list, "machine": machine_info(), "mounts": mounts_on(&disks) }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        if cfg_str(&ctx.config, "source").as_deref() == Some("vsphere") { return super::replica_vs::backup(ctx).await; }
        let log = &ctx.log;
        let disks = { let d = crate::connectors::net::p_list(&ctx.config, "disks"); if d.is_empty() { all_disks()? } else { d } };
        if disks.is_empty() { bail!("нет дисков для реплики"); }
        let cutover = ctx.kind == "cutover";
        let (prev_id, prev) = prev_maps(&ctx.repo, &ctx.instance).await?;
        log(&format!("реплика {}: {}{}", if prev_id.is_some() { "изменения" } else { "первая синхронизация" }, disks.join(", "), if cutover { " · переключение" } else { "" }));
        // Переключение: службы приложений — остановить, ФС — заморозить на время чтения.
        let stop = crate::connectors::net::p_list(&ctx.config, "stop_services");
        if cutover { for s in &stop { if svc::active(s) { svc::stop(s, log).await?; } } }
        let _freeze = if cutover && kit::bool_param(&ctx.config, "final_freeze", true) {
            let _ = kit::run(&Cmd::new("sync"), log).await;
            Some(Freeze::new(mounts_on(&disks), ctx.config.get("freeze_timeout").and_then(|x| x.as_u64()).unwrap_or(600), log)?)
        } else { None };
        let mut w = ctx.repo.writer();
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        for d in &disks {
            let id = disk_id(d);
            let size = dev_size(d)?;
            let pm = match kit::stream(&prev, &format!("{id}.map")) { Ok(s) => kit::read_small(&ctx.repo, s).await.unwrap_or_default(), Err(_) => Vec::new() };
            let prev_size = prev.meta["disks"].as_array().and_then(|a| a.iter().find(|x| x["id"] == id.as_str())).and_then(|x| x["size"].as_u64());
            let pm = if prev_size == Some(size) { pm } else { if prev_size.is_some() { log(&format!("  {d}: размер изменился — диск читается заново")); } Vec::new() };
            let (r, wp) = std::io::pipe()?;
            let path = d.clone();
            let stats = Arc::new(parking_lot::Mutex::new(ScanStats::default()));
            let st2 = stats.clone();
            let prod = std::thread::spawn(move || -> Result<Vec<u8>> {
                let f = DirectReader::open(&path)?;
                let mut s = ScanStats::default();
                let map = scan(f, size, &pm, std::io::BufWriter::with_capacity(1 << 20, wp), &mut s)?;
                *st2.lock() = s;
                Ok(map)
            });
            let res = w.add_stream(&format!("{id}.delta"), r, None).await;
            let map = prod.join().map_err(|_| anyhow!("чтение диска аварийно завершилось"))??;
            res?;
            kit::add_bytes(&mut w, &format!("{id}.map"), map).await?;
            let s = stats.lock().clone();
            log(&format!("  {d}: {} · изменено блоков {} из {} ({})", crate::util::human_bytes(size), s.changed, s.blocks, crate::util::human_bytes(s.changed_bytes)));
            out.push(json!({ "id": id, "path": d, "size": size, "changed": s.changed, "changed_bytes": s.changed_bytes, "zero_blocks": s.zero }));
        }
        drop(_freeze);
        let secs = t0.elapsed().as_secs_f64();
        let meta = json!({ "disks": out, "machine": machine_info(), "prev": prev_id, "cutover": cutover, "read_seconds": secs });
        let kind = if cutover { "cutover" } else { "sync" };
        let (header, stats) = w.finish(&ctx.instance, "replica", kind, json!({ "prev": prev_id, "disks": disks.len() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    /// Применить цепочку синхронизаций (после `applied` и до выбранной) к томам цели: заданным явно
    /// (`targets`) или созданным на платформе (`platform` — ВМ и тома при первом применении, дальше —
    /// по `state`, которое возвращает прошлое применение).
    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let snaps = kit::snapshots_of(&ctx.repo, &ctx.instance).await?;
        let upto = snaps.iter().position(|s| s.id == ctx.snapshot_id).ok_or_else(|| anyhow!("нет синхронизации {}", ctx.snapshot_id))?;
        let applied = cfg_str(&ctx.params, "applied").or_else(|| ctx.params["state"]["applied"].as_str().map(String::from));
        // Тома изменены на цели (тест или работа без снимка): обнулить и применить всю цепочку заново.
        let dirty = ctx.params["state"]["dirty"].as_bool().unwrap_or(false);
        if !ctx.params["state"]["testing"].is_null() && ctx.params["state"].is_object() { bail!("идёт тестовый запуск ВМ на цели — сначала завершите его"); }
        let from = match &applied { _ if dirty => 0, Some(a) => snaps.iter().position(|s| s.id == *a).map(|i| i + 1).ok_or_else(|| anyhow!("применённой синхронизации {a} нет в репозитории"))?, None => 0 };
        if from > upto { bail!("синхронизация {} уже применена", ctx.snapshot_id); }
        let insecure = kit::bool_param(&ctx.params, "insecure", true);
        let mut chain = Vec::new();
        for s in &snaps[from..=upto] { chain.push((s.id.clone(), ctx.repo.manifest(&s.id).await?.1)); }
        // Диски — по последней синхронизации (размер — нынешний); машина — тоже.
        let last = &chain.last().unwrap().1.meta;
        let mut plan: Vec<super::migrate::DiskPlan> = Vec::new();
        for (_, m) in chain.iter().rev() {
            for d in m.meta["disks"].as_array().into_iter().flatten() {
                let id = d["id"].as_str().unwrap_or("").to_string();
                if !plan.iter().any(|p| p.id == id) { plan.push(super::migrate::DiskPlan { id, size: d["size"].as_u64().unwrap_or(0) }); }
            }
        }
        let order: Vec<String> = last["disks"].as_array().into_iter().flatten().filter_map(|d| d["id"].as_str().map(String::from)).collect();
        plan.sort_by_key(|p| order.iter().position(|x| *x == p.id).unwrap_or(usize::MAX));
        let platform = cfg_str(&ctx.params, "platform").unwrap_or_default();
        let mut tgt = if platform.is_empty() { None } else { Some(super::migrate::target(&platform, &ctx.params)?) };
        let vols: Vec<super::migrate::Vol> = match &mut tgt {
            Some(t) => {
                let mut v = t.open(&plan, &last["machine"], log).await?;
                if dirty { log("  тома изменены на цели — будут обнулены и записаны заново всей цепочкой"); for x in &mut v { x.prezero = true; } }
                v
            }
            None => {
                let targets = ctx.params["targets"].as_object().cloned().ok_or_else(|| anyhow!("не заданы тома цели (targets) или платформа (platform)"))?;
                let mut v = Vec::new();
                for (_, m) in &chain {
                    for d in m.meta["disks"].as_array().into_iter().flatten() {
                        let id = d["id"].as_str().unwrap_or("").to_string();
                        if v.iter().any(|x: &super::migrate::Vol| x.disk == id) { continue; }
                        let t = targets.get(&id).or_else(|| targets.get(d["path"].as_str().unwrap_or(""))).and_then(|x| x.as_str()).ok_or_else(|| anyhow!("нет тома цели для диска {id}"))?;
                        v.push(super::migrate::Vol { disk: id, target: t.to_string(), prezero: false });
                    }
                }
                v
            }
        };
        let res = async {
            let mut total = 0u64;
            for v in vols.iter().filter(|v| v.prezero) {
                let size = plan.iter().find(|p| p.id == v.disk).map(|p| p.size).unwrap_or(0);
                log(&format!("  {}: новый том не обнулён хранилищем — обнуление {}", v.disk, crate::util::human_bytes(size)));
                zero_volume(&v.target, size, insecure).await.with_context(|| format!("обнуление тома {}", v.target))?;
            }
            for (sid, m) in &chain {
                for d in m.meta["disks"].as_array().cloned().unwrap_or_default() {
                    let id = d["id"].as_str().unwrap_or("").to_string();
                    let size = d["size"].as_u64().unwrap_or(0);
                    let v = vols.iter().find(|v| v.disk == id).ok_or_else(|| anyhow!("нет тома цели для диска {id}"))?;
                    let e = kit::stream(m, &format!("{id}.delta"))?.clone();
                    let n = apply_stream(&ctx.repo, &e, &v.target, size, insecure).await.with_context(|| format!("диск {id} → {}", v.target))?;
                    total += n;
                    log(&format!("  {sid} · {id}: записано блоков {n}"));
                }
            }
            Ok::<_, anyhow::Error>(total)
        }.await;
        let state = match &mut tgt { Some(t) => Some(t.close(res.is_ok(), log).await), None => None };
        let total = res?;
        let mut state = match state { Some(s) => s?, None => Value::Null };
        if state.is_object() { state["applied"] = json!(ctx.snapshot_id); if let Some(o) = state.as_object_mut() { o.remove("dirty"); } }
        let cut = chain.last().map(|(_, m)| m.meta["cutover"].as_bool().unwrap_or(false)).unwrap_or(false);
        Ok(json!({ "applied": ctx.snapshot_id, "syncs": chain.len(), "blocks": total, "cutover": cut, "state": state }))
    }

    async fn action(&self, name: &str, cfg: &Value, _instance: &str, log: &LogFn) -> Result<Value> {
        match name {
            // На источнике.
            "prepare-kvm" => prepare_kvm(cfg, log).await,
            "poweroff" => poweroff(cfg, log),
            "resume" if cfg_str(cfg, "source").as_deref() == Some("vsphere") => super::replica_vs::power_on(cfg, log).await,
            "resume" => {
                let stop = crate::connectors::net::p_list(cfg, "stop_services");
                for s in &stop { kit::run(&Cmd::new("systemctl").args(["start", s.as_str()]), log).await?; }
                Ok(json!({ "started": stop }))
            }
            // На стороне цели: управление ВМ по состоянию переезда.
            n if n.starts_with("target-") => {
                let platform = cfg_str(cfg, "platform").ok_or_else(|| anyhow!("не задана платформа цели"))?;
                // Запуск после выключения источника — когда тот точно выключен (те же MAC и адреса).
                if let Some(d) = cfg.get("start_delay").and_then(|x| x.as_u64()).filter(|_| n == "target-start") {
                    log(&format!("запуск ВМ на цели через {d} с — после выключения источника"));
                    tokio::time::sleep(std::time::Duration::from_secs(d)).await;
                }
                let st = super::migrate::power(&platform, cfg, &n["target-".len()..], log).await?;
                Ok(json!({ "state": st }))
            }
            _ => bail!("действие «{name}» не поддерживается (есть: prepare-kvm, poweroff, resume, target-…)"),
        }
    }
}

/// Выключить источник после последней синхронизации (чтобы ВМ на цели с теми же MAC и адресами не
/// конфликтовала с ним). Через `delay` секунд — агент успевает сообщить итог.
fn poweroff(cfg: &Value, log: &LogFn) -> Result<Value> {
    let delay = cfg.get("delay").and_then(|x| x.as_u64()).unwrap_or(20).max(5);
    if std::env::var_os("HOPEBACKUP_NO_POWEROFF").is_some() { log(&format!("выключение источника через {delay} с (пропущено: HOPEBACKUP_NO_POWEROFF)")); return Ok(json!({ "poweroff_in": delay, "skipped": true })); }
    std::process::Command::new("setsid").args(["sh", "-c", &format!("sleep {delay}; systemctl poweroff || poweroff")])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().context("выключение")?;
    log(&format!("источник выключится через {delay} с"));
    Ok(json!({ "poweroff_in": delay }))
}

/// Подготовка Linux к KVM на работающем источнике (см. replica_prep.sh).
async fn prepare_kvm(cfg: &Value, log: &LogFn) -> Result<Value> {
    log("подготовка к KVM: драйверы virtio в initramfs, имена интерфейсов по MAC, fstab по UUID, гостевой агент");
    let o = if cfg_str(cfg, "source").as_deref() == Some("vsphere") {
        super::replica_vs::prepare(cfg, include_str!("replica_prep.sh"), log).await?
    } else {
        if !cfg!(target_os = "linux") { bail!("подготовка к KVM — только для Linux"); }
        if unsafe { libc::geteuid() } != 0 { bail!("нужны права root"); }
        let wd = kit::WorkDir::new(None, None)?;
        let f = wd.join("prep.sh");
        std::fs::write(&f, include_str!("replica_prep.sh"))?;
        kit::output(&Cmd::new("sh").arg(f.to_string_lossy().to_string()), log).await?
    };
    if !o.contains("HB-PREP-DONE") { bail!("подготовка прервалась: {}", o.lines().last().unwrap_or("")); }
    let lines: Vec<&str> = o.lines().filter(|l| *l != "HB-PREP-DONE" && !l.trim().is_empty()).collect();
    for l in &lines { log(&format!("  {l}")); }
    let warn: Vec<&str> = lines.iter().filter(|l| l.starts_with('!')).map(|l| l.trim_start_matches("! ")).collect();
    Ok(json!({ "done": lines.iter().filter(|l| !l.starts_with('!')).collect::<Vec<_>>(), "warnings": warn, "ready": warn.is_empty() }))
}

/// Обнулить весь том цели (новый том толстого LVM или блочного домена oVirt).
async fn zero_volume(target: &str, size: u64, insecure: bool) -> Result<()> {
    let tgt = target.to_string();
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut sink: Box<dyn BlockSink> = match tgt.strip_prefix("imageio:") { Some(u) => Box::new(ImageioSink::new(u, insecure, rt)?), None => Box::new(FileSink::open(&tgt, size)?) };
        sink.zero_all(size)?;
        sink.finish()
    }).await?
}

/// Применить один поток изменений к тому цели.
pub async fn apply_stream(repo: &Arc<Repo>, e: &StreamEntry, target: &str, size: u64, insecure: bool) -> Result<u64> {
    let (reader, fetch) = repo.stream_reader(e)?;
    let tgt = target.to_string();
    let rt = tokio::runtime::Handle::current();
    let job = tokio::task::spawn_blocking(move || -> Result<u64> {
        let mut sink: Box<dyn BlockSink> = match tgt.strip_prefix("imageio:") { Some(u) => Box::new(ImageioSink::new(u, insecure, rt)?), None => Box::new(FileSink::open(&tgt, size)?) };
        let mut r = reader;
        let res = apply(std::io::BufReader::with_capacity(1 << 20, &mut r), sink.as_mut());
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        res.map(|x| x.1)
    });
    let (a, b) = tokio::join!(job, fetch);
    let n = a??;
    b??;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mem(Vec<u8>);
    impl BlockSink for Mem {
        fn write_at(&mut self, off: u64, d: &[u8]) -> Result<()> { self.0[off as usize..off as usize + d.len()].copy_from_slice(d); Ok(()) }
        fn zero_at(&mut self, off: u64, len: u64) -> Result<()> { self.0[off as usize..(off + len) as usize].fill(0); Ok(()) }
    }
    #[test]
    fn delta_roundtrip() {
        let size = BLOCK * 3 + 1000;
        let mut a = vec![0u8; size as usize];
        a[10] = 1; a[(BLOCK * 3 + 5) as usize] = 9;
        let mut d1 = Vec::new(); let mut s = ScanStats::default();
        let m1 = scan(&a[..], size, &[], &mut d1, &mut s).unwrap();
        assert_eq!(s.changed, 2);
        let mut tgt = Mem(vec![0u8; size as usize]);
        apply(&d1[..], &mut tgt).unwrap();
        assert_eq!(tgt.0, a);
        // Второй синхронизацией: изменён блок 1, блок 0 обнулён.
        a[(BLOCK + 7) as usize] = 5; a[10] = 0;
        let mut d2 = Vec::new(); let mut s2 = ScanStats::default();
        scan(&a[..], size, &m1, &mut d2, &mut s2).unwrap();
        assert_eq!(s2.changed, 2);
        assert!(d2.len() < BLOCK as usize + 100, "нулевой блок передаётся без данных");
        apply(&d2[..], &mut tgt).unwrap();
        assert_eq!(tgt.0, a);
    }
}
