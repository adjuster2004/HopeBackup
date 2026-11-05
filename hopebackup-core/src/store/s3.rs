//! Репозиторий в объектном хранилище S3: Yandex Object Storage, AWS S3, MinIO, Ceph RGW.
//!
//! Раскладка та же, что на узле площадки (`<префикс>/<репозиторий>/packs|index|snapshots|objects`),
//! поэтому репозиторий можно переносить между узлом и S3 простым копированием. Данные шифруются
//! на источнике — хранилище видит только шифротекст.
//!
//! Защита от удаления: у агентов — ключ без права удаления (политика бакета/IAM), у сервера —
//! отдельный ключ для очистки по политике хранения. Object Lock (WORM): pack-файлы, индекс,
//! снапшоты и сегменты WAL пишутся с датой хранения — их нельзя удалить или перезаписать до неё
//! даже владельцу ключей (режим COMPLIANCE).
//!
//! Очистка не удаляет pack-файлы, пока есть свежие блокировки записи (`locks/`): агент ставит
//! блокировку на время копии и обновляет её каждые 5 минут.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use super::backend::{valid_name, valid_ns, Backend};
use super::cindex::ChunkIndex;
use super::types::*;
use crate::cloud::objstore::{ObjStore, S3Store};
use crate::cloud::s3::{lock_headers, S3};

/// Доступ к бакету (часть `Remote` для площадок S3).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct S3Access {
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub path_style: bool,
    /// `GOVERNANCE` или `COMPLIANCE` — Object Lock на `lock_days` дней.
    #[serde(default)]
    pub lock_mode: String,
    #[serde(default)]
    pub lock_days: u32,
    /// Класс хранения: STANDARD, COLD/ICE (Yandex), STANDARD_IA, GLACIER_IR (AWS).
    #[serde(default)]
    pub storage_class: String,
    /// Отпечаток сертификата (MinIO с самоподписанным сертификатом); пусто — системные CA.
    #[serde(default)]
    pub fingerprint: String,
}

impl S3Access {
    pub fn client(&self) -> Result<S3> {
        if self.bucket.trim().is_empty() { bail!("не задан бакет S3"); }
        if self.access_key.trim().is_empty() || self.secret_key.trim().is_empty() { bail!("не заданы ключи доступа S3"); }
        S3::new(&self.endpoint, &self.region, &self.bucket, &self.access_key, &self.secret_key, self.path_style, &self.fingerprint)
    }
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    pack: String,
    entries: Vec<IndexEntry>,
}

struct Inner {
    c: Arc<dyn ObjStore>,
    /// `<префикс>/<репозиторий>/`
    root: String,
    cache: PathBuf,
    index: tokio::sync::Mutex<Option<ChunkIndex>>,
    /// Своя блокировка записи: ключ и задача её обновления.
    write_lock: parking_lot::Mutex<Option<(String, tokio::task::JoinHandle<()>)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some((key, h)) = self.write_lock.lock().take() {
            h.abort();
            // Снять блокировку в фоне (если рантайм ещё жив); иначе она устареет сама.
            let c = self.c.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() { rt.spawn(async move { let _ = c.delete(&key).await; }); }
        }
    }
}

#[derive(Clone)]
pub struct S3Backend {
    i: Arc<Inner>,
}

const LOCK_FRESH_SECS: i64 = 30 * 60;

fn pack_key(root: &str, id: &str) -> String { format!("{root}packs/{}/{id}.pack", &id[..2]) }

impl S3Backend {
    pub fn open(a: &S3Access, repo: &str) -> Result<S3Backend> {
        let mut worm = lock_headers(&a.lock_mode, a.lock_days);
        let sc = a.storage_class.trim();
        if !sc.is_empty() { worm.push(("x-amz-storage-class".into(), sc.to_uppercase())); }
        let tag = format!("s3|{}|{}", a.endpoint, a.bucket);
        let store: Arc<dyn ObjStore> = Arc::new(S3Store { c: a.client()?, worm, tag });
        Ok(Self::open_store(store, &a.prefix, repo))
    }

    /// Репозиторий в любом хранилище объектов (S3, WebDAV, Яндекс Диск, Google Диск).
    pub fn open_store(store: Arc<dyn ObjStore>, prefix: &str, repo: &str) -> S3Backend {
        let pre = prefix.trim().trim_matches('/');
        let root = if pre.is_empty() { format!("{repo}/") } else { format!("{pre}/{repo}/") };
        let tag = crate::util::sha256_hex(&format!("{}|{root}", store.id()));
        let cache = crate::agent::data_dir().join("cache").join("s3").join(&tag[..16]);
        S3Backend { i: Arc::new(Inner { c: store, root, cache, index: tokio::sync::Mutex::new(None), write_lock: parking_lot::Mutex::new(None) }) }
    }

    fn k(&self, rel: &str) -> String { format!("{}{rel}", self.i.root) }

    /// Индекс блоков: список `index/` + локальный кэш (файлы индекса не меняются).
    async fn index(&self) -> Result<tokio::sync::MutexGuard<'_, Option<ChunkIndex>>> {
        let mut g = self.i.index.lock().await;
        if g.is_some() { return Ok(g); }
        let dir = self.i.cache.join("index");
        std::fs::create_dir_all(&dir)?;
        let objs = self.i.c.list(&self.k("index/"), false).await.context("список индекса S3")?;
        let listed: HashSet<String> = objs.iter().filter_map(|o| o.key.rsplit('/').next().map(|s| s.to_string())).collect();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() { if !listed.contains(&e.file_name().to_string_lossy().to_string()) { let _ = std::fs::remove_file(e.path()); } }
        }
        let todo: Vec<String> = objs.iter().map(|o| o.key.clone()).filter(|k| !dir.join(k.rsplit('/').next().unwrap_or("")).exists()).collect();
        let c = self.i.c.clone();
        let mut fetch = futures::stream::iter(todo).map(|k| { let c = c.clone(); async move { (k.clone(), c.get(&k).await) } }).buffer_unordered(16);
        while let Some((k, r)) = fetch.next().await {
            let b = r?.ok_or_else(|| anyhow!("индекс {k} пропал во время чтения"))?;
            crate::util::write_json(&dir.join(k.rsplit('/').next().unwrap_or("x")), &serde_json::from_slice::<IndexFile>(&b)?, false)?;
        }
        let mut idx = ChunkIndex::new();
        idx.reserve((objs.iter().map(|o| o.size).sum::<u64>() / 90) as usize);
        for name in &listed {
            let Ok(b) = std::fs::read(dir.join(name)) else { continue };
            let Ok(f) = serde_json::from_slice::<IndexFile>(&b) else { tracing::warn!("пропущен повреждённый индекс {name}"); continue };
            idx.load_pack(&f.pack, &f.entries)?;
        }
        idx.finish_load();
        *g = Some(idx);
        Ok(g)
    }

    /// Блокировка записи: пока она свежая, очистка не трогает pack-файлы. Без неё писать нельзя —
    /// очистка могла бы удалить блоки, на которые опирается дедупликация этой копии.
    async fn hold_write_lock(&self) -> Result<()> {
        if self.i.write_lock.lock().is_some() { return Ok(()); }
        let key = self.k(&format!("locks/{}.json", uuid::Uuid::new_v4()));
        let body = serde_json::json!({ "host": crate::util::hostname(), "pid": std::process::id(), "time": chrono::Utc::now() }).to_string();
        self.i.c.put(&key, Bytes::from(body), false).await.context("блокировка записи S3 (нужно право записи в locks/)")?;
        let c = self.i.c.clone();
        let k2 = key.clone();
        let h = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                let body = serde_json::json!({ "host": crate::util::hostname(), "pid": std::process::id(), "time": chrono::Utc::now() }).to_string();
                if let Err(e) = c.put(&k2, Bytes::from(body), false).await { tracing::warn!("обновление блокировки записи S3: {e:#}"); }
            }
        });
        let mut g = self.i.write_lock.lock();
        if g.is_some() {
            // Параллельный вызов успел раньше — лишнюю блокировку снять.
            h.abort();
            let c = self.i.c.clone();
            tokio::spawn(async move { let _ = c.delete(&key).await; });
        } else {
            *g = Some((key, h));
        }
        Ok(())
    }

    fn own_lock(&self) -> Option<String> { self.i.write_lock.lock().as_ref().map(|(k, _)| k.clone()) }

    /// Свежие блокировки записи других процессов.
    async fn foreign_locks(&self, delete_stale: bool) -> Result<bool> {
        let now = chrono::Utc::now();
        let own = self.own_lock();
        let locks = self.i.c.list(&self.k("locks/"), false).await?;
        if delete_stale {
            for l in locks.iter().filter(|l| (now - l.last_modified).num_hours() >= 24) { let _ = self.i.c.delete(&l.key).await; }
        }
        for l in locks.iter().filter(|l| Some(&l.key) != own.as_ref() && (now - l.last_modified).num_seconds() < LOCK_FRESH_SECS) {
            // Ключ агентов не может удалять — снятая блокировка помечается `released`.
            let released = match self.i.c.get(&l.key).await {
                Ok(Some(b)) => serde_json::from_slice::<serde_json::Value>(&b).ok().and_then(|v| v.get("released").and_then(|r| r.as_bool())).unwrap_or(false),
                Ok(None) => true,
                Err(_) => false,
            };
            if released {
                if delete_stale { let _ = self.i.c.delete(&l.key).await; }
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Записать неизменяемый объект: повтор того же содержимого — не ошибка, другого — Conflict.
    async fn put_once(&self, key: &str, data: Vec<u8>, what: &str) -> Result<()> {
        if self.i.c.exists(key).await? {
            let old = self.i.c.get(key).await?.unwrap_or_default();
            if old.as_ref() == data.as_slice() { return Ok(()); }
            return Err(Conflict(what.to_string()).into());
        }
        self.i.c.put(key, Bytes::from(data), true).await
    }

    /// Заголовки всех снапшотов. Записи снапшотов неизменны, поэтому заголовок (и для очистки —
    /// ссылки, 32 байта на блок) кэшируется на диске после первого чтения. `strict` (для очистки):
    /// любой нечитаемый снапшот — ошибка, иначе блоки, на которые он ссылается, сочли бы мёртвыми.
    async fn heads(&self, strict: bool) -> Result<Vec<SnapshotHeader>> {
        let hdir = self.i.cache.join("heads");
        let rdir = self.i.cache.join("refs");
        std::fs::create_dir_all(&hdir)?;
        // Кэш прежней версии хранил записи целиком.
        let _ = std::fs::remove_dir_all(self.i.cache.join("snapshots"));
        let objs = self.i.c.list(&self.k("snapshots/"), false).await?;
        let ids: Vec<String> = objs.iter().filter_map(|o| o.key.rsplit('/').next().and_then(|n| n.strip_suffix(".json")).map(|s| s.to_string())).filter(|n| valid_name(n)).collect();
        let listed: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        for d in [&hdir, &rdir] {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    let id = n.rsplit_once('.').map(|x| x.0).unwrap_or(&n);
                    if !listed.contains(id) { let _ = std::fs::remove_file(e.path()); }
                }
            }
        }
        let mut out = Vec::new();
        let mut todo = Vec::new();
        for id in &ids {
            match std::fs::read(hdir.join(format!("{id}.json"))).ok().and_then(|b| serde_json::from_slice::<SnapshotHeader>(&b).ok()) {
                Some(h) => out.push(h),
                None => todo.push(id.clone()),
            }
        }
        // Запись снапшота велика (≈95 МБ на 1 ТБ источника) — не больше двух одновременно в памяти.
        let mut fetch = futures::stream::iter(todo).map(|id| async move { let r = self.fetch_snapshot(&id, strict).await; (id, r) }).buffer_unordered(2);
        while let Some((id, r)) = fetch.next().await {
            match r {
                Ok(Some(h)) => out.push(h),
                Ok(None) => {} // удалён во время чтения
                Err(e) if strict => return Err(e.context(format!("снапшот {id} — очистка остановлена, чтобы не удалить его блоки"))),
                Err(e) => tracing::warn!("снапшот {id}: {e:#}"),
            }
        }
        Ok(out)
    }

    /// Скачать запись снапшота, сохранить в кэш заголовок и (если `with_refs`) ссылки.
    async fn fetch_snapshot(&self, id: &str, with_refs: bool) -> Result<Option<SnapshotHeader>> {
        let Some(b) = self.i.c.get(&self.k(&format!("snapshots/{id}.json"))).await? else { return Ok(None) };
        let (hdir, rdir) = (self.i.cache.join("heads"), self.i.cache.join("refs"));
        let id = id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<SnapshotHeader>> {
            let mut refs: Vec<u8> = Vec::new();
            let sc = super::snapfile::scan(&b[..], |r| if with_refs { refs.extend_from_slice(&r.0) })?;
            drop(b);
            if with_refs {
                std::fs::create_dir_all(&rdir)?;
                let tmp = rdir.join(format!("{id}.tmp"));
                std::fs::write(&tmp, &refs)?;
                std::fs::rename(&tmp, rdir.join(format!("{id}.bin")))?;
            }
            crate::util::write_json(&hdir.join(format!("{id}.json")), &sc.header, false)?;
            Ok(Some(sc.header))
        })
        .await?
    }

    /// Ссылки снапшота — из кэша (32 байта на блок) или из записи на площадке.
    async fn refs_of(&self, id: &str, mut f: impl FnMut(ChunkId)) -> Result<()> {
        let p = self.i.cache.join("refs").join(format!("{id}.bin"));
        if !p.exists() && self.fetch_snapshot(id, true).await?.is_none() { bail!("снапшот {id} пропал во время очистки"); }
        let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(&p)?);
        let mut a = [0u8; 32];
        loop {
            match std::io::Read::read_exact(&mut r, &mut a) {
                Ok(()) => f(ChunkId(a)),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[async_trait]
impl Backend for S3Backend {
    async fn read_config(&self) -> Result<Option<RepoConfig>> {
        match self.i.c.get(&self.k("config.json")).await? {
            Some(b) => Ok(Some(serde_json::from_slice(&b)?)),
            None => Ok(None),
        }
    }

    async fn write_config(&self, cfg: &RepoConfig) -> Result<()> {
        let key = self.k("config.json");
        if self.i.c.exists(&key).await? { return Err(Conflict("репозиторий уже создан".into()).into()); }
        self.i.c.put(&key, Bytes::from(serde_json::to_vec_pretty(cfg)?), true).await
    }

    async fn missing(&self, ids: Vec<ChunkId>) -> Result<Vec<ChunkId>> {
        self.hold_write_lock().await?;
        let g = self.index().await?;
        let idx = g.as_ref().unwrap();
        Ok(ids.into_iter().filter(|id| !idx.contains(id)).collect())
    }

    async fn put_pack(&self, pack_id: &str, data: Bytes, entries: Vec<IndexEntry>) -> Result<()> {
        if blake3::hash(&data).to_hex().as_str() != pack_id { bail!("контрольная сумма pack-файла не совпадает"); }
        for e in &entries { if (e.offset as u64 + e.len as u64) > data.len() as u64 { bail!("запись индекса выходит за пределы pack-файла"); } }
        self.hold_write_lock().await?;
        // Сначала pack-файл, затем индекс: индекс никогда не ссылается на отсутствующие данные.
        self.i.c.put(&pack_key(&self.i.root, pack_id), data, true).await?;
        let f = IndexFile { pack: pack_id.to_string(), entries };
        let body = serde_json::to_vec(&f)?;
        self.i.c.put(&self.k(&format!("index/{pack_id}.json")), Bytes::from(body), true).await?;
        let dir = self.i.cache.join("index");
        std::fs::create_dir_all(&dir)?;
        crate::util::write_json(&dir.join(format!("{pack_id}.json")), &f, false)?;
        let mut g = self.index().await?;
        g.as_mut().unwrap().add_pack(pack_id, &f.entries)
    }

    async fn get_blob(&self, id: &ChunkId) -> Result<Vec<u8>> {
        let (pack, loc) = {
            let g = self.index().await?;
            let idx = g.as_ref().unwrap();
            let l = idx.get(id).ok_or_else(|| NotFound(format!("блок {}", id.to_hex())))?;
            (idx.pack_hex(l.pack), l)
        };
        Ok(self.i.c.get_range(&pack_key(&self.i.root, &pack), loc.offset as u64, loc.len as u64).await?.to_vec())
    }

    async fn put_snapshot(&self, rec: &SnapshotRecord) -> Result<()> {
        if !valid_name(&rec.header.id) { bail!("недопустимый идентификатор снапшота"); }
        {
            let g = self.index().await?;
            let idx = g.as_ref().unwrap();
            if !rec.refs.iter().all(|r| idx.contains(r)) { bail!("снапшот ссылается на блоки, которых нет на площадке"); }
        }
        let id = rec.header.id.clone();
        self.put_once(&self.k(&format!("snapshots/{id}.json")), serde_json::to_vec(rec)?, &format!("снапшот {id}")).await?;
        let hdir = self.i.cache.join("heads");
        let _ = std::fs::create_dir_all(&hdir);
        let _ = crate::util::write_json(&hdir.join(format!("{id}.json")), &rec.header, false);
        Ok(())
    }

    async fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord> {
        if !valid_name(id) { bail!("недопустимый идентификатор снапшота"); }
        match self.i.c.get(&self.k(&format!("snapshots/{id}.json"))).await? {
            Some(b) => Ok(serde_json::from_slice(&b)?),
            None => Err(NotFound(format!("снапшот {id}")).into()),
        }
    }

    async fn list_snapshots(&self, instance: Option<&str>) -> Result<Vec<SnapshotHeader>> {
        let mut v: Vec<SnapshotHeader> = self.heads(false).await?.into_iter().filter(|h| instance.map(|i| h.instance == i).unwrap_or(true)).collect();
        v.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.id.cmp(&b.id)));
        Ok(v)
    }

    async fn put_object(&self, ns: &str, name: &str, data: Vec<u8>) -> Result<()> {
        if !valid_ns(ns) || !valid_name(name) { bail!("недопустимое имя объекта"); }
        self.put_once(&self.k(&format!("objects/{ns}/{name}")), data, &format!("{ns}/{name}")).await
    }

    async fn get_object(&self, ns: &str, name: &str) -> Result<Option<Vec<u8>>> {
        if !valid_ns(ns) || !valid_name(name) { bail!("недопустимое имя объекта"); }
        Ok(self.i.c.get(&self.k(&format!("objects/{ns}/{name}"))).await?.map(|b| b.to_vec()))
    }

    async fn list_objects(&self, ns: &str, prefix: &str) -> Result<Vec<ObjInfo>> {
        if !valid_ns(ns) { bail!("недопустимое имя пространства"); }
        let dir = self.k(&format!("objects/{ns}/"));
        let objs = self.i.c.list(&format!("{dir}{prefix}"), true).await?;
        let mut v: Vec<ObjInfo> = objs.into_iter().filter_map(|o| o.key.strip_prefix(&dir).map(|n| ObjInfo { name: n.to_string(), size: o.size, mtime: o.last_modified.timestamp() })).filter(|o| !o.name.contains('/')).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn stats(&self) -> Result<RepoStats> {
        let chunks = { let g = self.index().await?; g.as_ref().unwrap().len() as u64 };
        let packs = self.i.c.list(&self.k("packs/"), false).await?;
        let snaps = self.i.c.list(&self.k("snapshots/"), false).await?;
        let objs = self.i.c.list(&self.k("objects/"), false).await?;
        Ok(RepoStats {
            packs: packs.len() as u64,
            pack_bytes: packs.iter().map(|o| o.size).sum(),
            chunks,
            snapshots: snaps.len() as u64,
            objects: objs.len() as u64,
            object_bytes: objs.iter().map(|o| o.size).sum(),
        })
    }

    async fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport> {
        let mut rep = PruneReport { dry_run: policy.dry_run, ..Default::default() };
        let mut all = self.heads(true).await?;
        all.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.id.cmp(&a.id)));
        let (ki, di) = super::prune::plan(&all, policy);
        let keep: Vec<SnapshotHeader> = ki.iter().map(|&i| all[i].clone()).collect();
        // Живые блоки — битовая карта по индексу (1 бит на блок); ссылки — по одному снапшоту.
        let mut g = self.index().await?;
        g.as_mut().unwrap().compact();
        let mut live = g.as_ref().unwrap().new_bitmap();
        for h in &keep {
            let idx = g.as_ref().unwrap();
            self.refs_of(&h.id, |r| if let Some(n) = idx.position(&r) { live.set(n) }).await.with_context(|| format!("снапшот {} — очистка остановлена, чтобы не удалить его блоки", h.id))?;
        }
        for &i in &di {
            let id = all[i].id.clone();
            rep.snapshots_removed.push(id.clone());
            if !policy.dry_run {
                self.i.c.delete(&self.k(&format!("snapshots/{id}.json"))).await?;
                let _ = std::fs::remove_file(self.i.cache.join("heads").join(format!("{id}.json")));
                let _ = std::fs::remove_file(self.i.cache.join("refs").join(format!("{id}.bin")));
            }
        }
        // Свежая блокировка записи — копия ещё пишется и может ссылаться на «мёртвые» pack-файлы.
        let now = chrono::Utc::now();
        let active = self.foreign_locks(!policy.dry_run).await?;
        let pack_objs = self.i.c.list(&self.k("packs/"), false).await?;
        let objinfo: HashMap<String, (chrono::DateTime<chrono::Utc>, u64)> = pack_objs.iter().filter_map(|o| o.key.rsplit('/').next().and_then(|n| n.strip_suffix(".pack")).map(|id| (id.to_string(), (o.last_modified, o.size)))).collect();
        drop(pack_objs);
        let mut dead: Vec<u32> = Vec::new();
        {
            let idx = g.as_ref().unwrap();
            let (any_live, any_dead) = idx.pack_liveness(&live);
            for no in 0..idx.pack_slots() as u32 {
                if !idx.pack_alive(no) { continue; }
                if any_live[no as usize] {
                    if any_dead[no as usize] { rep.packs_partial += 1; }
                    continue;
                }
                let young = objinfo.get(&idx.pack_hex(no)).map(|(t, _)| (now - *t).num_seconds() < 3600).unwrap_or(true);
                if active || young { continue; }
                dead.push(no);
            }
        }
        drop(live);
        // Вторая фаза: перед удалением ещё раз — не началась ли запись и не появились ли снапшоты,
        // ссылающиеся на кандидатов (копия, начатая между первым списком и этим моментом).
        if !policy.dry_run && !dead.is_empty() {
            drop(g);
            let known: HashSet<String> = all.iter().map(|h| h.id.clone()).collect();
            let fresh: Vec<SnapshotHeader> = self.heads(true).await?.into_iter().filter(|h| !known.contains(&h.id)).collect();
            if self.foreign_locks(false).await? {
                tracing::info!("очистка S3: началась запись — pack-файлы не удаляются");
                dead.clear();
            }
            g = self.index().await?;
            if !fresh.is_empty() && !dead.is_empty() {
                let idx = g.as_ref().unwrap();
                let mut is_dead = vec![false; idx.pack_slots()];
                for &d in &dead { is_dead[d as usize] = true; }
                for h in &fresh {
                    self.refs_of(&h.id, |r| if let Some(l) = idx.get(&r) { is_dead[l.pack as usize] = false }).await?;
                }
                dead.retain(|d| is_dead[*d as usize]);
            }
        }
        let names: Vec<String> = { let idx = g.as_ref().unwrap(); dead.iter().map(|&no| idx.pack_hex(no)).collect() };
        for pack in &names {
            rep.packs_removed += 1;
            rep.bytes_freed += objinfo.get(pack).map(|x| x.1).unwrap_or(0);
            if policy.dry_run { continue; }
            // Сначала индекс, затем данные: индекс никогда не ссылается на отсутствующий pack-файл.
            self.i.c.delete(&self.k(&format!("index/{pack}.json"))).await?;
            self.i.c.delete(&pack_key(&self.i.root, pack)).await?;
            let _ = std::fs::remove_file(self.i.cache.join("index").join(format!("{pack}.json")));
        }
        if !policy.dry_run { g.as_mut().unwrap().remove_packs(&dead); }
        drop(g);
        for (inst, start) in super::prune::wal_starts(keep.iter()) {
            let dir = self.k(&format!("objects/wal/{inst}/"));
            let objs = self.i.c.list(&dir, true).await?;
            for o in objs {
                let name = o.key.strip_prefix(&dir).unwrap_or("");
                if super::prune::wal_expired(name, &start) {
                    rep.wal_removed += 1;
                    if !policy.dry_run { self.i.c.delete(&o.key).await?; }
                }
            }
        }
        Ok(rep)
    }

    async fn list_packs(&self) -> Result<Vec<PackInfo>> {
        // Только pack-файлы с индексом: без индекса — запись не завершена (или прервана).
        let ix = self.i.c.list(&self.k("index/"), false).await?;
        let indexed: HashSet<String> = ix.iter().filter_map(|o| o.key.rsplit('/').next().and_then(|n| n.strip_suffix(".json")).map(|s| s.to_string())).collect();
        let objs = self.i.c.list(&self.k("packs/"), false).await?;
        Ok(objs.into_iter().filter_map(|o| o.key.rsplit('/').next().and_then(|n| n.strip_suffix(".pack")).map(|id| PackInfo { id: id.to_string(), size: o.size })).filter(|p| indexed.contains(&p.id)).collect())
    }

    async fn pack_index(&self, id: &str) -> Result<Vec<IndexEntry>> {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) { bail!("неверный идентификатор pack-файла"); }
        let idx = self.i.c.get(&self.k(&format!("index/{id}.json"))).await?.ok_or_else(|| NotFound(format!("индекс {id}")))?;
        Ok(serde_json::from_slice::<IndexFile>(&idx)?.entries)
    }

    async fn get_pack(&self, id: &str) -> Result<(Vec<IndexEntry>, Bytes)> {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) { bail!("неверный идентификатор pack-файла"); }
        let idx = self.i.c.get(&self.k(&format!("index/{id}.json"))).await?.ok_or_else(|| NotFound(format!("индекс {id}")))?;
        let f: IndexFile = serde_json::from_slice(&idx)?;
        let data = self.i.c.get(&pack_key(&self.i.root, id)).await?.ok_or_else(|| NotFound(format!("pack {id}")))?;
        if blake3::hash(&data).to_hex().as_str() != id { bail!("pack-файл {} повреждён (контрольная сумма)", &id[..12]); }
        Ok((f.entries, data))
    }

    async fn list_all_objects(&self) -> Result<Vec<NsObj>> {
        let base = self.k("objects/");
        let objs = self.i.c.list(&base, false).await?;
        Ok(objs.into_iter().filter_map(|o| {
            let rel = o.key.strip_prefix(&base)?;
            let (ns, name) = rel.rsplit_once('/')?;
            Some(NsObj { ns: ns.to_string(), obj: ObjInfo { name: name.to_string(), size: o.size, mtime: o.last_modified.timestamp() } })
        }).collect())
    }

    async fn release(&self) {
        let taken = self.i.write_lock.lock().take();
        if let Some((key, h)) = taken {
            h.abort();
            if self.i.c.delete(&key).await.is_err() {
                let body = serde_json::json!({ "host": crate::util::hostname(), "pid": std::process::id(), "released": true }).to_string();
                if let Err(e) = self.i.c.put(&key, Bytes::from(body), false).await { tracing::warn!("снятие блокировки записи S3: {e:#}"); }
            }
        }
    }
}

/// Проверка площадки S3: бакет доступен этими ключами.
pub async fn health(r: &super::Remote) -> Result<()> {
    let a = r.s3.as_ref().ok_or_else(|| anyhow!("у площадки S3 нет параметров доступа"))?;
    a.client()?.head_bucket().await
}
