//! Репликация площадок: всё, чего нет на принимающей площадке, копируется с исходной как есть —
//! данные уже сжаты и зашифрованы, ключи не нужны. Работает между любыми видами площадок (узел ↔ S3).
//!
//! Порядок: конфигурация репозитория → pack-файлы с индексом → снапшоты → объекты (WAL). Если продукты площадки
//! исключены из репликации (закладка «Репликация» продукта), pack-файлы целиком не копируются: передаются
//! снапшоты остальных продуктов и только их недостающие блоки — блоки исключённых туда не попадают. Удаления не
//! повторяются: у принимающей площадки своя очистка по тем же политикам хранения — шифровальщик,
//! добравшийся до исходной площадки, не уничтожит копии на принимающей.

use anyhow::{bail, Result};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::Server;
use crate::plugins::LogFn;
use crate::store::Backend;
use crate::store::types::ChunkId;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReplStats {
    pub packs: u64,
    pub pack_bytes: u64,
    pub snapshots: u64,
    pub objects: u64,
    pub object_bytes: u64,
    pub created_repo: bool,
    /// Снапшоты, блоки которых ещё не на принимающей площадке (догонят в следующий проход).
    pub snapshots_deferred: u64,
    /// Блоки, переданные по снапшотам (когда есть исключённые продукты).
    pub blocks: u64,
    /// Снапшоты исключённых продуктов, которые не передавались.
    pub snapshots_excluded: u64,
    /// pack-файлы, все блоки которых уже были на принимающей площадке (не передавались).
    pub packs_skipped: u64,
    /// pack-файлы, из которых переданы только недостающие блоки.
    pub packs_partial: u64,
}

impl ReplStats {
    pub fn empty(&self) -> bool { self.packs == 0 && self.snapshots == 0 && self.objects == 0 && !self.created_repo }
}

/// Площадка перестала отвечать посреди передачи (обычно упал VPN-канал): проход прерывается, чтобы поднять канал
/// заново и продолжить, а не ждать таймаут на каждом из оставшихся файлов.
#[derive(Debug)]
pub struct LinkLost(pub u64);
impl std::fmt::Display for LinkLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "площадка перестала отвечать во время передачи (ошибок подряд: {})", self.0)
    }
}
impl std::error::Error for LinkLost {}

/// Сколько ошибок передачи подряд считать потерей канала (при 3 параллельных pack-файлах — два «круга»).
const LINK_LOST_AFTER: u64 = 6;
/// Сколько раз за проход поднимать канал заново.
const LINK_RETRIES: u32 = 3;

/// pack-файлы исходной площадки, которые не нужно передавать на принимающую (их блоки уже там) — по паре площадок,
/// в памяти сервера (после перезапуска проверяются один раз заново).
pub type Known = parking_lot::Mutex<HashSet<String>>;

/// Скопировать недостающее с `src` на `dst`; снапшоты (и сегменты WAL) экземпляров из `exclude` не передаются.
pub async fn replicate(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, exclude: &HashSet<String>, known: &Known, log: &LogFn) -> Result<ReplStats> {
    let mut st = ReplStats::default();
    let scfg = src.read_config().await?.ok_or_else(|| anyhow::anyhow!("на исходной площадке нет репозитория"))?;
    match dst.read_config().await? {
        None => {
            dst.write_config(&scfg).await?;
            st.created_repo = true;
            log(&format!("на принимающей площадке создан репозиторий {}", scfg.id));
        }
        Some(d) if d.id != scfg.id => {
            // Конфигурацию существующего репозитория узел не перезаписывает (защита ключа) — подсказать выход.
            let empty = dst.list_snapshots(None).await.map(|v| v.is_empty()).unwrap_or(false) && dst.list_packs().await.map(|v| v.is_empty()).unwrap_or(false);
            if empty {
                bail!("на принимающей площадке под этим именем уже создан другой, пустой репозиторий ({}; исходный {}) — вероятно, кнопкой «Создать репозиторий». Укажите у принимающей площадки другое имя репозитория (например, как у исходной) — репликация создаст его сама", d.id, scfg.id)
            }
            bail!("на принимающей площадке под этим именем другой репозиторий с копиями ({}, а исходный {}) — укажите у принимающей площадки другое имя репозитория", d.id, scfg.id)
        }
        Some(_) => {}
    }
    if !exclude.is_empty() { return by_snapshots(src, dst, exclude, st, log).await; }
    // pack-файлы: по 3 параллельно. Перед передачей — индекс pack-файла (без данных) и проверка, каких его блоков
    // на принимающей площадке нет: все есть (их туда уже передала репликация с исключениями или репликация
    // продукта) — файл пропускается и запоминается; часть есть — передаются только недостающие блоки.
    let have: HashSet<String> = dst.list_packs().await?.into_iter().map(|p| p.id).collect();
    let mut todo: Vec<_> = src.list_packs().await?.into_iter().filter(|p| !have.contains(&p.id) && !known.lock().contains(&p.id)).collect();
    todo.sort_by(|a, b| a.id.cmp(&b.id));
    if !todo.is_empty() { log(&format!("pack-файлов к передаче: {} ({})", todo.len(), crate::util::human_bytes(todo.iter().map(|p| p.size).sum()))); }
    enum Done { Whole(u64), Part(u64, u64), Skipped }
    let mut copy = futures::stream::iter(todo).map(|p| {
        let (src, dst) = (src.clone(), dst.clone());
        let log = log.clone();
        async move {
            let r: Result<Done> = async {
                let idx = src.pack_index(&p.id).await?;
                let mut miss: Vec<ChunkId> = Vec::new();
                for part in idx.iter().map(|e| e.id).collect::<Vec<_>>().chunks(4096) { miss.extend(dst.missing(part.to_vec()).await?); }
                if !idx.is_empty() && miss.is_empty() { return Ok(Done::Skipped); }
                if !idx.is_empty() && miss.len() < idx.len() {
                    let (n, b) = super::instrepl::copy_blobs(src.clone(), dst.clone(), miss, &log).await?;
                    return Ok(Done::Part(n, b));
                }
                let (entries, data) = src.get_pack(&p.id).await?;
                let n = data.len() as u64;
                dst.put_pack(&p.id, data, entries).await?;
                Ok(Done::Whole(n))
            }
            .await;
            (p.id, r)
        }
    }).buffer_unordered(3);
    let mut last_log = std::time::Instant::now();
    let mut failed = 0u64;
    let mut streak = 0u64;
    while let Some((id, r)) = copy.next().await {
        // Один неисправный pack-файл не должен останавливать репликацию остального; много подряд — канал потерян.
        match r {
            Ok(Done::Whole(n)) => { st.pack_bytes += n; st.packs += 1; streak = 0; }
            Ok(Done::Part(n, b)) => { st.blocks += n; st.pack_bytes += b; st.packs_partial += 1; streak = 0; known.lock().insert(id); }
            Ok(Done::Skipped) => { st.packs_skipped += 1; streak = 0; known.lock().insert(id); }
            Err(e) => {
                if e.downcast_ref::<LinkLost>().is_some() { drop(copy); dst.release().await; return Err(e); }
                failed += 1; streak += 1;
                log(&format!("pack-файл {}: {e:#} — пропущен", &id[..12.min(id.len())]));
                if streak >= LINK_LOST_AFTER { drop(copy); dst.release().await; return Err(LinkLost(streak).into()); }
                continue;
            }
        }
        if last_log.elapsed() > Duration::from_secs(10) { log(&format!("передано pack-файлов: {} ({})", st.packs, crate::util::human_bytes(st.pack_bytes))); last_log = std::time::Instant::now(); }
    }
    if st.packs_skipped > 0 || st.packs_partial > 0 {
        log(&format!("pack-файлов, блоки которых уже на площадке: {} (не передавались), частично: {} (переданы только недостающие блоки)", st.packs_skipped, st.packs_partial));
    }
    // Снапшоты — после блоков: принимающая площадка проверяет, что все ссылки на месте.
    let have: HashSet<String> = dst.list_snapshots(None).await?.into_iter().map(|h| h.id).collect();
    for h in src.list_snapshots(None).await? {
        if have.contains(&h.id) { continue; }
        // Снапшот, чьи блоки ещё не доехали (копия завершилась во время прохода), — в следующий раз.
        let r: Result<()> = async { let rec = src.get_snapshot(&h.id).await?; dst.put_snapshot(&rec).await }.await;
        match r {
            Ok(()) => st.snapshots += 1,
            Err(e) => { st.snapshots_deferred += 1; tracing::info!("репликация: снапшот {} отложен: {e:#}", h.id); }
        }
    }
    // Снапшот не принят (каких-то блоков нет) — пропущенные pack-файлы проверить заново в следующий проход.
    if st.snapshots_deferred > 0 { known.lock().clear(); }
    // Объекты (сегменты WAL и т. п.).
    let have: HashSet<(String, String)> = dst.list_all_objects().await?.into_iter().map(|o| (o.ns, o.obj.name)).collect();
    let todo: Vec<_> = src.list_all_objects().await?.into_iter().filter(|o| !have.contains(&(o.ns.clone(), o.obj.name.clone()))).collect();
    let mut copy = futures::stream::iter(todo).map(|o| {
        let (src, dst) = (src.clone(), dst.clone());
        async move {
            let Some(b) = src.get_object(&o.ns, &o.obj.name).await? else { return Ok(0u64) };
            let n = b.len() as u64;
            dst.put_object(&o.ns, &o.obj.name, b).await?;
            Ok::<u64, anyhow::Error>(n)
        }
    }).buffer_unordered(8);
    let mut streak = 0u64;
    while let Some(r) = copy.next().await {
        match r {
            Ok(n) => { st.object_bytes += n; st.objects += 1; streak = 0; }
            Err(e) => {
                failed += 1; streak += 1;
                log(&format!("объект: {e:#} — пропущен"));
                if streak >= LINK_LOST_AFTER { drop(copy); dst.release().await; return Err(LinkLost(streak).into()); }
            }
        }
    }
    dst.release().await;
    if failed > 0 { bail!("не передано объектов и pack-файлов: {failed} (остальное передано, повтор — при следующем проходе)"); }
    Ok(st)
}

/// Репликация с исключёнными продуктами: снапшоты остальных и их недостающие блоки (новыми pack-файлами), объекты
/// кроме WAL исключённых.
async fn by_snapshots(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, exclude: &HashSet<String>, mut st: ReplStats, log: &LogFn) -> Result<ReplStats> {
    let have: HashSet<String> = dst.list_snapshots(None).await?.into_iter().map(|h| h.id).collect();
    let mut todo: Vec<_> = src.list_snapshots(None).await?.into_iter().filter(|h| !have.contains(&h.id)).collect();
    st.snapshots_excluded = todo.iter().filter(|h| exclude.contains(&h.instance)).count() as u64;
    todo.retain(|h| !exclude.contains(&h.instance));
    todo.sort_by(|a, b| a.time.cmp(&b.time));
    if !todo.is_empty() { log(&format!("снапшотов к передаче: {} (исключённые из репликации площадки продукты не передаются)", todo.len())); }
    let mut failed = 0u64;
    for h in &todo {
        let r: Result<()> = async {
            let rec = src.get_snapshot(&h.id).await?;
            let mut need = Vec::new();
            for part in rec.refs.chunks(4096) { need.extend(dst.missing(part.to_vec()).await?); }
            let (c, b) = super::instrepl::copy_blobs(src.clone(), dst.clone(), need, log).await?;
            st.blocks += c; st.pack_bytes += b;
            dst.put_snapshot(&rec).await
        }
        .await;
        match r {
            Ok(()) => st.snapshots += 1,
            Err(e) if e.downcast_ref::<LinkLost>().is_some() => { dst.release().await; return Err(e); }
            Err(e) => { failed += 1; log(&format!("снапшот {}: {e:#} — повтор при следующем проходе", h.id)); }
        }
    }
    let skip = |ns: &str| ns.strip_prefix("wal/").map(|i| exclude.contains(i)).unwrap_or(false);
    let have: HashSet<(String, String)> = dst.list_all_objects().await?.into_iter().map(|o| (o.ns, o.obj.name)).collect();
    for o in src.list_all_objects().await?.into_iter().filter(|o| !skip(&o.ns) && !have.contains(&(o.ns.clone(), o.obj.name.clone()))) {
        match async { let Some(b) = src.get_object(&o.ns, &o.obj.name).await? else { return Ok(0u64) }; let n = b.len() as u64; dst.put_object(&o.ns, &o.obj.name, b).await?; Ok::<u64, anyhow::Error>(n) }.await {
            Ok(n) => { st.object_bytes += n; st.objects += 1; }
            Err(e) => { failed += 1; log(&format!("объект: {e:#} — пропущен")); }
        }
    }
    dst.release().await;
    if failed > 0 { bail!("не передано снапшотов и объектов: {failed} (остальное передано, повтор — при следующем проходе)"); }
    Ok(st)
}

/// Пара «исходная → принимающая».
pub fn pair_key(src: &str, dst: &str) -> String { format!("{src}>{dst}") }

impl Server {
    /// Запомнить место на диске узла площадки (для карточки площадки).
    pub async fn note_site_disk(&self, site_id: &str, remote: &crate::store::http::Remote) {
        if let Some((t, u)) = crate::store::node_disk(remote).await {
            // Готовые образы qcow2 на площадке (продукт «Образы qcow2») — где лежат на её диске.
            let images = crate::store::node_images(remote).await.filter(|v| v["sets"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
            self.site_disk.lock().insert(site_id.to_string(), json!({ "total": t, "used": u, "free": t.saturating_sub(u), "time": chrono::Utc::now(), "images": images }));
        }
    }

    /// Выполнить репликацию площадки на все её принимающие площадки. `job` — записать как задание.
    pub async fn replicate_site(self: &Arc<Self>, site_id: &str, job: Option<&str>, log: LogFn) -> Result<Value> {
        let replicas = self.store.read(|st| st.sites.iter().find(|s| s.id == site_id).map(|s| s.replicas.clone())).unwrap_or_default();
        if replicas.is_empty() { bail!("у площадки нет принимающих площадок"); }
        let src_a = self.site_access(site_id)?;
        let src = crate::store::backend_for(&self.tunnels.connect(&src_a, &*log).await?)?;
        let mut out = Vec::new();
        let mut errors = Vec::new();
        // Продукты, исключённые из репликации площадки (закладка «Репликация» продукта).
        let exclude: HashSet<String> = self.store.read(|st| st.instances.iter().filter(|i| i.site_repl_excluded()).map(|i| i.id.clone()).collect());
        if job.is_some() && !exclude.is_empty() {
            let names = self.store.read(|st| st.instances.iter().filter(|i| i.site_repl_excluded() && i.site_id == site_id).map(|i| format!("«{}»", i.name)).collect::<Vec<_>>());
            if !names.is_empty() { log(&format!("не передаются (исключены из репликации площадки): {}", names.join(", "))); }
        }
        for dst_id in replicas {
            let key = pair_key(site_id, &dst_id);
            // Одна репликация пары за раз; если пара уже синхронизируется (автоматический проход или другое
            // задание) — сказать об этом в журнале, а не ждать молча.
            let lock = self.repl_lock(&key);
            let _g = match lock.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    let dn = self.store.read(|st| st.sites.iter().find(|s| s.id == dst_id).map(|s| s.name.clone())).unwrap_or_else(|| dst_id.clone());
                    let now = self.repl.lock().get(&key).and_then(|v| v["progress"].as_str().map(String::from)).unwrap_or_default();
                    log(&format!("«{dn}» уже синхронизируется (автоматический проход или другое задание){} — продолжу, когда он закончится", if now.is_empty() { String::new() } else { format!(": {now}") }));
                    lock.lock().await
                }
            };
            let started = chrono::Utc::now();
            // Ход прохода виден в карточке площадки, в том числе у автоматического (у него нет задания).
            {
                let mut r = self.repl.lock();
                let e = r.entry(key.clone()).or_insert_with(|| json!({ "ok": null, "error": null }));
                e["running"] = json!(true);
                e["started"] = json!(started);
                e["progress"] = json!("подключение…");
            }
            let log: LogFn = {
                let (me, key, inner) = (self.clone(), key.clone(), log.clone());
                Arc::new(move |m: &str| {
                    inner(m);
                    if let Some(v) = me.repl.lock().get_mut(&key) { if v["running"] == true { v["progress"] = json!(m); } }
                })
            };
            let r: Result<(ReplStats, String)> = async {
                let dst_a = self.site_access(&dst_id)?;
                let name = dst_a.name.clone();
                // Канал упал посреди прохода — поднять заново (connect проверяет, жив ли он) и докачать недостающее.
                let mut attempt = 1;
                let st = loop {
                    let remote = self.tunnels.connect(&dst_a, &*log).await?;
                    let dst = crate::store::backend_for(&remote)?;
                    let known = self.repl_known(&key);
                    let r = replicate(src.clone(), dst, &exclude, &known, &log).await;
                    self.note_site_disk(&dst_id, &remote).await;
                    match r {
                        Err(e) if e.downcast_ref::<LinkLost>().is_some() && attempt < LINK_RETRIES => {
                            attempt += 1;
                            log(&format!("«{name}»: {e:#} — поднимаю канал заново и продолжаю (попытка {attempt} из {LINK_RETRIES})"));
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        r => break r?,
                    }
                };
                // Очистка принимающей площадки по тем же политикам хранения экземпляров.
                if let Ok(Some(adm)) = self.site_access_admin(&dst_id) {
                    // Политики всех экземпляров каталога; копии неизвестных экземпляров не удаляются;
                    // на принимающей площадке всё моложе 7 дней хранится в любом случае.
                    let per: HashMap<String, usize> = self.store.read(|s| s.instances.iter().map(|i| (i.id.clone(), i.retention.keep_last.max(1))).collect());
                    if !per.is_empty() {
                        let keep = per.values().copied().max().unwrap_or(1);
                        let be = crate::store::backend_for(&self.tunnels.connect(&adm, &|_| {}).await?)?;
                        if let Ok(rep) = be.prune(&crate::store::types::PrunePolicy { keep_last: keep, keep_days: Some(7), dry_run: false, per_instance: per, keep_unknown: true }).await {
                            if !rep.snapshots_removed.is_empty() { log(&format!("«{name}»: очистка по политике — снапшотов {}", rep.snapshots_removed.len())); }
                        }
                    }
                }
                Ok((st, name))
            }
            .await;
            let dst_name = self.store.read(|st| st.sites.iter().find(|s| s.id == dst_id).map(|s| s.name.clone())).unwrap_or_else(|| dst_id.clone());
            match r {
                Ok((st, name)) => {
                    if !st.empty() || job.is_some() {
                        if exclude.is_empty() {
                            log(&format!("→ «{name}»: pack-файлов {} ({}), снапшотов {}, объектов {} ({})", st.packs, crate::util::human_bytes(st.pack_bytes), st.snapshots, st.objects, crate::util::human_bytes(st.object_bytes)));
                        } else {
                            log(&format!("→ «{name}»: снапшотов {}, блоков {} ({}), объектов {} ({}); исключённых снапшотов не передано: {}", st.snapshots, st.blocks, crate::util::human_bytes(st.pack_bytes), st.objects, crate::util::human_bytes(st.object_bytes), st.snapshots_excluded));
                        }
                    }
                    self.repl.lock().insert(key, json!({ "ok": true, "time": started, "finished": chrono::Utc::now(), "stats": st, "error": null }));
                    out.push(json!({ "site": dst_id, "name": name, "stats": st }));
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let was_ok = self.repl.lock().get(&key).map(|v| v["ok"] == true).unwrap_or(true);
                    if was_ok { self.audit("system", "replication_failed", format!("{site_id} → {dst_name}: {msg}")); }
                    let prev_ok = self.repl.lock().get(&key).and_then(|v| v.get("last_ok").cloned().or_else(|| if v["ok"] == true { v.get("finished").cloned() } else { None }));
                    self.repl.lock().insert(key, json!({ "ok": false, "time": started, "finished": chrono::Utc::now(), "error": msg, "last_ok": prev_ok }));
                    log(&format!("→ «{dst_name}»: ошибка: {msg}"));
                    errors.push(format!("{dst_name}: {msg}"));
                }
            }
        }
        if !errors.is_empty() { bail!("репликация: {}", errors.join("; ")); }
        Ok(json!({ "replicas": out }))
    }

    /// Поставить репликацию площадки заданием на сервере (после копии или по кнопке).
    pub fn enqueue_replication(self: &Arc<Self>, site_id: &str, user: &str) -> Option<String> {
        let (name, has) = self.store.read(|st| st.sites.iter().find(|s| s.id == site_id).map(|s| (s.name.clone(), !s.replicas.is_empty())))?;
        if !has { return None; }
        // Уже идёт репликация этой площадки — повторить после неё.
        if self.jobs.list(None, 200).iter().any(|j| j.kind == "replicate" && (j.status == "running" || j.status == "queued") && j.title.contains(&format!("«{name}»"))) {
            self.repl_pending.lock().insert(site_id.to_string());
            return None;
        }
        let spec = crate::proto::JobSpec { id: String::new(), kind: "replicate".into(), instance: None, site: None, snapshot_id: None, mode: site_id.to_string(), params: Value::Null };
        let id = self.jobs.enqueue(spec, format!("Репликация площадки «{name}»"), "server", user);
        let s = self.clone();
        let jid = id.clone();
        tokio::spawn(async move {
            let Some(spec) = s.jobs.start_local(&jid) else { return };
            let (s2, j2) = (s.clone(), jid.clone());
            let log: LogFn = Arc::new(move |m: &str| s2.jobs.log(&j2, vec![m.to_string()]));
            match s.replicate_site(&spec.mode, Some(&jid), log.clone()).await {
                Ok(v) => { s.jobs.finish(&jid, true, v, None); }
                Err(e) => { log(&format!("ошибка: {e:#}")); s.jobs.finish(&jid, false, Value::Null, Some(format!("{e:#}"))); }
            }
            if s.repl_pending.lock().remove(&spec.mode) { s.enqueue_replication(&spec.mode, "после копии"); }
        });
        Some(id)
    }
}

/// Фоновая репликация: каждые `interval` минут для всех площадок с принимающими (сегменты WAL идут
/// непрерывно, без заданий копирования). Пустые проходы в историю заданий не пишутся.
pub async fn replication_loop(s: Arc<Server>) {
    let mut last: HashMap<String, std::time::Instant> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if !s.is_active() { continue; }
        let sites: Vec<(String, u64)> = s.store.read(|st| st.sites.iter().filter(|x| !x.replicas.is_empty()).map(|x| (x.id.clone(), x.replication_interval_min.max(1))).collect());
        for (id, every) in sites {
            if last.get(&id).map(|t| t.elapsed() < Duration::from_secs(every * 60)).unwrap_or(false) { continue; }
            last.insert(id.clone(), std::time::Instant::now());
            let s2 = s.clone();
            tokio::spawn(async move {
                let log: LogFn = Arc::new(|m: &str| tracing::debug!("репликация: {m}"));
                if let Err(e) = s2.replicate_site(&id, None, log).await { tracing::warn!("репликация площадки {id}: {e:#}"); }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// Принимающая площадка, у которой «падает канал»: пока `down`, запись pack-файлов не проходит.
    struct Flaky { inner: Arc<dyn Backend>, down: AtomicBool, after: u64, puts: AtomicU64 }

    #[async_trait]
    impl Backend for Flaky {
        async fn read_config(&self) -> Result<Option<RepoConfig>> { self.inner.read_config().await }
        async fn write_config(&self, c: &RepoConfig) -> Result<()> { self.inner.write_config(c).await }
        async fn missing(&self, ids: Vec<ChunkId>) -> Result<Vec<ChunkId>> { self.inner.missing(ids).await }
        async fn put_pack(&self, id: &str, d: bytes::Bytes, e: Vec<IndexEntry>) -> Result<()> {
            if self.puts.fetch_add(1, Ordering::SeqCst) >= self.after { self.down.store(true, Ordering::SeqCst); }
            if self.down.load(Ordering::SeqCst) { anyhow::bail!("Network is unreachable (os error 101)"); }
            self.inner.put_pack(id, d, e).await
        }
        async fn get_blob(&self, id: &ChunkId) -> Result<Vec<u8>> { self.inner.get_blob(id).await }
        async fn put_snapshot(&self, r: &SnapshotRecord) -> Result<()> { self.inner.put_snapshot(r).await }
        async fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord> { self.inner.get_snapshot(id).await }
        async fn list_snapshots(&self, i: Option<&str>) -> Result<Vec<SnapshotHeader>> { self.inner.list_snapshots(i).await }
        async fn put_object(&self, ns: &str, n: &str, d: Vec<u8>) -> Result<()> { self.inner.put_object(ns, n, d).await }
        async fn get_object(&self, ns: &str, n: &str) -> Result<Option<Vec<u8>>> { self.inner.get_object(ns, n).await }
        async fn list_objects(&self, ns: &str, p: &str) -> Result<Vec<ObjInfo>> { self.inner.list_objects(ns, p).await }
        async fn stats(&self) -> Result<RepoStats> { self.inner.stats().await }
        async fn prune(&self, p: &PrunePolicy) -> Result<PruneReport> { self.inner.prune(p).await }
        async fn list_packs(&self) -> Result<Vec<PackInfo>> { self.inner.list_packs().await }
        async fn get_pack(&self, id: &str) -> Result<(Vec<IndexEntry>, bytes::Bytes)> { self.inner.get_pack(id).await }
        async fn list_all_objects(&self) -> Result<Vec<NsObj>> { self.inner.list_all_objects().await }
    }

    #[tokio::test]
    async fn link_lost_aborts_pass_and_resume_copies_rest() {
        let t = tempfile::tempdir().unwrap();
        let src: Arc<dyn Backend> = Arc::new(crate::store::local::LocalBackend::open(t.path().join("src")).unwrap());
        crate::store::repo::Repo::init(src.clone(), "test-password").await.unwrap();
        for i in 0..40u32 {
            let data = bytes::Bytes::from(format!("pack-{i}-").repeat(100).into_bytes());
            src.put_pack(&blake3::hash(&data).to_hex().to_string(), data, vec![]).await.unwrap();
        }
        let dst_inner: Arc<dyn Backend> = Arc::new(crate::store::local::LocalBackend::open(t.path().join("dst")).unwrap());
        let flaky = Arc::new(Flaky { inner: dst_inner.clone(), down: AtomicBool::new(false), after: 10, puts: AtomicU64::new(0) });
        let log: LogFn = Arc::new(|_| {});
        // Канал «падает» после 10 pack-файлов: проход прерывается быстро, а не пропускает все 40.
        let e = replicate(src.clone(), flaky.clone(), &HashSet::new(), &Known::default(), &log).await.unwrap_err();
        assert!(e.downcast_ref::<LinkLost>().is_some(), "{e:#}");
        let tried = flaky.puts.load(Ordering::SeqCst);
        // Успешные записи, закончившиеся после первых ошибок, сбрасывают счёт подряд — несколько лишних попыток
        // возможны, но проход прерывается задолго до конца списка.
        assert!(tried <= 30, "проход должен прерваться, а не пробовать все 40: {tried}");
        // Запись, начатая до обрыва, может закончиться в фоне (отмена прохода её не прерывает) — дождаться.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let have = dst_inner.list_packs().await.unwrap().len();
        assert!((7..=10).contains(&have), "{have}");
        // Канал снова поднят: следующий проход докачивает только недостающее.
        let st = replicate(src.clone(), dst_inner.clone(), &HashSet::new(), &Known::default(), &log).await.unwrap();
        assert_eq!(st.packs as usize, 40 - have);
        assert_eq!(dst_inner.list_packs().await.unwrap().len(), 40);
    }

    /// pack-файл из блоков: (id, данные, индекс).
    fn mkpack(blobs: &[&[u8]]) -> (String, bytes::Bytes, Vec<IndexEntry>) {
        let mut data = Vec::new();
        let mut e = Vec::new();
        for b in blobs {
            e.push(IndexEntry { id: ChunkId(*blake3::hash(b).as_bytes()), offset: data.len() as u32, len: b.len() as u32 });
            data.extend_from_slice(b);
        }
        let d = bytes::Bytes::from(data);
        (blake3::hash(&d).to_hex().to_string(), d, e)
    }

    /// Возврат к обычной репликации после репликации с исключениями: pack-файлы, блоки которых уже на принимающей
    /// площадке (под другими pack-файлами), не передаются; частично совпавшие — передаются только недостающие блоки.
    #[tokio::test]
    async fn packs_with_blocks_already_there_are_not_resent() {
        let t = tempfile::tempdir().unwrap();
        let src: Arc<dyn Backend> = Arc::new(crate::store::local::LocalBackend::open(t.path().join("src")).unwrap());
        crate::store::repo::Repo::init(src.clone(), "test-password").await.unwrap();
        let dst: Arc<dyn Backend> = Arc::new(crate::store::local::LocalBackend::open(t.path().join("dst")).unwrap());
        let a: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 1000 + i as usize]).collect();
        // На исходной: P1 = a0..a2, P2 = a3..a5. На принимающей уже есть a0..a3, но в другом pack-файле.
        let (p1, d1, e1) = mkpack(&[&a[0], &a[1], &a[2]]);
        let (p2, d2, e2) = mkpack(&[&a[3], &a[4], &a[5]]);
        src.put_pack(&p1, d1, e1).await.unwrap();
        src.put_pack(&p2, d2, e2).await.unwrap();
        dst.write_config(&src.read_config().await.unwrap().unwrap()).await.unwrap();
        let (q, dq, eq) = mkpack(&[&a[3], &a[0], &a[2], &a[1]]);
        dst.put_pack(&q, dq, eq).await.unwrap();
        let known = Known::default();
        let log: LogFn = Arc::new(|_| {});
        let st = replicate(src.clone(), dst.clone(), &HashSet::new(), &known, &log).await.unwrap();
        assert_eq!((st.packs, st.packs_skipped, st.packs_partial, st.blocks), (0, 1, 1, 2), "{st:?}");
        // Все блоки на месте, читаются.
        for b in &a { assert_eq!(&dst.get_blob(&ChunkId(*blake3::hash(b).as_bytes())).await.unwrap(), b); }
        // Следующий проход: оба pack-файла запомнены — ни проверок, ни передачи.
        let st = replicate(src.clone(), dst.clone(), &HashSet::new(), &known, &log).await.unwrap();
        assert_eq!((st.packs, st.packs_skipped, st.packs_partial), (0, 0, 0), "{st:?}");
        // Пакетное чтение блоков отдаёт их по порядку.
        let ids: Vec<ChunkId> = a.iter().rev().map(|b| ChunkId(*blake3::hash(b).as_bytes())).collect();
        let got = src.get_blobs(&ids).await.unwrap();
        assert_eq!(got, a.iter().rev().cloned().collect::<Vec<_>>());
    }
}
