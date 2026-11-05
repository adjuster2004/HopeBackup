//! Сборка файлов образа (qcow2, vmdk streamOptimized, vhd, raw) из блоков образа и развёртывание ВМ или
//! выгрузка файлов. Образ пишется в файл с «дырами» на месте нулевых блоков (разрежённо — на диске занимает
//! столько, сколько реальных данных, а не полный размер диска), затем при необходимости переводится в нужный
//! формат через `qemu-img` (проверяем его наличие).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::os::unix::fs::FileExt;

use super::super::migrate::{self, DiskPlan};
use super::super::replica::{BlockSink, FileSink, ImageioSink};
use super::super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::store::repo::{ImageSink, ZERO_ID};
use crate::store::types::ImageEntry;

/// Расширение файла для формата.
pub fn ext(format: &str) -> &'static str { match format { "vmdk" => "vmdk", "vhd" => "vhd", "raw" => "img", _ => "qcow2" } }

/// Файл с «дырами»: нулевые блоки не пишутся (разрежённость), данные — по смещению.
struct SparseFile { f: std::fs::File }
impl ImageSink for SparseFile {
    fn data(&mut self, off: u64, b: &[u8]) -> Result<()> { self.f.write_all_at(b, off)?; Ok(()) }
    fn zero(&mut self, _off: u64, _len: u64) -> Result<()> { Ok(()) }
    fn finish(&mut self) -> Result<()> { self.f.sync_all()?; Ok(()) }
}

/// Запись образа в том гипервизора (поверх блочного приёмника реплики): данные — по смещению, нули —
/// операцией обнуления (на imageio — без передачи нулей).
struct VolSink(Box<dyn BlockSink + Send>);
impl ImageSink for VolSink {
    fn data(&mut self, off: u64, b: &[u8]) -> Result<()> { self.0.write_at(off, b) }
    fn zero(&mut self, off: u64, len: u64) -> Result<()> { self.0.zero_at(off, len) }
    fn finish(&mut self) -> Result<()> { self.0.finish() }
}

/// Записать образ разрежённым raw-файлом по пути `path`.
async fn write_raw(repo: &std::sync::Arc<crate::store::repo::Repo>, img: &ImageEntry, path: &std::path::Path) -> Result<u64> {
    let f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path).with_context(|| format!("файл образа {}", path.display()))?;
    f.set_len(img.size)?;
    repo.restore_image_sparse(img, Box::new(SparseFile { f })).await
}

/// Собрать файл образа диска в формате `format` по пути `path`. Для не-raw форматов нужен `qemu-img`.
pub async fn build(repo: &std::sync::Arc<crate::store::repo::Repo>, img: &ImageEntry, path: &std::path::Path, format: &str) -> Result<u64> {
    build_opt(repo, img, path, format, false).await
}

/// То же; `stable` — qcow2 с закреплённой раскладкой (`preallocation=metadata`): данные каждого участка диска
/// всегда на одном месте файла, пустое — дыры (разрежённо). Тогда следующий набор отличается от прошлого
/// только изменившимися участками — так он передаётся на удалённую площадку (см. `filesync`).
pub async fn build_opt(repo: &std::sync::Arc<crate::store::repo::Repo>, img: &ImageEntry, path: &std::path::Path, format: &str, stable: bool) -> Result<u64> {
    if format == "raw" { return write_raw(repo, img, path).await; }
    if crate::plugins::appkit::which("qemu-img").is_none() { bail!("для формата {format} нужен qemu-img (установите qemu-utils / qemu-img)"); }
    let tmp = path.with_extension("raw.part");
    let n = match write_raw(repo, img, &tmp).await { Ok(n) => n, Err(e) => { let _ = std::fs::remove_file(&tmp); return Err(e); } };
    let out = path.to_string_lossy().to_string();
    let src = tmp.to_string_lossy().to_string();
    let mut args = vec!["convert".into(), "-f".into(), "raw".into(), "-O".into()];
    match format {
        "qcow2" => { args.push("qcow2".into()); if stable { args.extend(["-o".into(), "preallocation=metadata".into()]); } }
        "vmdk" => { args.extend(["vmdk".into(), "-o".into(), "subformat=streamOptimized".into()]); }
        "vhd" => { args.extend(["vpc".into(), "-o".into(), "subformat=dynamic".into()]); }
        _ => bail!("формат {format} не поддерживается (qcow2, vmdk, vhd, raw)"),
    }
    args.push(src.clone());
    args.push(out);
    let log = crate::plugins::stdout_log();
    let r = crate::plugins::appkit::run(&crate::plugins::appkit::Cmd::new("qemu-img").args(args), &log).await;
    let _ = std::fs::remove_file(&tmp);
    if r.is_err() { let _ = std::fs::remove_file(path); }
    r.with_context(|| format!("qemu-img convert → {format}"))?;
    Ok(n)
}

/// Приёмник для тома цели: файл/устройство или imageio.
fn vol_sink(target: &str, size: u64, insecure: bool) -> Result<Box<dyn ImageSink>> {
    let rt = tokio::runtime::Handle::current();
    let s: Box<dyn BlockSink + Send> = match target.strip_prefix("imageio:") {
        Some(u) => Box::new(ImageioSink::new(u, insecure, rt)?),
        None => Box::new(FileSink::open(target, size)?),
    };
    Ok(Box::new(VolSink(s)))
}

/// Восстановление образа машины: развернуть ВМ на платформе или выгрузить файлы образа в каталог.
pub async fn deploy_or_export(ctx: &RestoreCtx) -> Result<Value> {
    let log = &ctx.log;
    let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
    if m.images.is_empty() { bail!("в снапшоте {} нет образов дисков", ctx.snapshot_id); }
    let mode = cfg_str(&ctx.params, "mode").unwrap_or_else(|| "deploy".into());
    let machine = m.meta.get("machine").cloned().unwrap_or(Value::Null);

    match mode.as_str() {
        "export" => {
            let dir = cfg_str(&ctx.params, "dir").or_else(|| cfg_str(&ctx.params, "root")).ok_or_else(|| anyhow!("не задан каталог выгрузки (dir)"))?;
            let format = cfg_str(&ctx.params, "format").unwrap_or_else(|| "qcow2".into());
            std::fs::create_dir_all(&dir).with_context(|| format!("каталог {dir}"))?;
            let mut files = Vec::new();
            for img in &m.images {
                let path = std::path::Path::new(&dir).join(format!("{}.{}", img.name, ext(&format)));
                log(&format!("выгрузка {} → {} ({})", img.name, path.display(), format));
                build(&ctx.repo, img, &path, &format).await?;
                files.push(json!({ "disk": img.name, "file": path.to_string_lossy(), "size": img.size }));
            }
            Ok(json!({ "mode": "export", "format": format, "dir": dir, "files": files }))
        }
        "deploy" => {
            let platform = cfg_str(&ctx.params, "platform").ok_or_else(|| anyhow!("не задана платформа (platform): proxmox, libvirt, ovirt, openstack"))?;
            if platform == "openstack" { return openstack_deploy(ctx, &m.images, &machine).await; }
            let insecure = crate::plugins::appkit::bool_param(&ctx.params, "insecure", true);
            let plan: Vec<DiskPlan> = m.images.iter().map(|i| DiskPlan { id: i.name.clone(), size: i.size }).collect();
            let mut tgt = migrate::target(&platform, &ctx.params)?;
            let vols = tgt.open(&plan, &machine, log).await?;
            let res = async {
                let mut total = 0u64;
                for v in &vols {
                    let img = m.images.iter().find(|i| i.name == v.disk).ok_or_else(|| anyhow!("нет образа для диска {}", v.disk))?;
                    log(&format!("  запись образа {} → {}", img.name, v.target));
                    let sink = vol_sink(&v.target, img.size, insecure)?;
                    total += ctx.repo.restore_image_sparse(img, sink).await.with_context(|| format!("запись образа {} в {}", img.name, v.target))?;
                }
                Ok::<_, anyhow::Error>(total)
            }.await;
            let state = tgt.close(res.is_ok(), log).await;
            let total = res?;
            let state = state?;
            Ok(json!({ "mode": "deploy", "platform": platform, "disks": m.images.len(), "bytes": total, "state": state }))
        }
        _ => bail!("mode: deploy или export"),
    }
}

/// Развёртывание на OpenStack: каждый диск — отдельным образом Glance; при заданном flavor — запуск ВМ
/// из корневого образа с указанными сетями. Образ собирается во временный файл и загружается потоком.
async fn openstack_deploy(ctx: &RestoreCtx, images: &[ImageEntry], machine: &Value) -> Result<Value> {
    let log = &ctx.log;
    let os = crate::plugins::vm::openstack::OpenStack::login(&ctx.params, log).await?;
    let format = cfg_str(&ctx.params, "format").unwrap_or_else(|| "qcow2".into());
    let gfmt = match format.as_str() { "raw" => "raw", "vmdk" => "vmdk", "vhd" => "vpc", _ => "qcow2" };
    let wd = crate::plugins::appkit::WorkDir::new(None, None)?;
    let name = cfg_str(&ctx.params, "name").unwrap_or_else(|| machine["hostname"].as_str().unwrap_or("hopebackup-vm").to_string());
    let mut image_ids = Vec::new();
    for img in images {
        let path = wd.join(&format!("{}.{}", img.name, ext(&format)));
        log(&format!("сборка {} → {} ({format})", img.name, path.display()));
        let n = build(&ctx.repo, img, &path, &format).await?;
        let id = os.upload_image_file(&format!("{name}-{}", img.name), gfmt, &path, n, log).await?;
        let _ = std::fs::remove_file(&path);
        image_ids.push(json!({ "disk": img.name, "image": id }));
    }
    let mut server = Value::Null;
    if let Some(flavor) = cfg_str(&ctx.params, "flavor") {
        let root = image_ids.first().and_then(|x| x["image"].as_str()).ok_or_else(|| anyhow!("нет образа для запуска"))?;
        let nets: Vec<String> = ctx.params["networks"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        server = json!({ "id": os.boot_from_image(&name, root, &flavor, &nets, log).await? });
    } else {
        log("flavor не задан — образы загружены в Glance, ВМ не запускалась (запустите из образа вручную или задайте flavor)");
    }
    Ok(json!({ "mode": "deploy", "platform": "openstack", "images": image_ids, "server": server }))
}

// ---------------------------------------------------------------- продукт «Образы qcow2»

/// Свободно байт в файловой системе каталога.
pub fn free_bytes(dir: &str) -> Option<u64> {
    let c = std::ffi::CString::new(dir).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 { return None; }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

/// Оценка места под образы по копии: данные каждого диска (ненулевые блоки) и итог.
pub struct Estimate { pub disks: Vec<(String, u64, u64)>, pub total: u64, pub largest: u64 }

impl Estimate {
    /// Сколько нужно свободного места на время сборки: файлы образов плюс, для форматов кроме raw,
    /// временный разрежённый raw самого большого диска (он лежит рядом, пока qemu-img переводит формат),
    /// и запас 5%.
    pub fn need(&self, format: &str) -> u64 {
        let tmp = if format == "raw" { 0 } else { self.largest };
        (self.total + tmp) + (self.total + tmp) / 20
    }
}

/// Оценка размера готовых образов (по числу ненулевых блоков копии).
pub async fn estimate(repo: &std::sync::Arc<crate::store::repo::Repo>, images: &[ImageEntry]) -> Result<Estimate> {
    let mut e = Estimate { disks: Vec::new(), total: 0, largest: 0 };
    for img in images {
        let ids = repo.read_image_map(img).await?;
        let used = ids.iter().filter(|id| **id != ZERO_ID).count() as u64 * img.block as u64;
        e.total += used;
        e.largest = e.largest.max(used);
        e.disks.push((img.name.clone(), img.size, used));
    }
    Ok(e)
}

/// Подкаталог набора образов: время копии вида 20261104T063129Z. Только такие каталоги продукт создаёт —
/// и только их удаляет при хранении последних N (остальное в каталоге не трогается).
pub fn is_set_name(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() == 16 && b[8] == b'T' && b[15] == b'Z' && b[..8].iter().chain(&b[9..15]).all(|c| c.is_ascii_digit())
}

pub struct Qcow2Product;

pub fn qcow2_schema() -> Value {
    json!({ "type": "object", "properties": {
        "source": { "type": "string", "title": "Источник — экземпляр «Образ машины по SSH»", "description": "Из какого продукта брать последнюю копию", "x-widget": "instance:machine" },
        "dir": { "type": "string", "title": "Локальная папка на сервере HopeBackup", "x-widget": "server-dir",
                 "description": "Папка на подключённом к серверу управления диске (например, /mnt/data/hopebackup-images): сюда сервер собирает образы из копии и сам конвертирует их в qcow2. Образы НЕ шифруются; файлы разрежённые — занимают место по данным дисков; на время сборки нужно ещё место под временный raw самого большого диска. Продукт создаёт и удаляет только свои подкаталоги наборов (ГГГГММДДTччммссZ) — остальное в папке не трогается." },
        "format": { "type": "string", "enum": ["qcow2", "vmdk", "vhd", "raw"], "title": "Формат", "default": "qcow2" },
        "keep": { "type": "integer", "title": "Хранить наборов в локальной папке", "default": 3, "description": "Сколько последних наборов образов держать на сервере" }
    } })
}
pub fn qcow2_defaults() -> Value { json!({ "format": "qcow2", "keep": 3 }) }

/// Локальная папка образов: абсолютный путь на подключённом диске. Папку на системном диске («/») продукт
/// не создаёт сам — образы могут занять его целиком; существующую — использует с предупреждением.
fn check_dir(dir: &str, log: &LogFn) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let p = std::path::Path::new(dir);
    if !p.is_absolute() { bail!("локальная папка образов {dir}: нужен полный путь (например, /mnt/data/hopebackup-images)"); }
    let root_dev = std::fs::metadata("/").map(|m| m.dev()).unwrap_or(0);
    if p.is_dir() {
        if std::fs::metadata(p).map(|m| m.dev()).unwrap_or(0) == root_dev {
            log(&format!("внимание: папка {dir} на системном диске сервера — образы могут занять его целиком; лучше подключить отдельный диск (например, /mnt/data)"));
        }
        return Ok(());
    }
    let parent = p.ancestors().skip(1).find(|a| a.is_dir()).unwrap_or(std::path::Path::new("/"));
    if std::fs::metadata(parent).map(|m| m.dev()).unwrap_or(0) == root_dev {
        bail!("папки {dir} нет, а {} — на системном диске сервера: подключите диск для образов (например, смонтируйте его в /mnt/data) или создайте папку вручную", parent.display());
    }
    std::fs::create_dir_all(p).with_context(|| format!("создание папки {dir}"))?;
    log(&format!("создана папка {dir}"));
    Ok(())
}

#[async_trait]
impl Plugin for Qcow2Product {
    fn id(&self) -> &'static str { "qcow2images" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let dir = cfg_str(cfg, "dir").unwrap_or_default();
        let free = if dir.is_empty() { None } else { free_bytes(&dir) };
        let writable = !dir.is_empty() && std::fs::create_dir_all(&dir).is_ok();
        Ok(json!({ "dir": dir, "free": free, "writable": writable }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let source = cfg_str(&ctx.config, "source").ok_or_else(|| anyhow!("не выбран источник (экземпляр «Образ машины по SSH»)"))?;
        let dir = cfg_str(&ctx.config, "dir").ok_or_else(|| anyhow!("не задан каталог для образов (dir)"))?;
        let format = cfg_str(&ctx.config, "format").unwrap_or_else(|| "qcow2".into());
        let keep = ctx.config.get("keep").and_then(|x| x.as_u64()).unwrap_or(3).max(1) as usize;

        let snaps = crate::plugins::appkit::snapshots_of(&ctx.repo, &source).await?;
        let last = snaps.last().ok_or_else(|| anyhow!("у источника {source} ещё нет копий образа"))?;
        let (_, m) = ctx.repo.manifest(&last.id).await?;
        if m.images.is_empty() { bail!("в последней копии источника нет образов дисков"); }

        if !["qcow2", "vmdk", "vhd", "raw"].contains(&format.as_str()) { bail!("формат {format} не поддерживается (qcow2, vmdk, vhd, raw)"); }
        check_dir(&dir, log)?;
        // Набор образов — в подкаталог по времени копии (чтобы хранить последние N наборов).
        let set = last.time.format("%Y%m%dT%H%M%SZ").to_string();
        let setdir = std::path::Path::new(&dir).join(&set);
        let path_of = |img: &ImageEntry| setdir.join(format!("{}.{}", img.name, ext(&format)));
        let mut files = Vec::new();
        let mut bytes = 0u64;
        if m.images.iter().all(|img| std::fs::metadata(path_of(img)).map(|x| x.len() > 0).unwrap_or(false)) {
            // Новой копии источника не было: набор этой копии уже собран.
            log(&format!("набор {set} уже собран — новой копии источника не было, пересборка не нужна"));
            for img in &m.images { files.push(json!({ "disk": img.name, "file": path_of(img).to_string_lossy(), "size": img.size })); }
        } else {
            // ДО создания: сколько займут образы и сколько свободно; при нехватке — не начинать.
            let est = estimate(&ctx.repo, &m.images).await?;
            let need = est.need(&format);
            let free = free_bytes(&dir).unwrap_or(u64::MAX);
            for (n, size, used) in &est.disks { log(&format!("  {n}: диск {}, данные ~{}", crate::util::human_bytes(*size), crate::util::human_bytes(*used))); }
            log(&format!("образы займут примерно {}{}; нужно свободного места ~{}, свободно в {dir} — {}", crate::util::human_bytes(est.total),
                if format == "raw" { String::new() } else { format!(" (на время сборки ещё ~{} под временный raw)", crate::util::human_bytes(est.largest)) },
                crate::util::human_bytes(need), crate::util::human_bytes(free)));
            if free < need { bail!("в каталоге {dir} недостаточно места: нужно ~{}, свободно {} — образы не создавались", crate::util::human_bytes(need), crate::util::human_bytes(free)); }
            std::fs::create_dir_all(&setdir)?;
            let res = async {
                for img in &m.images {
                    let path = path_of(img);
                    log(&format!("сборка {} → {} ({format})", img.name, path.display()));
                    bytes += build_opt(&ctx.repo, img, &path, &format, true).await?;
                    files.push(json!({ "disk": img.name, "file": path.to_string_lossy(), "size": img.size }));
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            if let Err(e) = res {
                // Недособранный набор — убрать, чтобы не занимал место и не выглядел готовым.
                let _ = std::fs::remove_dir_all(&setdir);
                return Err(e);
            }
        }
        // Хранение последних N наборов: удаляются только каталоги наборов (ГГГГММДДTччммссZ), созданные этим
        // продуктом; прочие файлы и каталоги в {dir} не трогаются.
        let mut sets: Vec<String> = std::fs::read_dir(&dir).map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| is_set_name(n)).collect()).unwrap_or_default();
        sets.sort();
        let removed: Vec<String> = if sets.len() > keep { sets[..sets.len() - keep].to_vec() } else { vec![] };
        for s in &removed { let _ = std::fs::remove_dir_all(std::path::Path::new(&dir).join(s)); log(&format!("удалён старый набор {s}")); }

        // Маркерный снапшот: сам продукт блоки не пишет, только отмечает, что собрал файлы.
        let w = ctx.repo.writer();
        let meta = json!({ "source": source, "set": set, "dir": dir, "format": format, "files": files, "kept": keep });
        let (header, stats) = w.finish(&ctx.instance, "qcow2images", "full", json!({ "set": set, "bytes": bytes }), meta).await?;
        log(&format!("готово: набор {set}, файлов {}, {}", m.images.len(), crate::util::human_bytes(bytes)));
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, _ctx: &RestoreCtx) -> Result<Value> {
        bail!("продукт «Образы qcow2» сам раскладывает файлы в каталог — восстанавливать нечего; используйте продукт «Образ машины по SSH»")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_names_and_need() {
        assert!(is_set_name("20261104T063129Z"));
        for n in ["hopebackup", "hopebackup-server", "2026110T063129Z", "20261104T063129Zx", "20261104X063129Z", "lost+found"] { assert!(!is_set_name(n), "{n}"); }
        let e = Estimate { disks: vec![], total: 100 << 20, largest: 60 << 20 };
        assert_eq!(e.need("raw"), (100 << 20) + (5 << 20));
        assert_eq!(e.need("qcow2"), (160 << 20) + (8 << 20));
    }
}
