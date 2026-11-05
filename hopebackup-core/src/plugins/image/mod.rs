//! Продукт «Образ машины по SSH»: сервер управления (или любой агент) подключается к машине по SSH,
//! запускает на ней программу `hopebackup disk-read`, которая читает диски блоками по 4 МиБ через снимки
//! LVM (для согласованности томов), и складывает блоки в репозиторий с дедупликацией, шифрованием и
//! репликацией — как обычный продукт. В каждую следующую копию уходят только изменившиеся блоки.
//!
//! Агент на машине не нужен: достаточно доступа SSH из раздела «Серверы и ВМ» и прав root (для снимков
//! LVM и чтения дисков). Программа копируется на машину во временный каталог (как опись сервера),
//! запускается, а после — удаляется.

pub mod filesync;
pub mod formats;
pub mod frames;
pub mod layout;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::sshc;
use crate::store::repo::Block;

/// Размер блока образа по умолчанию, МиБ.
pub const DEFAULT_BLOCK_MIB: u32 = 4;

pub struct Machine;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "machine_id": { "type": "string", "title": "Машина", "description": "Из раздела «Серверы и ВМ» (доступ SSH берётся оттуда)", "x-widget": "machine" },
        "disks": { "type": "array", "items": { "type": "string" }, "title": "Диски", "description": "Блочные устройства (/dev/sda); пусто — все диски машины" },
        "lvm": { "type": "string", "enum": ["auto", "required", "off"], "title": "Снимки томов LVM", "default": "auto",
                 "description": "auto — где можно, снимок LVM, иначе на ходу с предупреждением; required — только снимком (иначе ошибка); off — всё на ходу" },
        "freeze_small": { "type": "boolean", "title": "Замораживать небольшие ФС без LVM", "default": true, "description": "fsfreeze на разделах с ФС до 4 ГиБ на время чтения — данные в одном состоянии" },
        "freeze_timeout": { "type": "integer", "title": "Предел заморозки, с", "default": 600 },
        "block_mib": { "type": "integer", "title": "Размер блока, МиБ", "default": 4 }
    } })
}

pub fn defaults() -> Value { json!({ "lvm": "auto", "freeze_small": true, "freeze_timeout": 600, "block_mib": 4 }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "mode": { "type": "string", "enum": ["deploy", "export"], "title": "Что сделать", "default": "deploy",
                  "description": "deploy — развернуть ВМ на платформе; export — выгрузить файлы образа в каталог" },
        "format": { "type": "string", "enum": ["qcow2", "vmdk", "vhd", "raw"], "title": "Формат файла (выгрузка)", "default": "qcow2" },
        "dir": { "type": "string", "title": "Каталог выгрузки", "description": "Куда положить файлы образа (на этой машине/агенте)" },
        "platform": { "type": "string", "enum": ["proxmox", "libvirt", "ovirt", "openstack"], "title": "Платформа (развернуть ВМ)" },
        "storage": { "type": "string", "title": "Хранилище / пул / домен / том Cinder" },
        "cluster": { "type": "string", "title": "Кластер (oVirt)" },
        "flavor": { "type": "string", "title": "Flavor (OpenStack)", "description": "Тип инстанса для запуска ВМ; без него образы только загружаются в Glance" },
        "networks": { "type": "object", "title": "Сети", "description": "Интерфейс источника (или «*») → мост/сеть/профиль платформы (для OpenStack — id сети)" },
        "name": { "type": "string", "title": "Имя ВМ", "description": "По умолчанию — имя машины источника" },
        "start": { "type": "boolean", "title": "Запустить ВМ после развёртывания", "default": false }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["platform", "storage", "cluster", "networks", "insecure"] }

// ---------------------------------------------------------------- доступ по SSH

struct Access { host: String, port: u16, auth: sshc::Auth, host_key: String }

fn access(cfg: &Value) -> Result<Access> {
    let host = cfg_str(cfg, "host").ok_or_else(|| anyhow!("не задан доступ к машине (выберите машину в разделе «Серверы и ВМ»)"))?;
    let key = cfg_str(cfg, "auth").as_deref() == Some("key");
    let auth = sshc::Auth {
        user: cfg_str(cfg, "user").unwrap_or_else(|| "root".into()),
        password: if key { String::new() } else { cfg_str(cfg, "password").unwrap_or_default() },
        private_key: if key { cfg_str(cfg, "private_key").unwrap_or_default() } else { String::new() },
        key_passphrase: cfg_str(cfg, "key_passphrase").unwrap_or_default(),
        sudo_password: cfg_str(cfg, "sudo_password").unwrap_or_default(),
    };
    Ok(Access { host, port: cfg.get("port").and_then(|x| x.as_u64()).unwrap_or(22) as u16, auth, host_key: cfg_str(cfg, "host_key").unwrap_or_default() })
}

fn sha256_bytes(b: &[u8]) -> String { use sha2::{Digest, Sha256}; let mut h = Sha256::new(); h.update(b); hex::encode(h.finalize()) }

/// Скопировать программу на машину во временный каталог (как опись сервера): проверка контрольной суммы и
/// запуска. Возвращает путь к каталогу.
async fn stage(c: &sshc::Conn, bin: &[u8], log: &LogFn) -> Result<String> {
    let root = c.sudo_mode() != sshc::Sudo::None;
    let sum = sha256_bytes(bin);
    let bases: &[&str] = if root { &["/var/tmp", "/tmp", "/dev/shm", "/root", "/run"] } else { &["/var/tmp", "$HOME", "/tmp", "/dev/shm"] };
    let mut last = String::from("нет подходящего временного каталога");
    for base in bases {
        let script = format!("umask 077; d=$(mktemp -d {base}/hopebackup.XXXXXX) || exit 3; cat > \"$d/hopebackup\" && chmod 700 \"$d/hopebackup\" && echo \"$d\"");
        log(&format!("копирование программы ({}) в {base}", crate::util::human_bytes(bin.len() as u64)));
        let o = if root { c.exec_root(&script, Some(bin), Duration::from_secs(600)).await? } else { c.exec(&script, Some(bin), Duration::from_secs(600)).await? };
        let dir = o.text().lines().last().unwrap_or("").trim().to_string();
        if o.code != 0 || !dir.starts_with('/') { last = format!("{base}: {}", o.err()); continue; }
        let q = crate::util::shq(&dir);
        let chk = format!("f={q}/hopebackup; (sha256sum \"$f\" 2>/dev/null || openssl dgst -sha256 -r \"$f\" 2>/dev/null) | cut -c1-64; \"$f\" --version >/dev/null 2>&1; echo \"rc=$?\"");
        let o = if root { c.exec_root(&chk, None, Duration::from_secs(60)).await? } else { c.exec(&chk, None, Duration::from_secs(60)).await? };
        let t = o.text();
        let got = t.lines().next().unwrap_or("").trim().to_string();
        let rc = t.lines().find_map(|l| l.strip_prefix("rc=")).unwrap_or("").trim().to_string();
        if got.len() == 64 && got != sum { let _ = rm_dir(c, &dir).await; bail!("программа на машине повреждена или подменена"); }
        if rc == "0" { return Ok(dir); }
        let _ = rm_dir(c, &dir).await;
        last = format!("{base}: программа не запускается (код {rc}{})", if rc == "126" { ", каталог смонтирован с noexec" } else if rc == "127" { ", не та библиотека C" } else { "" });
        if rc != "126" { break; }
    }
    bail!("не удалось разместить программу на машине: {last}")
}

async fn rm_dir(c: &sshc::Conn, dir: &str) {
    let root = c.sudo_mode() != sshc::Sudo::None;
    let cmd = format!("rm -rf {}", crate::util::shq(dir));
    let _ = if root { c.exec_root(&cmd, None, Duration::from_secs(60)).await } else { c.exec(&cmd, None, Duration::from_secs(60)).await };
}

async fn program(c: &sshc::Conn, log: &LogFn) -> Result<Vec<u8>> {
    let o = c.exec(crate::dist::PROBE, None, Duration::from_secs(30)).await?;
    let (arch, libc) = crate::dist::parse_probe(&o.text());
    let (t, bin) = crate::dist::pick(&arch, &libc)?;
    log(&format!("машина: {arch}, {}; программа: {}", crate::dist::label_libc(&libc), crate::dist::label(&t)));
    Ok(bin)
}

fn disk_id(path: &str) -> String { path.trim_start_matches("/dev/").replace('/', "_") }

/// Состояние записи одного диска образа на сервере.
struct Cur {
    name: String,
    st: crate::store::repo::FixedState,
    idx: u64,
    pmap: Vec<[u8; frames::HASH]>,
    prev_ids: Vec<crate::store::types::ChunkId>,
    prev_pmap: Option<frames::PrevMap>,
}

/// Завершить диск: сохранить карту блоков и карту изменений (pmap), отметить число блоков в мете.
async fn finish_disk(w: &mut crate::store::repo::SnapshotWriter, cur: Cur, meta: &mut [Value]) -> Result<()> {
    let img = w.fixed_end(&cur.name, cur.st).await?;
    let mut pm = Vec::with_capacity(cur.pmap.len() * frames::HASH);
    for h in &cur.pmap { pm.extend_from_slice(h); }
    crate::plugins::appkit::add_bytes(w, &format!("{}.pmap", cur.name), pm).await?;
    if let Some(d) = meta.iter_mut().find(|d| d["name"] == img.name.as_str()) { d["blocks"] = json!(img.blocks); }
    Ok(())
}

// ---------------------------------------------------------------- плагин

#[async_trait]
impl Plugin for Machine {
    fn id(&self) -> &'static str { "machine" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let a = access(cfg)?;
        let c = sshc::connect(&a.host, a.port, &a.auth, &a.host_key).await?;
        if c.sudo_mode() == sshc::Sudo::None { let _ = c.close().await; bail!("нужны права root на машине (вход под root или sudo): {}", "образ диска читается от root"); }
        // Диски и разделы (lsblk), подкачка, монтирования — лёгкий обзор без копирования программы.
        let o = c.exec_root("lsblk -b -J -o NAME,TYPE,SIZE,FSTYPE,MOUNTPOINT 2>/dev/null; echo '==='; cat /proc/swaps 2>/dev/null; echo '==='; (command -v pvs >/dev/null && echo yes || echo no)", None, Duration::from_secs(40)).await?;
        let t = o.text();
        let mut parts = t.splitn(3, "===");
        let lsblk: Value = serde_json::from_str(parts.next().unwrap_or("{}").trim()).unwrap_or(json!({}));
        let has_lvm = parts.nth(1).map(|s| s.contains("yes")).unwrap_or(false);
        let want: Vec<String> = crate::connectors::net::p_list(cfg, "disks");
        let mut disks = Vec::new();
        let mut total = 0u64;
        for d in lsblk["blockdevices"].as_array().into_iter().flatten() {
            if d["type"] != "disk" { continue; }
            let name = d["name"].as_str().unwrap_or("");
            let path = format!("/dev/{name}");
            if !want.is_empty() && !want.iter().any(|w| w == &path || w == name) { continue; }
            let size = d["size"].as_u64().unwrap_or(0);
            total += size;
            let lvm = d["children"].as_array().into_iter().flatten().any(|c| c["fstype"].as_str() == Some("LVM2_member"));
            disks.push(json!({ "disk": path, "size": size, "lvm": lvm }));
        }
        let machine = machine_info_remote(&c).await;
        let _ = c.close().await;
        Ok(json!({ "disks": disks, "machine": machine, "estimate": total, "lvm_tools": has_lvm }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let a = access(&ctx.config)?;
        let block = (ctx.config.get("block_mib").and_then(|x| x.as_u64()).unwrap_or(DEFAULT_BLOCK_MIB as u64).clamp(1, 64) as u32) << 20;
        let lvm = cfg_str(&ctx.config, "lvm").unwrap_or_else(|| "auto".into());
        let freeze_small = crate::plugins::appkit::bool_param(&ctx.config, "freeze_small", true);
        let freeze_timeout = ctx.config.get("freeze_timeout").and_then(|x| x.as_u64()).unwrap_or(600);
        let want: Vec<String> = crate::connectors::net::p_list(&ctx.config, "disks");

        let c = sshc::connect(&a.host, a.port, &a.auth, &a.host_key).await?;
        if c.sudo_mode() == sshc::Sudo::None { let _ = c.close().await; bail!("нужны права root на машине (вход под root или sudo)"); }
        log(&format!("машина {}: подготовка образа", a.host));
        let minfo = machine_info_remote(&c).await;
        let bin = program(&c, log).await?;
        let dir = stage(&c, &bin, log).await?;

        // Прошлая копия: карты изменений (plain-хэши) и карты блоков (идентификаторы репозитория для Ref).
        let (prev_id, prev) = super::replica::prev_maps(&ctx.repo, &ctx.instance).await.unwrap_or((None, crate::store::types::Manifest { streams: vec![], images: vec![], meta: Value::Null }));
        let mut prev_pmap: HashMap<String, frames::PrevMap> = HashMap::new();
        let mut prev_ids: HashMap<String, Vec<crate::store::types::ChunkId>> = HashMap::new();
        for img in &prev.images {
            if let Ok(ids) = ctx.repo.read_image_map(img).await { prev_ids.insert(img.name.clone(), ids); }
            if let Ok(s) = crate::plugins::appkit::stream(&prev, &format!("{}.pmap", img.name)) {
                if let Ok(raw) = crate::plugins::appkit::read_small(&ctx.repo, s).await {
                    if raw.len() == img.blocks as usize * frames::HASH {
                        let hashes = raw.chunks_exact(frames::HASH).map(|c| { let mut x = [0u8; frames::HASH]; x.copy_from_slice(c); x }).collect();
                        prev_pmap.insert(img.name.clone(), frames::PrevMap { block: img.block, hashes });
                    }
                }
            }
        }
        // Карты для stdin машины: только по дискам, что будем снимать (имя диска — id вида «sda»).
        let prev_for_stdin: Vec<(String, frames::PrevMap)> = prev_pmap.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut stdin = Vec::new();
        frames::write_prev_maps(&mut stdin, &prev_for_stdin)?;

        let args = format!(
            "{bin} disk-read --block {block} --lvm {lvm}{disks}{fs}{ft}",
            bin = crate::util::shq(&format!("{dir}/hopebackup")),
            disks = if want.is_empty() { String::new() } else { format!(" --disks {}", crate::util::shq(&want.join(","))) },
            fs = if freeze_small { " --freeze-small" } else { "" },
            ft = format!(" --freeze-timeout {freeze_timeout}"),
        );
        let (cmd, pw) = c.root_wrap(&args)?;
        let mut prefix = pw;
        prefix.extend_from_slice(&stdin);
        log(&format!("чтение дисков на машине (блок {} МиБ, LVM {lvm})", block >> 20));
        let (reader, jh) = c.stream_out(&cmd, prefix).await?;

        // Разбор кадров в отдельном потоке → в асинхронную запись.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<frames::Item>(64);
        let parser = std::thread::spawn(move || -> Result<()> {
            let mut fr = frames::Reader::new(reader);
            while let Some(it) = fr.next()? { let end = matches!(it, frames::Item::End); if tx.blocking_send(it).is_err() { break; } if end { break; } }
            Ok(())
        });

        let mut w = ctx.repo.writer();
        let mut disks_meta: Vec<Value> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut cur: Option<Cur> = None;

        while let Some(it) = rx.recv().await {
            match it {
                frames::Item::Disk(h) => {
                    if let Some(c0) = cur.take() { finish_disk(&mut w, c0, &mut disks_meta).await?; }
                    let st = w.begin_fixed(h.size, h.block);
                    let prev_ids = prev_ids.get(&h.name).cloned().unwrap_or_default();
                    let prev_pmap = prev_pmap.get(&h.name).cloned();
                    cur = Some(Cur { name: h.name.clone(), st, idx: 0, pmap: Vec::with_capacity(h.blocks as usize), prev_ids, prev_pmap });
                    disks_meta.push(json!({ "name": h.name, "size": h.size, "block": h.block }));
                }
                frames::Item::Data(bytes) => {
                    let c0 = cur.as_mut().ok_or_else(|| anyhow!("кадр данных без заголовка диска"))?;
                    c0.pmap.push(frames::block_hash(&bytes));
                    w.fixed_push(&mut c0.st, Block::Data(&bytes)).await?;
                    c0.idx += 1;
                }
                frames::Item::Zero(n) => {
                    let c0 = cur.as_mut().ok_or_else(|| anyhow!("кадр нулей без заголовка диска"))?;
                    for _ in 0..n { c0.pmap.push([0u8; frames::HASH]); w.fixed_push(&mut c0.st, Block::Zero).await?; c0.idx += 1; }
                }
                frames::Item::Ref(n) => {
                    let c0 = cur.as_mut().ok_or_else(|| anyhow!("кадр ссылок без заголовка диска"))?;
                    for _ in 0..n {
                        let i = c0.idx as usize;
                        let id = *c0.prev_ids.get(i).ok_or_else(|| anyhow!("ссылка на блок {i} диска {}, которого нет в прошлой копии", c0.name))?;
                        let h = c0.prev_pmap.as_ref().and_then(|p| p.get(c0.idx)).unwrap_or([0u8; frames::HASH]);
                        c0.pmap.push(h);
                        w.fixed_push(&mut c0.st, Block::Ref(id)).await?;
                        c0.idx += 1;
                    }
                }
                frames::Item::Warn(m) => { log(&format!("  ! {m}")); warnings.push(m); }
                frames::Item::Stat(v) => { if let Some(d) = disks_meta.last_mut() { d["stat"] = v; } }
                frames::Item::End => break,
            }
        }
        if let Some(c0) = cur.take() { finish_disk(&mut w, c0, &mut disks_meta).await?; }

        let presult = parser.join().map_err(|_| anyhow!("разбор кадров аварийно завершился"))?;
        let (code, err) = jh.await.unwrap_or(Ok((-1, "нет ответа".into()))).unwrap_or((-1, "ошибка канала".into()));
        rm_dir(&c, &dir).await;
        let _ = c.close().await;
        presult?;
        if code != 0 { bail!("disk-read на машине завершился с кодом {code}: {err}"); }
        if disks_meta.is_empty() { bail!("машина не вернула ни одного диска"); }

        let meta = json!({ "disks": disks_meta, "machine": minfo, "prev": prev_id, "warnings": warnings, "source": "ssh" });
        let (header, stats) = w.finish(&ctx.instance, "machine", "full", json!({ "prev": prev_id, "disks": disks_meta.len() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        formats::deploy_or_export(ctx).await
    }
}

/// Сведения о машине для создания ВМ (CPU, память, сеть с MAC, прошивка) — командами по SSH.
async fn machine_info_remote(c: &sshc::Conn) -> Value {
    let script = "echo CPUS=$(nproc 2>/dev/null || echo 1); echo MEM=$(awk '/MemTotal/{print $2*1024}' /proc/meminfo 2>/dev/null); \
                  echo OS=$( . /etc/os-release 2>/dev/null; echo \"$PRETTY_NAME\"); echo FW=$([ -d /sys/firmware/efi ] && echo uefi || echo bios); echo HOST=$(hostname 2>/dev/null); \
                  for n in /sys/class/net/*; do i=$(basename $n); [ \"$i\" = lo ] && continue; [ -e $n/device ] || continue; echo NIC=$i $(cat $n/address 2>/dev/null); done";
    let o = match c.exec_root(script, None, Duration::from_secs(30)).await { Ok(o) => o, Err(_) => return json!({}) };
    let t = o.text();
    let get = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).map(|s| s.trim().to_string());
    let nics: Vec<Value> = t.lines().filter_map(|l| l.strip_prefix("NIC=")).filter_map(|s| { let (n, m) = s.trim().split_once(' ').unwrap_or((s.trim(), "")); Some(json!({ "name": n, "mac": m.trim() })) }).collect();
    json!({
        "hostname": get("HOST=").unwrap_or_default(),
        "os": get("OS=").unwrap_or_default(),
        "cpus": get("CPUS=").and_then(|x| x.parse::<u64>().ok()).unwrap_or(1),
        "memory": get("MEM=").and_then(|x| x.parse::<u64>().ok()).unwrap_or(0),
        "firmware": get("FW=").unwrap_or_else(|| "bios".into()),
        "nics": nics,
    })
}

// ---------------------------------------------------------------- подкоманда disk-read (на машине)

/// Читает диски на машине и выдаёт кадры в stdout. Запускается сервером по SSH от root.
pub async fn disk_read(block: u32, disks_csv: &str, lvm: &str, freeze_small: bool, freeze_timeout: u64) -> Result<()> {
    if !cfg!(target_os = "linux") { bail!("disk-read — только для Linux"); }
    // Карты прошлой копии со stdin (блокирующее чтение в отдельном потоке).
    let prev = tokio::task::spawn_blocking(|| frames::read_prev_maps(std::io::stdin().lock())).await??;
    let disks: Vec<String> = if disks_csv.is_empty() { local_all_disks()? } else { disks_csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() };
    if disks.is_empty() { bail!("нет дисков для образа"); }
    let mode = layout::LvmMode::parse(lvm);
    // Вся выдача — в отдельном блокирующем потоке (stdout, чтение дисков, снимки LVM).
    tokio::task::spawn_blocking(move || disk_read_blocking(block, &disks, mode, freeze_small, freeze_timeout, &prev)).await?
}

fn local_all_disks() -> Result<Vec<String>> {
    let o = std::process::Command::new("lsblk").args(["-dnbo", "NAME,TYPE,RM,SIZE"]).output().context("lsblk")?;
    Ok(String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 4 && f[1] == "disk" && f[2] == "0" && f[3] != "0").then(|| format!("/dev/{}", f[0]))
    }).collect())
}

fn disk_read_blocking(block: u32, disks: &[String], mode: layout::LvmMode, freeze_small: bool, freeze_timeout: u64, prev: &HashMap<String, frames::PrevMap>) -> Result<()> {
    let out = std::io::BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    let mut w = frames::Writer::new(out)?;
    for disk in disks {
        let size = layout::dev_size(disk)?;
        let id = disk_id(disk);
        let (plan, cleanup) = layout::prepare(disk, size, mode, freeze_small, freeze_timeout, &mut w)?;
        let pm = prev.get(&id).filter(|p| p.block == block && p.hashes.len() as u64 == size.div_ceil(block as u64));
        w.disk(&frames::DiskHdr { name: id.clone(), size, block, blocks: size.div_ceil(block as u64) })?;
        let stat = read_disk(disk, size, block, &plan, pm, &mut w)?;
        w.stat(&stat)?;
        drop(cleanup); // снять снимки LVM и заморозку
    }
    w.end()?;
    Ok(())
}

/// Прочитать один диск по плану и выдать кадры (данные/нули/ссылки). Возвращает статистику.
fn read_disk<W: std::io::Write>(disk: &str, size: u64, block: u32, plan: &layout::Plan, prev: Option<&frames::PrevMap>, w: &mut frames::Writer<W>) -> Result<Value> {
    let mut ov = layout::Overlay::open(disk, plan)?;
    let n = size.div_ceil(block as u64);
    let mut buf = vec![0u8; block as usize];
    let (mut changed, mut zero, mut same) = (0u64, 0u64, 0u64);
    // Пробеги нулей и ссылок копим, сбрасывая противоположный пробег перед сменой вида.
    let (mut zrun, mut rrun) = (0u64, 0u64);
    for i in 0..n {
        let off = i * block as u64;
        let len = (block as u64).min(size - off) as usize;
        ov.read_block(off, &mut buf[..len])?;
        let h = frames::block_hash(&buf[..len]);
        if h == [0u8; frames::HASH] {
            if rrun > 0 { w.ref_run(rrun)?; rrun = 0; }
            zrun += 1; zero += 1;
        } else if prev.and_then(|p| p.get(i)) == Some(h) {
            if zrun > 0 { w.zero_run(zrun)?; zrun = 0; }
            rrun += 1; same += 1;
        } else {
            if zrun > 0 { w.zero_run(zrun)?; zrun = 0; }
            if rrun > 0 { w.ref_run(rrun)?; rrun = 0; }
            w.data(&buf[..len])?;
            changed += 1;
        }
    }
    if zrun > 0 { w.zero_run(zrun)?; }
    if rrun > 0 { w.ref_run(rrun)?; }
    Ok(json!({ "blocks": n, "changed": changed, "zero": zero, "same": same, "warnings": plan.warnings }))
}
