//! Отправка готовых образов продукта «Образы qcow2» на удалённые площадки (узлы HopeBackup).
//!
//! Образы собирает и конвертирует сам сервер управления в своей локальной папке; после каждой успешной сборки
//! ставится задание «отправка на площадки». На узел уходит только то, чем новый набор отличается от прошлого,
//! уже лежащего там (см. `plugins::image::filesync`): узел копирует прошлый набор у себя на диске, сервер
//! сравнивает суммы участков по 4 МиБ и передаёт изменившиеся (сжатыми), затем суммы сверяются целиком.
//! Обрыв канала — повтор: принятое на узле сохраняется, передаётся остаток.

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::state::Instance;
use super::Server;
use crate::plugins::image::filesync::{self, CHUNK};
use crate::plugins::image::formats::is_set_name;
use crate::plugins::LogFn;

const EXTS: &[&str] = &[".qcow2", ".vmdk", ".vhd", ".img"];
/// Сколько участков передаётся одновременно (канал до площадки часто с большой задержкой).
const PARALLEL: usize = 4;
const ATTEMPTS: u32 = 3;

/// Последний собранный набор в локальной папке: (имя набора, файлы).
fn latest_set(dir: &Path) -> Result<(String, Vec<PathBuf>)> {
    let mut sets: Vec<String> = std::fs::read_dir(dir).with_context(|| format!("локальная папка образов {}", dir.display()))?
        .flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| is_set_name(n)).collect();
    sets.sort();
    let set = sets.pop().ok_or_else(|| anyhow!("в {} нет собранных наборов", dir.display()))?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join(&set))?.flatten().map(|e| e.path())
        .filter(|p| p.is_file() && EXTS.iter().any(|e| p.to_string_lossy().ends_with(e))).collect();
    files.sort();
    if files.is_empty() { bail!("в наборе {set} нет файлов образов"); }
    Ok((set, files))
}

struct Node { c: reqwest::Client, base: String, token: String }

impl Node {
    fn req(&self, m: reqwest::Method, p: &str) -> reqwest::RequestBuilder { self.c.request(m, format!("{}/images{p}", self.base)).bearer_auth(&self.token).timeout(Duration::from_secs(600)) }
    async fn ok(r: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
        let resp = r.send().await.with_context(|| format!("{what}: нет связи с площадкой"))?;
        let st = resp.status();
        if st.as_u16() == 404 { bail!("{what}: узел площадки не принимает образы — обновите HopeBackup на площадке до 0.9.1 или новее"); }
        if !st.is_success() { bail!("{what}: {} {}", st.as_u16(), resp.text().await.unwrap_or_default().chars().take(300).collect::<String>()); }
        Ok(resp)
    }
    async fn sums(&self, q: &[(&str, &str)]) -> Result<Vec<[u8; 16]>> {
        let b = Self::ok(self.req(reqwest::Method::GET, "/sums").query(q), "суммы участков на площадке").await?.bytes().await?;
        filesync::sums_from(&b)
    }
}

/// Отправить последний собранный набор на площадку `site_id` (хранить там `keep` наборов).
pub async fn run(s: &Arc<Server>, inst: &Instance, site_id: &str, keep: usize, log: &LogFn) -> Result<Value> {
    let dir = PathBuf::from(crate::plugins::cfg_str(&inst.config, "dir").ok_or_else(|| anyhow!("не задана локальная папка образов"))?);
    let (set, files) = latest_set(&dir)?;
    log(&format!("набор {set}: {}", files.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().to_string()).collect::<Vec<_>>().join(", ")));
    // Суммы своих файлов (чтение только участков с данными).
    let mut local = Vec::new();
    for f in &files {
        let p = f.clone();
        let size = std::fs::metadata(f)?.len();
        let sums = tokio::task::spawn_blocking(move || filesync::sums(&p)).await??;
        local.push((f.clone(), size, sums));
    }
    let mut last = None;
    for attempt in 1..=ATTEMPTS {
        match ship_site(s, site_id, &inst.id, &set, &local, keep.max(1), log).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                log(&format!("  попытка {attempt} из {ATTEMPTS}: {e:#}"));
                last = Some(e);
                if attempt < ATTEMPTS { tokio::time::sleep(Duration::from_secs(30)).await; }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("не отправлено")))
}

async fn ship_site(s: &Arc<Server>, site_id: &str, inst: &str, set: &str, local: &[(PathBuf, u64, Vec<[u8; 16]>)], keep: usize, log: &LogFn) -> Result<Value> {
    // Токен очистки (admin) нужен, чтобы удалять на площадке наборы сверх «хранить»; без него — только дописывание.
    let (access, admin) = match s.site_access_admin(site_id)? { Some(a) => (a, true), None => (s.site_access(site_id)?, false) };
    if access.remote.is_s3() { bail!("это площадка S3/облачного хранилища — файлы образов передаются только на узлы HopeBackup"); }
    if s.store.read(|st| st.sites.iter().any(|x| x.id == site_id && x.kind == "disk")) { bail!("это каталог на самом сервере управления — образы и так лежат в локальной папке; выберите удалённую площадку"); }
    let remote = s.tunnels.connect(&access, &|m| log(&format!("  {m}"))).await?;
    let fp = if remote.url.starts_with("https://") { Some(remote.fingerprint.as_str()) } else { None };
    let node = Node { c: crate::tlsutil::client(fp)?, base: format!("{}/v1/r/{}", remote.url.trim_end_matches('/'), remote.repo), token: remote.token.clone() };
    let list: Value = Node::ok(node.req(reqwest::Method::GET, ""), "наборы на площадке").await?.json().await?;
    let mine: Vec<&Value> = list["sets"].as_array().into_iter().flatten().filter(|x| x["instance"] == inst).collect();
    let have = |name: &str| mine.iter().any(|x| x["set"] == name && x["complete"] == true);
    if have(set) {
        log(&format!("  набор {set} уже на площадке ({})", list["root"].as_str().unwrap_or("")));
        return Ok(json!({ "set": set, "already": true }));
    }
    // Основа — последний полный набор на площадке: узел скопирует его у себя, по каналу пойдёт только разница.
    let base = mine.iter().filter(|x| x["complete"] == true).filter_map(|x| x["set"].as_str()).filter(|x| *x < set).max().unwrap_or("").to_string();
    log(&format!("  {}", if base.is_empty() { "на площадке наборов ещё нет — первый раз передаётся всё содержимое".to_string() } else { format!("основа — набор {base} на площадке: передаются только изменения") }));
    let (mut sent_raw, mut sent, mut changed, mut total) = (0u64, 0u64, 0u64, 0u64);
    let mut names = Vec::new();
    for (path, size, lsums) in local {
        let file = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let q = [("instance", inst), ("set", set), ("file", file.as_str())];
        let r: Value = Node::ok(node.req(reqwest::Method::POST, "/begin").json(&json!({ "instance": inst, "set": set, "file": file, "size": size, "base": base })), "начало приёма").await?.json().await?;
        let rsums = node.sums(&q).await?;
        let plan = filesync::plan(lsums, &rsums);
        let data_chunks = plan.iter().filter(|(_, z)| !*z).count();
        log(&format!("  {file}: {} участков, отличается {} ({} с данными){}", lsums.len(), plan.len(), data_chunks, if r["resumed"] == true { " — продолжение прерванной передачи" } else if r["from_base"] == true { " — относительно прошлого набора" } else { "" }));
        total += lsums.len() as u64;
        changed += plan.len() as u64;
        let t0 = Instant::now();
        let mut last_log = Instant::now();
        let mut done = 0usize;
        let n = plan.len();
        let mut st = futures::stream::iter(plan.into_iter().map(|(i, zero)| {
            let (node, path, file) = (&node, path.clone(), file.clone());
            async move {
                let off = i * CHUNK;
                let len = CHUNK.min(size - off);
                if zero {
                    Node::ok(node.req(reqwest::Method::POST, "/zero").json(&json!({ "instance": inst, "set": set, "file": file, "offset": off, "len": len })), "обнуление участка").await?;
                    return Ok::<(u64, u64), anyhow::Error>((0, 0));
                }
                let body = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    use std::os::unix::fs::FileExt;
                    let f = std::fs::File::open(&path)?;
                    let mut b = vec![0u8; len as usize];
                    f.read_exact_at(&mut b, off)?;
                    Ok(zstd::bulk::compress(&b, 3)?)
                }).await??;
                let blen = body.len() as u64;
                let qs = [("instance", inst.to_string()), ("set", set.to_string()), ("file", file), ("offset", off.to_string()), ("raw", len.to_string())];
                Node::ok(node.req(reqwest::Method::PUT, "/chunk").query(&qs).body(body), "передача участка").await?;
                Ok((len, blen))
            }
        })).buffer_unordered(PARALLEL);
        while let Some(r) = st.next().await {
            let (raw, comp) = r?;
            sent_raw += raw;
            sent += comp;
            done += 1;
            if last_log.elapsed() > Duration::from_secs(30) {
                let secs = t0.elapsed().as_secs_f64().max(1.0);
                log(&format!("  {file}: {done} из {n} участков, по каналу {} ({}/с)", crate::util::human_bytes(sent), crate::util::human_bytes((sent as f64 / secs) as u64)));
                last_log = Instant::now();
            }
        }
        drop(st);
        // Сверка: суммы файла на площадке должны совпасть с нашими целиком.
        let check = node.sums(&q).await?;
        if check != *lsums {
            let bad = lsums.iter().zip(check.iter()).filter(|(a, b)| a != b).count() + lsums.len().abs_diff(check.len());
            bail!("{file}: после передачи на площадке не совпадает участков — {bad}");
        }
        log(&format!("  {file}: сверено, совпадает"));
        names.push(file);
    }
    let fin: Value = Node::ok(node.req(reqwest::Method::POST, "/finish").json(&json!({ "instance": inst, "set": set, "files": names, "keep": keep })), "завершение набора").await?.json().await?;
    let removed: Vec<String> = fin["removed"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    if !removed.is_empty() { log(&format!("  на площадке удалены старые наборы: {}", removed.join(", "))); }
    if !admin { log("  старые наборы на площадке не удаляются: у площадки не задан токен очистки (admin)"); }
    s.note_site_disk(site_id, &remote).await;
    log(&format!("  набор {set} принят: изменилось {changed} из {total} участков, данных {}, по каналу {}", crate::util::human_bytes(sent_raw), crate::util::human_bytes(sent)));
    Ok(json!({ "set": set, "base": base, "chunks": total, "changed": changed, "data": sent_raw, "sent": sent, "removed": removed }))
}

/// Подключённые диски сервера (для выбора локальной папки образов): точка, ФС, всего, свободно.
pub fn mounts() -> Vec<Value> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let real = ["ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "nfs", "nfs4", "cifs", "smb3", "fuseblk", "f2fs", "jfs", "reiserfs"];
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 4 || !real.contains(&f[2]) { continue; }
        let mnt = f[1].replace("\\040", " ");
        if mnt.starts_with("/boot") || mnt.starts_with("/snap/") || mnt.starts_with("/var/lib/docker") || mnt.starts_with("/run") { continue; }
        if !seen.insert(f[0].to_string()) { continue; }
        let (total, used) = crate::plugins::host::inventory::statvfs(&mnt);
        if total == 0 { continue; }
        out.push(json!({ "mount": mnt, "fstype": f[2], "device": f[0], "total": total, "free": total.saturating_sub(used), "system": mnt == "/", "readonly": f[3].split(',').any(|o| o == "ro") }));
    }
    out
}
