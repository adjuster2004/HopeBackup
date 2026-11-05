//! Репликация копий одного экземпляра на выбранные площадки (закладка «Репликация» карточки продукта).
//!
//! Зашифрованные копии: на площадку уходят снапшоты этого экземпляра и блоки, на которые они ссылаются, —
//! как есть (сжатыми и зашифрованными), без пароля репозитория. Блоки, которые на площадке уже есть, не
//! передаются (дедупликация). На площадке — тот же репозиторий (тот же ключ): если его нет, он создаётся копией
//! конфигурации; если там другой репозиторий — объясняем, что делать. Сегменты WAL экземпляра — тоже.
//! После — очистка на площадке по «хранить» (только этого экземпляра и только с токеном очистки).
//!
//! «Образы qcow2» вместо блоков отправляют готовые файлы — см. `imageship`.
//!
//! Когда: сразу после каждого успешного задания или по расписанию (cron); вручную — «Реплицировать сейчас».

use anyhow::{anyhow, bail, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::state::{Instance, ReplTarget};
use super::Server;
use crate::plugins::LogFn;
use crate::store::types::{ChunkId, IndexEntry};
use crate::store::Backend;

/// Вид задания репликации экземпляра.
pub fn job_kind(inst: &Instance) -> &'static str { if inst.plugin == "qcow2images" { "image_ship" } else { "inst_repl" } }

/// Включённые площадки репликации экземпляра.
pub fn targets(inst: &Instance) -> Vec<ReplTarget> { inst.replicate.iter().filter(|t| t.enabled && !t.site_id.is_empty()).cloned().collect() }

/// Поставить репликацию экземпляра на площадку (если такая ещё не идёт). Возвращает задание.
pub fn enqueue(s: &Arc<Server>, inst: &Instance, site_id: &str, user: &str) -> Result<String> {
    let kind = job_kind(inst);
    if s.jobs.list(Some(&inst.id), 100).iter().any(|j| j.kind == kind && j.mode == site_id && (j.status == "queued" || j.status == "running")) {
        bail!("репликация на эту площадку уже идёт");
    }
    let site_name = s.store.read(|st| st.sites.iter().find(|x| x.id == site_id).map(|x| x.name.clone())).ok_or_else(|| anyhow!("площадка не найдена"))?;
    let spec = crate::proto::JobSpec { id: String::new(), kind: kind.into(), instance: Some(s.runtime(inst)), site: None, snapshot_id: None, mode: site_id.into(), params: Value::Null };
    let what = if kind == "image_ship" { "отправка qcow2" } else { "репликация" };
    let jid = s.jobs.enqueue(spec, format!("{} — {what} на «{site_name}»", inst.name), "server", user);
    let (s2, id2, inst2, site2) = (s.clone(), jid.clone(), inst.clone(), site_id.to_string());
    tokio::spawn(async move {
        if s2.jobs.start_local(&id2).is_none() { return; }
        let (s3, j3) = (s2.clone(), id2.clone());
        let log: LogFn = Arc::new(move |m: &str| s3.jobs.log(&j3, vec![m.to_string()]));
        let keep = inst2.replicate.iter().find(|t| t.site_id == site2).map(|t| t.keep).filter(|k| *k > 0);
        let r = if inst2.plugin == "qcow2images" {
            // Готовые файлы можно класть и на площадку источника (рядом с репозиторием, в каталог images).
            super::imageship::run(&s2, &inst2, &site2, keep.unwrap_or(2), &log).await
        } else {
            run(&s2, &inst2, &site2, keep.unwrap_or(inst2.retention.keep_last.max(1)), &log).await
        };
        match r {
            Ok(v) => { s2.jobs.finish(&id2, true, v, None); }
            Err(e) => {
                log(&format!("ошибка: {e:#}"));
                s2.jobs.finish(&id2, false, Value::Null, Some(format!("{e:#}")));
                super::notify::emit("job_failed", format!("Репликация не выполнена: {} → {site_name}", inst2.name),
                    format!("Копии экземпляра «{}» не удалось передать на площадку «{site_name}».\n{e:#}\n\nПовтор — при следующем задании или по расписанию репликации; вручную — «Реплицировать сейчас» на закладке «Репликация».", inst2.name));
            }
        }
    });
    Ok(jid)
}

/// После успешного задания экземпляра — репликация на площадки с «сразу после выполнения задания».
pub fn after_job(s: &Arc<Server>, inst: &Instance) {
    for t in targets(inst).into_iter().filter(|t| t.when != "cron") {
        if let Err(e) = enqueue(s, inst, &t.site_id, "после задания") { tracing::info!("репликация {} → {}: {e:#}", inst.name, t.site_id); }
    }
}

/// Плановые запуски по cron (вызывается планировщиком раз в минуту).
pub fn on_minute(s: &Arc<Server>, now: &chrono::DateTime<chrono::Local>, fired: &mut HashMap<String, String>, minute: &str) {
    let insts = s.store.read(|st| st.instances.clone());
    for i in insts {
        for t in targets(&i).into_iter().filter(|t| t.when == "cron" && !t.cron.trim().is_empty()) {
            let Ok(c) = croner::Cron::new(t.cron.trim()).parse() else { continue };
            if !c.is_time_matching(now).unwrap_or(false) { continue; }
            let key = format!("repl|{}|{}", i.id, t.site_id);
            if fired.get(&key).map(|m| m.as_str()) == Some(minute) { continue; }
            fired.insert(key, minute.to_string());
            if let Err(e) = enqueue(s, &i, &t.site_id, "расписание репликации") { tracing::info!("репликация {} → {}: {e:#}", i.name, t.site_id); }
        }
    }
}

/// Передать на площадку снапшоты экземпляра, которых там нет (вместе с недостающими блоками и WAL).
pub async fn run(s: &Arc<Server>, inst: &Instance, site_id: &str, keep: usize, log: &LogFn) -> Result<Value> {
    if site_id == inst.site_id { bail!("это площадка самого экземпляра — выберите другую"); }
    let mut last = None;
    for attempt in 1..=3u32 {
        match pass(s, inst, site_id, keep, log).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let lost = e.downcast_ref::<super::replication::LinkLost>().is_some();
                log(&format!("попытка {attempt} из 3: {e:#}"));
                if !lost && attempt > 1 { return Err(e); }
                last = Some(e);
                if attempt < 3 { tokio::time::sleep(Duration::from_secs(30)).await; }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("репликация не выполнена")))
}

async fn pass(s: &Arc<Server>, inst: &Instance, site_id: &str, keep: usize, log: &LogFn) -> Result<Value> {
    let src_a = s.site_access(&inst.site_id)?;
    let (dst_a, admin) = match s.site_access_admin(site_id)? { Some(a) => (a, true), None => (s.site_access(site_id)?, false) };
    let dst_name = dst_a.name.clone();
    let src_r = s.tunnels.connect(&src_a, &|m| log(&format!("  {m}"))).await?;
    let dst_r = s.tunnels.connect(&dst_a, &|m| log(&format!("  {m}"))).await?;
    let src = crate::store::backend_for(&src_r)?;
    let dst = crate::store::backend_for(&dst_r)?;
    let r = copy(src.clone(), dst.clone(), &inst.id, log).await;
    let mut out = match r { Ok(v) => v, Err(e) => { dst.release().await; return Err(e); } };
    // Очистка на площадке: только снапшоты этого экземпляра сверх «хранить».
    if admin {
        let pol = crate::store::types::PrunePolicy { keep_last: keep.max(1), keep_days: None, dry_run: false, per_instance: HashMap::from([(inst.id.clone(), keep.max(1))]), keep_unknown: true };
        match dst.prune(&pol).await {
            Ok(rep) => { if !rep.snapshots_removed.is_empty() { log(&format!("на площадке «{dst_name}» удалены старые копии: {}", rep.snapshots_removed.len())); } out["pruned"] = json!(rep.snapshots_removed.len()); }
            Err(e) => log(&format!("очистка на площадке «{dst_name}»: {e:#}")),
        }
    } else {
        log(&format!("у площадки «{dst_name}» нет токена очистки — старые копии там не удаляются"));
    }
    dst.release().await;
    s.note_site_disk(site_id, &dst_r).await;
    Ok(out)
}

/// Копирование снапшотов экземпляра `inst` с `src` на `dst`.
pub async fn copy(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, inst: &str, log: &LogFn) -> Result<Value> {
    let scfg = src.read_config().await?.ok_or_else(|| anyhow!("на площадке экземпляра нет репозитория"))?;
    let mut created = false;
    match dst.read_config().await? {
        None => { dst.write_config(&scfg).await?; created = true; log(&format!("на площадке создан репозиторий {} (тот же ключ и пароль, что у исходного)", scfg.id)); }
        Some(d) if d.id != scfg.id => bail!("на площадке под этим именем другой репозиторий ({}, а у экземпляра {}): копии с другим ключом туда не переносятся. Укажите у площадки другое имя репозитория (например, как у площадки экземпляра) — репликация создаст его сама", d.id, scfg.id),
        Some(_) => {}
    }
    let have: std::collections::HashSet<String> = dst.list_snapshots(Some(inst)).await?.into_iter().map(|h| h.id).collect();
    let mut todo: Vec<_> = src.list_snapshots(Some(inst)).await?.into_iter().filter(|h| !have.contains(&h.id)).collect();
    todo.sort_by(|a, b| a.time.cmp(&b.time));
    if todo.is_empty() { log("все копии экземпляра уже на площадке"); }
    else { log(&format!("копий к передаче: {}", todo.len())); }
    let (mut chunks, mut bytes, mut snaps) = (0u64, 0u64, 0u64);
    for h in &todo {
        let rec = src.get_snapshot(&h.id).await?;
        // Каких блоков снапшота на площадке нет (пачками).
        let mut need: Vec<ChunkId> = Vec::new();
        for part in rec.refs.chunks(4096) { need.extend(dst.missing(part.to_vec()).await?); }
        let (c, b) = copy_blobs(src.clone(), dst.clone(), need, log).await?;
        chunks += c; bytes += b;
        dst.put_snapshot(&rec).await?;
        snaps += 1;
        log(&format!("  копия {} ({}): новых блоков {c}, передано {}", h.id, h.time.format("%d.%m.%Y %H:%M"), crate::util::human_bytes(b)));
    }
    // Сегменты WAL экземпляра (PostgreSQL) — для восстановления на момент времени на площадке.
    let ns = format!("wal/{inst}");
    let have: std::collections::HashSet<String> = dst.list_objects(&ns, "").await.unwrap_or_default().into_iter().map(|o| o.name).collect();
    let mut objs = 0u64;
    for o in src.list_objects(&ns, "").await.unwrap_or_default().into_iter().filter(|o| !have.contains(&o.name)) {
        if let Some(b) = src.get_object(&ns, &o.name).await? { bytes += b.len() as u64; dst.put_object(&ns, &o.name, b).await?; objs += 1; }
    }
    if objs > 0 { log(&format!("  сегментов WAL: {objs}")); }
    log(&format!("готово: копий {snaps}, блоков {chunks}, передано {}", crate::util::human_bytes(bytes)));
    Ok(json!({ "snapshots": snaps, "chunks": chunks, "bytes": bytes, "wal": objs, "created_repo": created }))
}

/// Перенести блоки (зашифрованными, как лежат) новыми pack-файлами по ~16 МиБ.
///
/// Блоки читаются пачками (`get_blobs`: у узла 0.9.5+ — один запрос на пачку, у старого — по одному), до трёх пачек
/// одновременно; собранный pack-файл отправляется в фоне (до двух одновременно), пока читаются следующие блоки.
/// Обрыв связи (ошибка сети после повторов) — `LinkLost`: вызывающий поднимает канал заново и продолжает.
pub(crate) async fn copy_blobs(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, ids: Vec<ChunkId>, log: &LogFn) -> Result<(u64, u64)> {
    if ids.is_empty() { return Ok((0, 0)); }
    let total = ids.len();
    let lost = |e: anyhow::Error| -> anyhow::Error {
        if e.chain().any(|c| c.downcast_ref::<reqwest::Error>().is_some()) { anyhow::Error::new(super::replication::LinkLost(1)).context(format!("{e:#}")) } else { e }
    };
    let batches: Vec<Vec<ChunkId>> = ids.chunks(64).map(|c| c.to_vec()).collect();
    let mut st = futures::stream::iter(batches).map(|b| {
        let src = src.clone();
        async move {
            // Узел может вернуть часть пачки (ответ ограничен по размеру) — остальное запросить снова.
            let mut got: Vec<Vec<u8>> = Vec::with_capacity(b.len());
            while got.len() < b.len() {
                let r = src.get_blobs(&b[got.len()..]).await?;
                if r.is_empty() { bail!("площадка не вернула блоки"); }
                got.extend(r);
            }
            Ok::<_, anyhow::Error>(b.into_iter().zip(got).collect::<Vec<_>>())
        }
    }).buffered(3);
    let (mut pack, mut entries): (Vec<u8>, Vec<IndexEntry>) = (Vec::new(), Vec::new());
    let (mut n, mut bytes) = (0u64, 0u64);
    let mut last = std::time::Instant::now();
    let mut uploads: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
    async fn wait_one(u: &mut tokio::task::JoinSet<Result<()>>) -> Result<()> {
        match u.join_next().await { Some(r) => r.map_err(|e| anyhow!("отправка pack-файла: {e}"))?, None => Ok(()) }
    }
    let flush = |uploads: &mut tokio::task::JoinSet<Result<()>>, pack: &mut Vec<u8>, entries: &mut Vec<IndexEntry>| {
        if entries.is_empty() { return; }
        let data = std::mem::take(pack);
        let e = std::mem::take(entries);
        let dst = dst.clone();
        uploads.spawn(async move {
            let id = blake3::hash(&data).to_hex().to_string();
            dst.put_pack(&id, bytes::Bytes::from(data), e).await
        });
    };
    while let Some(r) = st.next().await {
        for (id, blob) in r.map_err(lost)? {
            entries.push(IndexEntry { id, offset: pack.len() as u32, len: blob.len() as u32 });
            bytes += blob.len() as u64;
            pack.extend_from_slice(&blob);
            n += 1;
            if pack.len() >= crate::store::repo::PACK_TARGET {
                while uploads.len() >= 2 { wait_one(&mut uploads).await.map_err(lost)?; }
                flush(&mut uploads, &mut pack, &mut entries);
            }
        }
        if last.elapsed() > Duration::from_secs(30) { log(&format!("    блоков {n} из {total}, {}", crate::util::human_bytes(bytes))); last = std::time::Instant::now(); }
    }
    flush(&mut uploads, &mut pack, &mut entries);
    while !uploads.is_empty() { wait_one(&mut uploads).await.map_err(lost)?; }
    Ok((n, bytes))
}

/// Состояние репликации экземпляра для закладки: по последнему заданию на каждую площадку.
pub fn status(s: &Server, inst: &Instance) -> Value {
    let kind = job_kind(inst);
    let jobs = s.jobs.list(Some(&inst.id), 400);
    let st = s.store.get();
    let site_level: Vec<Value> = st.sites.iter().find(|x| x.id == inst.site_id).map(|x| x.replicas.iter().filter_map(|r| st.sites.iter().find(|y| &y.id == r)).map(|y| json!({ "id": y.id, "name": y.name })).collect()).unwrap_or_default();
    let mut per = serde_json::Map::new();
    for j in jobs.iter().filter(|j| j.kind == kind) {
        if per.contains_key(&j.mode) { continue; }
        per.insert(j.mode.clone(), json!({ "job_id": j.id, "status": j.status, "started": j.started, "finished": j.finished, "error": j.error, "result": j.result }));
    }
    json!({ "kind": kind, "targets": inst.replicate, "last": per, "site_level": site_level, "own_site": inst.site_id, "repl_set": inst.repl_set, "site_repl": inst.site_repl, "site_repl_excluded": inst.site_repl_excluded(), "keep_default": if kind == "image_ship" { 2 } else { inst.retention.keep_last.max(1) } })
}
