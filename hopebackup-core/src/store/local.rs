//! Репозиторий в локальном каталоге (так хранит данные узел площадки).
//!
//! ```text
//! <root>/config.json            открытая конфигурация, мастер-ключ зашифрован паролем
//! <root>/packs/ab/<id>.pack     зашифрованные блоки подряд
//! <root>/index/<id>.json        где какой блок лежит в pack-файле (идентификаторы — keyed BLAKE3)
//! <root>/snapshots/<id>.json    открытый заголовок + список ссылок + зашифрованное тело
//! <root>/heads/<id>.json        только заголовок снапшота (для списка точек; восстанавливается из снапшота)
//! <root>/objects/<ns>/<name>    сегменты WAL и прочие объекты
//! ```
//!
//! В памяти — компактный индекс блоков (≈44 байта на блок) и заголовки снапшотов; записи снапшотов
//! целиком в память не читаются.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use bytes::Bytes;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::backend::{valid_name, valid_ns, Backend};
use super::cindex::ChunkIndex;
use super::types::*;

#[derive(Serialize, Deserialize)]
struct IndexFile {
    pack: String,
    entries: Vec<IndexEntry>,
}

struct Inner {
    root: PathBuf,
    index: RwLock<ChunkIndex>,
    /// Заголовки снапшотов по идентификатору.
    heads: RwLock<HashMap<String, SnapshotHeader>>,
    /// Когда блоки pack-файла последний раз признавались существующими: такие pack-файлы очистка
    /// не трогает сутки — на них может ссылаться копия, которая ещё пишется.
    touched: RwLock<HashMap<u32, SystemTime>>,
    /// Запись и очистка не идут одновременно.
    gc: RwLock<()>,
}

#[derive(Clone)]
pub struct LocalBackend {
    inner: Arc<Inner>,
}

const TOUCH_GRACE: Duration = Duration::from_secs(24 * 3600);

impl LocalBackend {
    pub fn open(root: impl AsRef<Path>) -> Result<LocalBackend> {
        let root = root.as_ref().to_path_buf();
        for d in ["packs", "index", "snapshots", "heads", "objects", "tmp"] {
            std::fs::create_dir_all(root.join(d)).with_context(|| format!("создание {}", root.join(d).display()))?;
        }
        let started = std::time::Instant::now();
        let mut idx = ChunkIndex::new();
        let files: Vec<(PathBuf, u64)> = std::fs::read_dir(root.join("index"))?
            .flatten()
            .filter_map(|e| { let p = e.path(); (p.extension().and_then(|s| s.to_str()) == Some("json")).then(|| { let n = e.metadata().map(|m| m.len()).unwrap_or(0); (p, n) }) })
            .collect();
        // Запись индекса в JSON — не меньше ~90 байт: оценка сверху, чтобы массив не перевыделялся.
        idx.reserve((files.iter().map(|f| f.1).sum::<u64>() / 90) as usize);
        for (p, _) in &files {
            let f: IndexFile = match std::fs::read(p).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
                Ok(f) => f,
                Err(e) => { tracing::warn!("пропущен повреждённый индекс {}: {e}", p.display()); continue; }
            };
            idx.load_pack(&f.pack, &f.entries)?;
        }
        idx.finish_load();
        let heads = load_heads(&root)?;
        if idx.len() > 100_000 {
            tracing::info!("репозиторий {}: блоков {}, снапшотов {}, индекс {} в памяти, открыт за {:.1} с", root.display(), idx.len(), heads.len(), crate::util::human_bytes(idx.mem_bytes() as u64), started.elapsed().as_secs_f64());
        }
        Ok(LocalBackend { inner: Arc::new(Inner { root, index: RwLock::new(idx), heads: RwLock::new(heads), touched: RwLock::new(HashMap::new()), gc: RwLock::new(()) }) })
    }

    fn snap_path(root: &Path, id: &str) -> PathBuf { root.join("snapshots").join(format!("{id}.json")) }
    fn head_path(root: &Path, id: &str) -> PathBuf { root.join("heads").join(format!("{id}.json")) }

    /// Путь к записи снапшота (узел отдаёт её потоком, не читая в память).
    pub fn snapshot_file(&self, id: &str) -> Result<PathBuf> {
        if !valid_name(id) { bail!("недопустимый идентификатор снапшота"); }
        let p = Self::snap_path(&self.inner.root, id);
        if !p.exists() { return Err(NotFound(format!("снапшот {id}")).into()); }
        Ok(p)
    }

    /// Каталог для временных файлов (приём снапшота потоком).
    pub fn tmp_file(&self) -> PathBuf { self.inner.root.join("tmp").join(format!("{}.tmp", uuid::Uuid::new_v4())) }

    /// Принять снапшот из временного файла (узел пишет тело запроса на диск): проверка разбором
    /// потоком — заголовок, все ссылки есть на площадке, — затем атомарное переименование.
    pub async fn put_snapshot_file(&self, tmp: PathBuf, id: String) -> Result<SnapshotHeader> {
        let r = self.blocking(move |i| {
            let res = (|| -> Result<SnapshotHeader> {
                if !valid_name(&id) { bail!("недопустимый идентификатор снапшота"); }
                let f = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(&tmp)?);
                let mut absent = 0u64;
                let sc = {
                    let idx = i.index.read();
                    super::snapfile::scan(f, |r| if !idx.contains(&r) { absent += 1 })?
                };
                if sc.header.id != id { bail!("идентификатор не совпадает"); }
                if !sc.has_body { bail!("в записи снапшота нет тела"); }
                if absent > 0 { bail!("снапшот ссылается на блоки, которых нет на площадке ({absent})"); }
                let dst = LocalBackend::snap_path(&i.root, &id);
                if dst.exists() {
                    if !same_file(&dst, &tmp)? { return Err(Conflict(format!("снапшот {id}")).into()); }
                    let _ = std::fs::remove_file(&tmp);
                } else {
                    std::fs::File::open(&tmp)?.sync_all()?;
                    std::fs::rename(&tmp, &dst)?;
                }
                save_head(i, &sc.header)?;
                Ok(sc.header)
            })();
            if res.is_err() { let _ = std::fs::remove_file(&tmp); }
            res
        });
        r.await
    }

    pub fn root(&self) -> &Path { &self.inner.root }

    fn pack_path(root: &Path, id: &str) -> PathBuf { root.join("packs").join(&id[..2]).join(format!("{id}.pack")) }

    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&Inner) -> Result<T> + Send + 'static) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || f(&inner)).await.map_err(|e| anyhow!("задача хранилища: {e}"))?
    }
}

/// Записать файл атомарно: во временный, fsync, переименование.
fn write_atomic(root: &Path, dst: &Path, data: &[u8]) -> Result<()> {
    if let Some(p) = dst.parent() { std::fs::create_dir_all(p)?; }
    let tmp = root.join("tmp").join(format!("{}.tmp", uuid::Uuid::new_v4()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

/// Записать, только если файла ещё нет; одинаковое содержимое — не ошибка.
fn write_once(root: &Path, dst: &Path, data: &[u8], what: &str) -> Result<()> {
    if let Ok(old) = std::fs::read(dst) {
        if old == data { return Ok(()); }
        return Err(Conflict(what.to_string()).into());
    }
    write_atomic(root, dst, data)
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[async_trait]
impl Backend for LocalBackend {
    async fn read_config(&self) -> Result<Option<RepoConfig>> {
        self.blocking(|i| match std::fs::read(i.root.join("config.json")) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn write_config(&self, cfg: &RepoConfig) -> Result<()> {
        let data = serde_json::to_vec_pretty(cfg)?;
        self.blocking(move |i| {
            let p = i.root.join("config.json");
            if p.exists() { return Err(Conflict("репозиторий уже создан".into()).into()); }
            write_atomic(&i.root, &p, &data)
        })
        .await
    }

    async fn missing(&self, ids: Vec<ChunkId>) -> Result<Vec<ChunkId>> {
        let idx = self.inner.index.read();
        let mut out = Vec::new();
        let mut touched = Vec::new();
        for id in ids {
            match idx.get(&id) {
                Some(l) => touched.push(l.pack),
                None => out.push(id),
            }
        }
        drop(idx);
        if !touched.is_empty() {
            let now = SystemTime::now();
            let mut t = self.inner.touched.write();
            for p in touched { t.insert(p, now); }
        }
        Ok(out)
    }

    async fn put_pack(&self, pack_id: &str, data: Bytes, entries: Vec<IndexEntry>) -> Result<()> {
        let pack_id = pack_id.to_string();
        self.blocking(move |i| {
            let real = blake3::hash(&data).to_hex().to_string();
            if real != pack_id { bail!("контрольная сумма pack-файла не совпадает"); }
            for e in &entries {
                if (e.offset as u64 + e.len as u64) > data.len() as u64 { bail!("запись индекса выходит за пределы pack-файла"); }
            }
            let _g = i.gc.read();
            write_atomic(&i.root, &LocalBackend::pack_path(&i.root, &pack_id), &data)?;
            let f = IndexFile { pack: pack_id.clone(), entries };
            write_atomic(&i.root, &i.root.join("index").join(format!("{pack_id}.json")), &serde_json::to_vec(&f)?)?;
            i.index.write().add_pack(&pack_id, &f.entries)
        })
        .await
    }

    async fn get_blob(&self, id: &ChunkId) -> Result<Vec<u8>> {
        let (pack, loc) = {
            let idx = self.inner.index.read();
            let l = idx.get(id).ok_or_else(|| NotFound(format!("блок {}", id.to_hex())))?;
            (idx.pack_hex(l.pack), l)
        };
        self.blocking(move |i| {
            let mut f = std::fs::File::open(LocalBackend::pack_path(&i.root, &pack))?;
            f.seek(SeekFrom::Start(loc.offset as u64))?;
            let mut buf = vec![0u8; loc.len as usize];
            f.read_exact(&mut buf)?;
            Ok(buf)
        })
        .await
    }

    async fn put_snapshot(&self, rec: &SnapshotRecord) -> Result<()> {
        if !valid_name(&rec.header.id) { bail!("недопустимый идентификатор снапшота"); }
        let present = {
            let idx = self.inner.index.read();
            rec.refs.iter().all(|r| idx.contains(r))
        };
        if !present { bail!("снапшот ссылается на блоки, которых нет на площадке"); }
        let data = serde_json::to_vec(rec)?;
        let header = rec.header.clone();
        self.blocking(move |i| {
            let id = header.id.clone();
            write_once(&i.root, &LocalBackend::snap_path(&i.root, &id), &data, &format!("снапшот {id}"))?;
            save_head(i, &header)
        })
        .await
    }

    async fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord> {
        if !valid_name(id) { bail!("недопустимый идентификатор снапшота"); }
        let id = id.to_string();
        self.blocking(move |i| match std::fs::read(LocalBackend::snap_path(&i.root, &id)) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(NotFound(format!("снапшот {id}")).into()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_snapshots(&self, instance: Option<&str>) -> Result<Vec<SnapshotHeader>> {
        let mut out: Vec<SnapshotHeader> = self.inner.heads.read().values().filter(|h| instance.map(|i| h.instance == i).unwrap_or(true)).cloned().collect();
        out.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn put_object(&self, ns: &str, name: &str, data: Vec<u8>) -> Result<()> {
        if !valid_ns(ns) || !valid_name(name) { bail!("недопустимое имя объекта"); }
        let (ns, name) = (ns.to_string(), name.to_string());
        self.blocking(move |i| write_once(&i.root, &i.root.join("objects").join(&ns).join(&name), &data, &format!("{ns}/{name}"))).await
    }

    async fn get_object(&self, ns: &str, name: &str) -> Result<Option<Vec<u8>>> {
        if !valid_ns(ns) || !valid_name(name) { bail!("недопустимое имя объекта"); }
        let (ns, name) = (ns.to_string(), name.to_string());
        self.blocking(move |i| match std::fs::read(i.root.join("objects").join(&ns).join(&name)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list_objects(&self, ns: &str, prefix: &str) -> Result<Vec<ObjInfo>> {
        if !valid_ns(ns) { bail!("недопустимое имя пространства"); }
        let (ns, prefix) = (ns.to_string(), prefix.to_string());
        self.blocking(move |i| {
            let dir = i.root.join("objects").join(&ns);
            let mut out = Vec::new();
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
                Err(e) => return Err(e.into()),
            };
            for e in rd {
                let e = e?;
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with(&prefix) || !e.file_type()?.is_file() { continue; }
                let size = e.metadata()?.len();
                out.push(ObjInfo { mtime: mtime(&e.path()), name, size });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
    }

    async fn stats(&self) -> Result<RepoStats> {
        let chunks = self.inner.index.read().len() as u64;
        let snapshots = self.inner.heads.read().len() as u64;
        self.blocking(move |i| {
            let mut s = RepoStats { chunks, ..Default::default() };
            if let Ok(rd) = std::fs::read_dir(i.root.join("packs")) {
                for d in rd.flatten() {
                    if let Ok(rd2) = std::fs::read_dir(d.path()) {
                        for f in rd2.flatten() {
                            s.packs += 1;
                            s.pack_bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                        }
                    }
                }
            }
            s.snapshots = snapshots;
            fn walk(p: &Path, s: &mut RepoStats) {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for e in rd.flatten() {
                        let Ok(ft) = e.file_type() else { continue };
                        if ft.is_dir() { walk(&e.path(), s) } else {
                            s.objects += 1;
                            s.object_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                        }
                    }
                }
            }
            walk(&i.root.join("objects"), &mut s);
            Ok(s)
        })
        .await
    }

    async fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport> {
        let policy = policy.clone();
        self.blocking(move |i| prune_blocking(i, &policy)).await
    }

    async fn list_packs(&self) -> Result<Vec<PackInfo>> {
        self.blocking(|i| {
            let mut out = Vec::new();
            for e in std::fs::read_dir(i.root.join("index"))?.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".json") else { continue };
                let size = std::fs::metadata(LocalBackend::pack_path(&i.root, id)).map(|m| m.len()).unwrap_or(0);
                if size > 0 { out.push(PackInfo { id: id.to_string(), size }); }
            }
            Ok(out)
        })
        .await
    }

    async fn pack_index(&self, id: &str) -> Result<Vec<IndexEntry>> {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) { bail!("неверный идентификатор pack-файла"); }
        let id = id.to_string();
        self.blocking(move |i| {
            let idx = std::fs::read(i.root.join("index").join(format!("{id}.json"))).map_err(|_| NotFound(format!("pack {id}")))?;
            Ok(serde_json::from_slice::<IndexFile>(&idx)?.entries)
        })
        .await
    }

    async fn get_blobs(&self, ids: &[ChunkId]) -> Result<Vec<Vec<u8>>> {
        // Чтение с диска узла: по порядку, ответ не больше BLOBS_MAX_BYTES (остальное клиент запросит снова).
        let mut out = Vec::new();
        let mut total = 0usize;
        for id in ids {
            if !out.is_empty() && total >= super::types::BLOBS_MAX_BYTES { break; }
            let b = self.get_blob(id).await?;
            total += b.len();
            out.push(b);
        }
        Ok(out)
    }

    async fn get_pack(&self, id: &str) -> Result<(Vec<IndexEntry>, Bytes)> {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) { bail!("неверный идентификатор pack-файла"); }
        let id = id.to_string();
        self.blocking(move |i| {
            let idx = std::fs::read(i.root.join("index").join(format!("{id}.json"))).map_err(|_| NotFound(format!("pack {id}")))?;
            let f: IndexFile = serde_json::from_slice(&idx)?;
            let data = std::fs::read(LocalBackend::pack_path(&i.root, &id)).map_err(|_| NotFound(format!("pack {id}")))?;
            if blake3::hash(&data).to_hex().as_str() != id { bail!("pack-файл {} повреждён (контрольная сумма)", &id[..12]); }
            Ok((f.entries, Bytes::from(data)))
        })
        .await
    }

    async fn list_all_objects(&self) -> Result<Vec<NsObj>> {
        self.blocking(|i| {
            let base = i.root.join("objects");
            let mut out = Vec::new();
            fn walk(base: &Path, p: &Path, out: &mut Vec<NsObj>) {
                let Ok(rd) = std::fs::read_dir(p) else { return };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() { walk(base, &e.path(), out); continue; }
                    let Some(ns) = p.strip_prefix(base).ok().map(|x| x.to_string_lossy().to_string()) else { continue };
                    if ns.is_empty() { continue; }
                    out.push(NsObj { ns, obj: ObjInfo { name: e.file_name().to_string_lossy().to_string(), size: e.metadata().map(|m| m.len()).unwrap_or(0), mtime: mtime(&e.path()) } });
                }
            }
            walk(&base, &base, &mut out);
            Ok(out)
        })
        .await
    }
}

/// Политика хранения: какие снапшоты удалить, затем сборка мусора pack-файлов и старых сегментов WAL.
///
/// Память: заголовки снапшотов и битовая карта живых блоков (1 бит на блок); ссылки оставляемых
/// снапшотов читаются из файлов потоком, по одному снапшоту.
fn prune_blocking(i: &Inner, policy: &PrunePolicy) -> Result<PruneReport> {
    let _g = i.gc.write();
    let mut rep = PruneReport { dry_run: policy.dry_run, ..Default::default() };
    let mut all: Vec<SnapshotHeader> = i.heads.read().values().cloned().collect();
    all.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.id.cmp(&a.id)));
    let (ki, di) = super::prune::plan(&all, policy);
    let keep: Vec<&SnapshotHeader> = ki.iter().map(|&n| &all[n]).collect();
    // Живые блоки — те, на которые ссылаются оставшиеся снапшоты. Нечитаемый снапшот — ошибка:
    // иначе его блоки сочли бы мёртвыми.
    i.index.write().compact();
    let idx = i.index.read();
    let mut live = idx.new_bitmap();
    for h in &keep {
        let p = LocalBackend::snap_path(&i.root, &h.id);
        let f = std::fs::File::open(&p).with_context(|| format!("снапшот {} — очистка остановлена, чтобы не удалить его блоки", h.id))?;
        super::snapfile::scan(std::io::BufReader::with_capacity(1 << 20, f), |r| if let Some(n) = idx.position(&r) { live.set(n) })
            .with_context(|| format!("снапшот {} не читается — очистка остановлена, чтобы не удалить его блоки", h.id))?;
    }
    for &n in &di {
        let h = &all[n];
        rep.snapshots_removed.push(h.id.clone());
        if !policy.dry_run {
            std::fs::remove_file(LocalBackend::snap_path(&i.root, &h.id))?;
            let _ = std::fs::remove_file(LocalBackend::head_path(&i.root, &h.id));
            i.heads.write().remove(&h.id);
        }
    }
    let (any_live, any_dead) = idx.pack_liveness(&live);
    drop(live);
    let touched = i.touched.read().clone();
    let now_sys = SystemTime::now();
    let mut dead = Vec::new();
    for no in 0..idx.pack_slots() as u32 {
        if !idx.pack_alive(no) { continue; }
        if any_live[no as usize] {
            if any_dead[no as usize] { rep.packs_partial += 1; }
            continue;
        }
        let pp = LocalBackend::pack_path(&i.root, &idx.pack_hex(no));
        // Свежие pack-файлы и те, чьи блоки недавно признаны существующими, не трогаем: на них может
        // ссылаться копия, которая ещё не дописана.
        let recent_touch = touched.get(&no).map(|t| now_sys.duration_since(*t).unwrap_or_default() < TOUCH_GRACE).unwrap_or(false);
        let young_file = std::fs::metadata(&pp).and_then(|m| m.modified()).map(|t| now_sys.duration_since(t).unwrap_or_default() < Duration::from_secs(3600)).unwrap_or(true);
        if recent_touch || young_file { continue; }
        rep.packs_removed += 1;
        rep.bytes_freed += std::fs::metadata(&pp).map(|m| m.len()).unwrap_or(0);
        dead.push(no);
    }
    drop(any_live);
    drop(any_dead);
    if !policy.dry_run && !dead.is_empty() {
        for &no in &dead {
            let hex = idx.pack_hex(no);
            // Сначала индекс, затем данные: индекс не ссылается на отсутствующий pack-файл.
            std::fs::remove_file(i.root.join("index").join(format!("{hex}.json")))?;
            let _ = std::fs::remove_file(LocalBackend::pack_path(&i.root, &hex));
        }
        drop(idx);
        i.index.write().remove_packs(&dead);
        let mut t = i.touched.write();
        for no in &dead { t.remove(no); }
    }
    // Сегменты WAL старше начала самой старой оставшейся полной копии экземпляра больше не нужны.
    for (inst, start) in super::prune::wal_starts(keep.iter().copied()) {
        let dir = i.root.join("objects").join("wal").join(&inst);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if super::prune::wal_expired(&name, &start) {
                rep.wal_removed += 1;
                if !policy.dry_run { let _ = std::fs::remove_file(e.path()); }
            }
        }
    }
    Ok(rep)
}

/// Заголовки снапшотов: из `heads/`; для снапшотов без заголовка (репозиторий прежней версии или
/// сбой между записью снапшота и заголовка) — разбором записи потоком.
fn load_heads(root: &Path) -> Result<HashMap<String, SnapshotHeader>> {
    let mut out = HashMap::new();
    let mut made = 0;
    for e in std::fs::read_dir(root.join("snapshots"))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".json") else { continue };
        let hp = LocalBackend::head_path(root, id);
        let h = match std::fs::read(&hp).ok().and_then(|b| serde_json::from_slice::<SnapshotHeader>(&b).ok()) {
            Some(h) => h,
            None => {
                let f = match std::fs::File::open(e.path()) { Ok(f) => f, Err(_) => continue };
                match super::snapfile::header(std::io::BufReader::with_capacity(1 << 20, f)) {
                    Ok(h) => { let _ = write_atomic(root, &hp, &serde_json::to_vec(&h)?); made += 1; h }
                    Err(err) => { tracing::warn!("снапшот {id} не читается: {err:#}"); continue }
                }
            }
        };
        out.insert(id.to_string(), h);
    }
    // Заголовки удалённых снапшотов.
    for e in std::fs::read_dir(root.join("heads"))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix(".json") { if !out.contains_key(id) { let _ = std::fs::remove_file(e.path()); } }
    }
    if made > 0 { tracing::info!("{}: заголовков снапшотов создано {made}", root.display()); }
    Ok(out)
}

fn save_head(i: &Inner, h: &SnapshotHeader) -> Result<()> {
    write_atomic(&i.root, &LocalBackend::head_path(&i.root, &h.id), &serde_json::to_vec(h)?)?;
    i.heads.write().insert(h.id.clone(), h.clone());
    Ok(())
}

/// Одинаковое ли содержимое у двух файлов (сравнение потоком).
fn same_file(a: &Path, b: &Path) -> Result<bool> {
    let (fa, fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() { return Ok(false); }
    let (mut ra, mut rb) = (std::io::BufReader::new(fa), std::io::BufReader::new(fb));
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = ra.read(&mut ba)?;
        if n == 0 { return Ok(true); }
        rb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] { return Ok(false); }
    }
}
