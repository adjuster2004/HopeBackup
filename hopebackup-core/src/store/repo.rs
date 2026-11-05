//! Репозиторий: запись снапшотов с дедупликацией и чтение при восстановлении.
//!
//! Конвейер записи: поток → FastCDC (отдельный поток ОС) → keyed BLAKE3 → пакетный вопрос площадке
//! «каких блоков нет» → zstd + XChaCha20-Poly1305 (параллельно) → pack-файлы по 16 МиБ → площадка.
//! По каналу уходят только блоки, которых на площадке ещё нет.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use futures::StreamExt;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::Arc;

use super::backend::Backend;
use super::crypto::{self, Keys};
use super::types::*;

pub const PACK_TARGET: usize = 16 * 1024 * 1024;
/// Идентификатор нулевого блока образа: нули (такой блок не хранится и при восстановлении отдаётся нулями).
pub const ZERO_ID: ChunkId = ChunkId([0u8; 32]);
/// Пакет блоков на один вопрос площадке «каких нет»: до 64 блоков и не больше 32 МиБ данных —
/// столько агент держит в памяти между нарезкой и отправкой.
const BATCH_CHUNKS: usize = 64;
const BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Ёмкость буфера pack-файла: цель + самый большой блок после сжатия и шифрования.
const PACK_CAP: usize = PACK_TARGET + 4 * 1024 * 1024 + 64 * 1024;

pub struct Repo {
    pub backend: Arc<dyn Backend>,
    pub keys: Keys,
    pub cfg: RepoConfig,
    /// Уровень zstd: на узком канале выгоднее сжимать сильнее.
    pub level: i32,
    /// Задание, которое пишет снапшоты (попадает в заголовок: по нему сервер сверяет итог задания).
    pub job: String,
}

impl Repo {
    /// Создать репозиторий на площадке.
    pub async fn init(backend: Arc<dyn Backend>, password: &str) -> Result<Repo> {
        if password.len() < 8 { bail!("пароль репозитория должен быть не короче 8 символов"); }
        if backend.read_config().await?.is_some() { return Err(Conflict("репозиторий уже создан".into()).into()); }
        let keys = Keys::generate();
        let kdf = crypto::new_kdf_params();
        let id = uuid::Uuid::new_v4().to_string();
        let master_key = crypto::wrap_master(&keys, password, &kdf, &id)?;
        let cfg = RepoConfig { version: 1, id, created: Utc::now(), chunker: ChunkerParams::default(), kdf, master_key };
        backend.write_config(&cfg).await?;
        Ok(Repo { backend, keys, cfg, level: 3, job: String::new() })
    }

    /// Открыть существующий репозиторий паролем.
    pub async fn open(backend: Arc<dyn Backend>, password: &str) -> Result<Repo> {
        let cfg = backend.read_config().await?.ok_or_else(|| anyhow!("репозиторий на площадке не создан"))?;
        if cfg.version != 1 { bail!("неподдерживаемая версия репозитория {}", cfg.version); }
        let (w, p, k, id) = (cfg.master_key.clone(), password.to_string(), cfg.kdf.clone(), cfg.id.clone());
        // Argon2 — заметная работа процессора, не держим ей поток рантайма.
        let keys = tokio::task::spawn_blocking(move || crypto::unwrap_master(&w, &p, &k, &id)).await??;
        Ok(Repo { backend, keys, cfg, level: 3, job: String::new() })
    }

    /// Открыть репозиторий уже расшифрованным мастер-ключом (для restore_command: без Argon2 на каждый сегмент).
    pub async fn open_with_key(backend: Arc<dyn Backend>, master: [u8; 32]) -> Result<Repo> {
        let cfg = backend.read_config().await?.ok_or_else(|| anyhow!("репозиторий на площадке не создан"))?;
        Ok(Repo { backend, keys: Keys::from_master(master), cfg, level: 3, job: String::new() })
    }

    pub fn with_job(mut self, job: &str) -> Self {
        self.job = job.to_string();
        self
    }

    pub fn with_level(mut self, level: i32) -> Self {
        self.level = level.clamp(0, 19);
        self
    }

    pub fn writer(self: &Arc<Self>) -> SnapshotWriter {
        SnapshotWriter { repo: self.clone(), pack: Vec::with_capacity(PACK_CAP), entries: Vec::new(), seen: HashSet::new(), streams: Vec::new(), images: Vec::new(), stats: WriteStats::default() }
    }

    /// Прочитать карту блоков образа (идентификаторы по 32 байта; нулевой — нулевой блок).
    pub async fn read_image_map(&self, img: &ImageEntry) -> Result<Vec<ChunkId>> {
        let buf = Arc::new(parking_lot::Mutex::new(Vec::with_capacity(img.blocks as usize * 32)));
        let b2 = buf.clone();
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        self.restore_stream(&img.map, Sink(b2)).await?;
        let raw = Arc::try_unwrap(buf).map_err(|_| anyhow!("карта блоков занята")).map(|m| m.into_inner()).unwrap_or_default();
        if raw.len() as u64 != img.blocks * 32 { bail!("карта блоков образа {} повреждена: {} байт при {} блоках", img.name, raw.len(), img.blocks); }
        Ok(raw.chunks_exact(32).map(|c| { let mut a = [0u8; 32]; a.copy_from_slice(c); ChunkId(a) }).collect())
    }

    /// Разреженная выдача образа: данные блоков и пробеги нулей по порядку (для записи в файл с «дырами»
    /// или в том гипервизора без передачи нулей).
    pub async fn restore_image_sparse(&self, img: &ImageEntry, sink: Box<dyn ImageSink>) -> Result<u64> {
        let ids = self.read_image_map(img).await?;
        let (n, block, size) = (ids.len() as u64, img.block as u64, img.size);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, Option<Vec<u8>>, u64)>(16);
        let writer = std::thread::spawn(move || -> Result<u64> {
            let mut sink = sink;
            let mut w = 0u64;
            while let Some((off, data, len)) = rx.blocking_recv() {
                match data { Some(b) => { sink.data(off, &b)?; w += b.len() as u64; } None => { sink.zero(off, len)?; w += len; } }
            }
            sink.finish()?;
            Ok(w)
        });
        let keys = self.keys.clone();
        let backend = self.backend.clone();
        let mut fetch = futures::stream::iter(ids.into_iter().enumerate())
            .map(|(i, id)| {
                let backend = backend.clone();
                let keys = keys.clone();
                let off = i as u64 * block;
                let len = if i as u64 == n - 1 && n > 0 { size - (n - 1) * block } else { block };
                async move {
                    if id == ZERO_ID { return Ok((off, None, len)); }
                    let blob = backend.get_blob(&id).await?;
                    let b = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                        let pt = crypto::open(&keys.data, &id.0, &blob)?;
                        if keys.chunk_id(&pt) != id { bail!("блок образа {id:?} повреждён"); }
                        Ok(pt)
                    }).await??;
                    Ok::<_, anyhow::Error>((off, Some(b), len))
                }
            })
            .buffered(8);
        let mut err = None;
        while let Some(r) = fetch.next().await {
            match r { Ok(m) => { if tx.send(m).await.is_err() { err = Some(anyhow!("получатель образа закрыт")); break; } } Err(e) => { err = Some(e); break; } }
        }
        drop(tx);
        drop(fetch);
        let w = tokio::task::spawn_blocking(move || writer.join()).await?.map_err(|_| anyhow!("поток записи образа аварийно завершился"))?;
        if let Some(e) = err { return Err(e); }
        Ok(w?)
    }

    /// Восстановить образ диска в `out` блоками по порядку (нулевые блоки — нулями, без скачивания).
    pub async fn restore_image<W: Write + Send + 'static>(&self, img: &ImageEntry, out: W) -> Result<u64> {
        let ids = self.read_image_map(img).await?;
        let (n, block, size) = (ids.len() as u64, img.block as u64, img.size);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let writer = std::thread::spawn(move || -> std::io::Result<u64> {
            let mut out = out;
            let mut w = 0u64;
            while let Some(b) = rx.blocking_recv() { out.write_all(&b)?; w += b.len() as u64; }
            out.flush()?;
            Ok(w)
        });
        let keys = self.keys.clone();
        let backend = self.backend.clone();
        let mut fetch = futures::stream::iter(ids.into_iter().enumerate())
            .map(|(i, id)| {
                let backend = backend.clone();
                let keys = keys.clone();
                let len = if i as u64 == n - 1 && n > 0 { (size - (n - 1) * block) as usize } else { block as usize };
                async move {
                    if id == ZERO_ID { return Ok(vec![0u8; len]); }
                    let blob = backend.get_blob(&id).await?;
                    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                        let pt = crypto::open(&keys.data, &id.0, &blob)?;
                        if keys.chunk_id(&pt) != id { bail!("блок образа {id:?} повреждён"); }
                        Ok(pt)
                    }).await?
                }
            })
            .buffered(8);
        let mut err = None;
        while let Some(r) = fetch.next().await {
            match r { Ok(b) => { if tx.send(b).await.is_err() { err = Some(anyhow!("получатель образа закрыт")); break; } } Err(e) => { err = Some(e); break; } }
        }
        drop(tx);
        drop(fetch);
        let w = tokio::task::spawn_blocking(move || writer.join()).await?.map_err(|_| anyhow!("поток записи образа аварийно завершился"))?;
        if let Some(e) = err { return Err(e); }
        Ok(w?)
    }

    /// Расшифровать тело снапшота.
    pub async fn manifest(&self, id: &str) -> Result<(SnapshotHeader, Manifest)> {
        let rec = self.backend.get_snapshot(id).await?;
        let blob = base64::engine::general_purpose::STANDARD.decode(&rec.body)?;
        let pt = crypto::open(&self.keys.meta, rec.header.id.as_bytes(), &blob).context("тело снапшота")?;
        Ok((rec.header, serde_json::from_slice(&pt)?))
    }

    /// Прочитать поток снапшота в `out` (в отдельном потоке ОС). Блоки скачиваются по 8 параллельно,
    /// каждый проверяется по идентификатору после расшифровки.
    pub async fn restore_stream<W: Write + Send + 'static>(&self, s: &StreamEntry, out: W) -> Result<u64> {
        // Канал и ожидание записи — без блокировки потоков рантайма: получатель потока (запись в imageio и
        // т. п.) может сам ждать задач рантайма, и заблокированный рабочий поток привёл бы к взаимной блокировке.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let writer = std::thread::spawn(move || -> std::io::Result<u64> {
            let mut out = out;
            let mut n = 0u64;
            while let Some(b) = rx.blocking_recv() {
                out.write_all(&b)?;
                n += b.len() as u64;
            }
            out.flush()?;
            Ok(n)
        });
        let keys = self.keys.clone();
        let backend = self.backend.clone();
        let mut fetch = futures::stream::iter(s.chunks.clone())
            .map(|id| {
                let backend = backend.clone();
                let keys = keys.clone();
                async move {
                    let blob = backend.get_blob(&id).await?;
                    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                        let pt = crypto::open(&keys.data, &id.0, &blob)?;
                        if keys.chunk_id(&pt) != id { bail!("блок {id:?} повреждён: содержимое не совпадает с идентификатором"); }
                        Ok(pt)
                    })
                    .await?
                }
            })
            .buffered(8);
        let mut err = None;
        while let Some(r) = fetch.next().await {
            match r {
                Ok(b) => {
                    if tx.send(b).await.is_err() { err = Some(anyhow!("получатель потока закрыт")); break; }
                }
                Err(e) => { err = Some(e); break; }
            }
        }
        drop(tx);
        drop(fetch);
        let n = tokio::task::spawn_blocking(move || writer.join()).await?.map_err(|_| anyhow!("поток записи аварийно завершился"))?;
        if let Some(e) = err { return Err(e); }
        Ok(n?)
    }

    /// Поток снапшота как `Read` (для распаковки tar на лету).
    pub fn stream_reader(self: &Arc<Self>, s: &StreamEntry) -> Result<(std::io::PipeReader, tokio::task::JoinHandle<Result<u64>>)> {
        let (r, w) = std::io::pipe()?;
        let repo = self.clone();
        let s = s.clone();
        let h = tokio::spawn(async move { repo.restore_stream(&s, w).await });
        Ok((r, h))
    }

    /// Положить объект (сегмент WAL и т. п.) — сжатым и зашифрованным.
    pub async fn put_object(&self, ns: &str, name: &str, data: Vec<u8>) -> Result<u64> {
        let aad = format!("{ns}/{name}");
        let (k, lvl) = (self.keys.data, self.level);
        let (blob, data) = tokio::task::spawn_blocking(move || { let b = crypto::seal(&k, aad.as_bytes(), &data, lvl); (b, data) }).await?;
        let n = blob.len() as u64;
        match self.backend.put_object(ns, name, blob).await {
            Ok(()) => Ok(n),
            // Повторная отправка того же сегмента (сбой после записи): шифротекст другой из-за случайного
            // nonce, поэтому сравниваем открытое содержимое.
            Err(e) if e.downcast_ref::<Conflict>().is_some() => match self.get_object(ns, name).await? {
                Some(old) if old == data => Ok(0),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn get_object(&self, ns: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(blob) = self.backend.get_object(ns, name).await? else { return Ok(None) };
        let aad = format!("{ns}/{name}");
        Ok(Some(crypto::open(&self.keys.data, aad.as_bytes(), &blob)?))
    }
}

/// Приёмник разреженной выдачи образа (запись в файл или том гипервизора).
pub trait ImageSink: Send {
    /// Данные по смещению.
    fn data(&mut self, off: u64, b: &[u8]) -> Result<()>;
    /// Пробел нулей по смещению (можно оставить «дырой» в файле / не передавать в том).
    fn zero(&mut self, off: u64, len: u64) -> Result<()>;
    fn finish(&mut self) -> Result<()> { Ok(()) }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WriteStats {
    /// Прочитано из источника, байт.
    pub bytes_read: u64,
    pub chunks: u64,
    pub chunks_new: u64,
    /// Новые данные до сжатия.
    pub new_bytes: u64,
    /// Ушло по каналу (после сжатия и шифрования).
    pub uploaded: u64,
}

pub struct SnapshotWriter {
    repo: Arc<Repo>,
    pack: Vec<u8>,
    entries: Vec<IndexEntry>,
    seen: HashSet<ChunkId>,
    streams: Vec<StreamEntry>,
    images: Vec<ImageEntry>,
    pub stats: WriteStats,
}

/// Блок образа для записи фиксированными блоками (`begin_fixed`/`fixed_push`/`fixed_end`).
pub enum Block<'a> {
    /// Данные блока: идентификатор считается по содержимому, блок дедуплицируется и хранится, если нов.
    Data(&'a [u8]),
    /// Блок не изменился с прошлой копии: идентификатор известен из прошлой карты, данные не передаются
    /// (блок уже в хранилище). Так в каждую следующую копию уходят только изменившиеся блоки.
    Ref(ChunkId),
    /// Нулевой блок: не хранится и не передаётся.
    Zero,
}

/// Состояние записи одного образа фиксированными блоками.
pub struct FixedState {
    size: u64,
    block: u32,
    ids: Vec<ChunkId>,
    batch: Vec<(ChunkId, Vec<u8>)>,
    batch_bytes: usize,
}

impl SnapshotWriter {
    /// Записать поток под именем `name`. `progress` вызывается после каждого пакета блоков.
    pub async fn add_stream<R: Read + Send + 'static>(&mut self, name: &str, reader: R, progress: Option<&(dyn Fn(&WriteStats) + Send + Sync)>) -> Result<StreamEntry> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<(ChunkId, Vec<u8>)>>(8);
        let keys = self.repo.keys.clone();
        let p = self.repo.cfg.chunker.clone();
        let chunker = std::thread::spawn(move || {
            let cdc = fastcdc::v2020::StreamCDC::with_level_and_seed(reader, p.min, p.avg, p.max, fastcdc::v2020::Normalization::Level1, keys.cdc_seed);
            for r in cdc {
                let msg = match r {
                    Ok(c) => Ok((keys.chunk_id(&c.data), c.data)),
                    Err(e) => Err(std::io::Error::other(e.to_string())),
                };
                let stop = msg.is_err();
                if tx.blocking_send(msg).is_err() || stop { break; }
            }
        });
        let mut entry = StreamEntry { name: name.to_string(), size: 0, chunks: Vec::new() };
        let mut batch: Vec<(ChunkId, Vec<u8>)> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut failure = None;
        while let Some(m) = rx.recv().await {
            match m {
                Ok(c) => {
                    batch_bytes += c.1.len();
                    batch.push(c);
                    if batch.len() >= BATCH_CHUNKS || batch_bytes >= BATCH_BYTES {
                        batch_bytes = 0;
                        if let Err(e) = self.process(&mut batch, &mut entry).await { failure = Some(e); break; }
                        if let Some(p) = progress { p(&self.stats); }
                    }
                }
                Err(e) => { failure = Some(anyhow!("чтение потока {name}: {e}")); break; }
            }
        }
        drop(rx);
        let _ = chunker.join();
        if let Some(e) = failure { return Err(e); }
        self.process(&mut batch, &mut entry).await?;
        if let Some(p) = progress { p(&self.stats); }
        self.streams.push(entry.clone());
        Ok(entry)
    }

    async fn process(&mut self, batch: &mut Vec<(ChunkId, Vec<u8>)>, entry: &mut StreamEntry) -> Result<()> {
        if batch.is_empty() { return Ok(()); }
        let items = std::mem::take(batch);
        let mut ask = Vec::new();
        let mut ask_set = HashSet::new();
        for (id, data) in &items {
            entry.size += data.len() as u64;
            entry.chunks.push(*id);
            self.stats.bytes_read += data.len() as u64;
            self.stats.chunks += 1;
            if !self.seen.contains(id) && ask_set.insert(*id) { ask.push(*id); }
        }
        let missing: HashSet<ChunkId> = if ask.is_empty() { HashSet::new() } else { self.repo.backend.missing(ask.clone()).await?.into_iter().collect() };
        self.seen.extend(ask);
        let mut todo = Vec::new();
        let mut taken = HashSet::new();
        for (id, data) in items {
            if missing.contains(&id) && taken.insert(id) { todo.push((id, data)); }
        }
        if todo.is_empty() { return Ok(()); }
        let (k, lvl) = (self.repo.keys.data, self.repo.level);
        let sealed: Vec<(ChunkId, Vec<u8>, usize)> = tokio::task::spawn_blocking(move || {
            todo.into_par_iter().map(|(id, d)| { let b = crypto::seal(&k, &id.0, &d, lvl); (id, b, d.len()) }).collect()
        })
        .await?;
        for (id, blob, plain) in sealed {
            self.stats.chunks_new += 1;
            self.stats.new_bytes += plain as u64;
            self.entries.push(IndexEntry { id, offset: self.pack.len() as u32, len: blob.len() as u32 });
            self.pack.extend_from_slice(&blob);
            if self.pack.len() >= PACK_TARGET { self.flush().await?; }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.entries.is_empty() { return Ok(()); }
        let data = std::mem::replace(&mut self.pack, Vec::with_capacity(PACK_CAP));
        let entries = std::mem::take(&mut self.entries);
        let id = blake3::hash(&data).to_hex().to_string();
        self.stats.uploaded += data.len() as u64;
        self.repo.backend.put_pack(&id, bytes::Bytes::from(data), entries).await
    }

    // ------------------------------------------------------------ образы: фиксированные блоки
    //
    // Диск записывается блоками фиксированного размера: каждый блок адресуется номером, а не содержимым.
    // Данные блока дедуплицируются как обычные блоки репозитория (пакетный вопрос «каких нет», zstd +
    // шифрование, pack-файлы). Неизменные с прошлой копии блоки передаются как `Block::Ref(id)` — их данные
    // уже на площадке, уходит только идентификатор. Карта блоков образа сохраняется отдельным потоком.

    /// Начать запись образа диска размером `size` блоками по `block` байт.
    pub fn begin_fixed(&mut self, size: u64, block: u32) -> FixedState {
        FixedState { size, block, ids: Vec::with_capacity(size.div_ceil(block as u64) as usize), batch: Vec::new(), batch_bytes: 0 }
    }

    /// Добавить очередной блок образа.
    pub async fn fixed_push(&mut self, st: &mut FixedState, b: Block<'_>) -> Result<()> {
        self.stats.chunks += 1;
        match b {
            Block::Zero => st.ids.push(ZERO_ID),
            Block::Ref(id) => {
                if id == ZERO_ID { st.ids.push(ZERO_ID); return Ok(()); }
                // Блок уже на площадке (из прошлой копии) — держим его живым ссылкой снапшота.
                self.seen.insert(id);
                st.ids.push(id);
            }
            Block::Data(d) => {
                let id = self.repo.keys.chunk_id(d);
                st.ids.push(id);
                self.stats.bytes_read += d.len() as u64;
                st.batch_bytes += d.len();
                st.batch.push((id, d.to_vec()));
                if st.batch.len() >= BATCH_CHUNKS || st.batch_bytes >= BATCH_BYTES { self.flush_fixed(st).await?; }
            }
        }
        Ok(())
    }

    /// Пакетно спросить площадку о недостающих блоках и сохранить их (как `process`, но блоки уже нарезаны).
    async fn flush_fixed(&mut self, st: &mut FixedState) -> Result<()> {
        if st.batch.is_empty() { return Ok(()); }
        let items = std::mem::take(&mut st.batch);
        st.batch_bytes = 0;
        let mut ask = Vec::new();
        let mut ask_set = HashSet::new();
        for (id, _) in &items { if !self.seen.contains(id) && ask_set.insert(*id) { ask.push(*id); } }
        let missing: HashSet<ChunkId> = if ask.is_empty() { HashSet::new() } else { self.repo.backend.missing(ask.clone()).await?.into_iter().collect() };
        self.seen.extend(ask);
        let mut todo = Vec::new();
        let mut taken = HashSet::new();
        for (id, data) in items { if missing.contains(&id) && taken.insert(id) { todo.push((id, data)); } }
        if todo.is_empty() { return Ok(()); }
        let (k, lvl) = (self.repo.keys.data, self.repo.level);
        let sealed: Vec<(ChunkId, Vec<u8>, usize)> = tokio::task::spawn_blocking(move || {
            todo.into_par_iter().map(|(id, d)| { let b = crypto::seal(&k, &id.0, &d, lvl); (id, b, d.len()) }).collect()
        }).await?;
        for (id, blob, plain) in sealed {
            self.stats.chunks_new += 1;
            self.stats.new_bytes += plain as u64;
            self.entries.push(IndexEntry { id, offset: self.pack.len() as u32, len: blob.len() as u32 });
            self.pack.extend_from_slice(&blob);
            if self.pack.len() >= PACK_TARGET { self.flush().await?; }
        }
        Ok(())
    }

    /// Завершить образ: дослать блоки, сохранить карту блоков потоком и вернуть запись образа.
    pub async fn fixed_end(&mut self, name: &str, mut st: FixedState) -> Result<ImageEntry> {
        self.flush_fixed(&mut st).await?;
        let mut mapbytes = Vec::with_capacity(st.ids.len() * 32);
        for id in &st.ids { mapbytes.extend_from_slice(&id.0); }
        let map = self.add_stream(&format!("{name}.blocks"), std::io::Cursor::new(mapbytes), None).await?;
        let img = ImageEntry { name: name.to_string(), size: st.size, block: st.block, blocks: st.ids.len() as u64, map };
        self.images.push(img.clone());
        Ok(img)
    }

    /// Дописать последний pack-файл и сохранить снапшот.
    pub async fn finish(mut self, instance: &str, product: &str, kind: &str, extra: serde_json::Value, meta: serde_json::Value) -> Result<(SnapshotHeader, WriteStats)> {
        self.flush().await?;
        let id = format!("{}-{}", Utc::now().format("%Y%m%dT%H%M%SZ"), &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let manifest = Manifest { streams: self.streams.clone(), images: self.images.clone(), meta };
        let body = crypto::seal(&self.repo.keys.meta, id.as_bytes(), &serde_json::to_vec(&manifest)?, 3);
        // Уникальные блоки снапшота — ровно те, о которых спрашивали площадку.
        let mut refs: Vec<ChunkId> = std::mem::take(&mut self.seen).into_iter().collect();
        refs.sort_unstable();
        let header = SnapshotHeader {
            id,
            time: Utc::now(),
            instance: instance.to_string(),
            product: product.to_string(),
            kind: kind.to_string(),
            host: crate::util::hostname(),
            // Образы учитываются по размеру диска, а не по размеру служебной карты блоков.
            size: self.images.iter().map(|i| i.size).sum::<u64>() + self.streams.iter().filter(|s| !self.images.iter().any(|i| i.map.name == s.name)).map(|s| s.size).sum::<u64>(),
            new_bytes: self.stats.new_bytes,
            uploaded: self.stats.uploaded + body.len() as u64,
            chunks: self.stats.chunks,
            extra: {
                let mut e = extra;
                if !self.repo.job.is_empty() { if let Some(o) = e.as_object_mut() { o.insert("job".into(), serde_json::json!(self.repo.job)); } }
                e
            },
        };
        let rec = SnapshotRecord { header: header.clone(), refs, body: base64::engine::general_purpose::STANDARD.encode(body) };
        self.repo.backend.put_snapshot(&rec).await?;
        Ok((header, self.stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::local::LocalBackend;
    use rand::{RngCore, SeedableRng};

    fn data(n: usize, seed: u64) -> Vec<u8> {
        let mut r = rand::rngs::StdRng::seed_from_u64(seed);
        let mut v = vec![0u8; n];
        r.fill_bytes(&mut v);
        v
    }

    #[tokio::test]
    async fn dedup_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let be: Arc<dyn Backend> = Arc::new(LocalBackend::open(dir.path()).unwrap());
        let repo = Arc::new(Repo::init(be.clone(), "пароль-репо").await.unwrap());
        let a = data(20 * 1024 * 1024, 1);
        let mut w = repo.writer();
        w.add_stream("a", std::io::Cursor::new(a.clone()), None).await.unwrap();
        let (h1, s1) = w.finish("i1", "test", "full", serde_json::json!({}), serde_json::json!({})).await.unwrap();
        assert_eq!(s1.bytes_read, a.len() as u64);
        assert_eq!(s1.new_bytes, a.len() as u64);

        // Вставка в середину: заново уходят только соседние блоки.
        let mut b = a.clone();
        let ins = data(4096, 2);
        b.splice(10_000_000..10_000_000, ins);
        let repo2 = Arc::new(Repo::open(be.clone(), "пароль-репо").await.unwrap());
        let mut w = repo2.writer();
        w.add_stream("a", std::io::Cursor::new(b.clone()), None).await.unwrap();
        let (h2, s2) = w.finish("i1", "test", "full", serde_json::json!({}), serde_json::json!({})).await.unwrap();
        assert!(s2.new_bytes < 10 * 1024 * 1024 / 2, "должно уйти мало данных, ушло {}", s2.new_bytes);

        for (h, orig) in [(h1, a), (h2, b)] {
            let (_, m) = repo2.manifest(&h.id).await.unwrap();
            let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
            struct W(Arc<parking_lot::Mutex<Vec<u8>>>);
            impl Write for W {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) }
                fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
            }
            repo2.restore_stream(&m.streams[0], W(out.clone())).await.unwrap();
            assert!(*out.lock() == orig);
        }
        assert!(Repo::open(be.clone(), "не тот пароль").await.is_err());
    }

    /// Очистка на узле: заголовки из heads/, ссылки потоком, битовая карта живых блоков; приём записи
    /// снапшота из файла (как узел принимает её по сети).
    #[tokio::test]
    async fn prune_and_snapshot_file() {
        let dir = tempfile::tempdir().unwrap();
        let be: Arc<dyn Backend> = Arc::new(LocalBackend::open(dir.path()).unwrap());
        let repo = Arc::new(Repo::init(be.clone(), "пароль-репо").await.unwrap());
        let shared = data(3 * 1024 * 1024, 9);
        let mut ids = Vec::new();
        for k in 0..3u64 {
            let mut d = shared.clone();
            d.extend(data(6 * 1024 * 1024, 100 + k));
            let mut w = repo.writer();
            w.add_stream("a", std::io::Cursor::new(d), None).await.unwrap();
            ids.push(w.finish("i1", "test", "full", serde_json::json!({}), serde_json::json!({})).await.unwrap().0.id);
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // разное время снапшотов
        }
        // Заново открыть (как после перезапуска узла) и состарить pack-файлы.
        let lb = LocalBackend::open(dir.path()).unwrap();
        assert_eq!(lb.list_snapshots(None).await.unwrap().len(), 3);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        for d in std::fs::read_dir(dir.path().join("packs")).unwrap().flatten() {
            for f in std::fs::read_dir(d.path()).unwrap().flatten() { std::fs::File::options().write(true).open(f.path()).unwrap().set_modified(old).unwrap(); }
        }
        let packs_before = lb.stats().await.unwrap().packs;
        let pol = PrunePolicy { keep_last: 1, keep_days: None, dry_run: false, per_instance: Default::default(), keep_unknown: false };
        let r = lb.prune(&pol).await.unwrap();
        assert_eq!(r.snapshots_removed.len(), 2);
        assert!(r.packs_removed > 0 && lb.stats().await.unwrap().packs < packs_before, "{r:?}");
        let left = lb.list_snapshots(None).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, ids[2]);
        assert_eq!(std::fs::read_dir(dir.path().join("heads")).unwrap().count(), 1);
        // Оставшийся снапшот восстанавливается и после повторного открытия.
        let be2: Arc<dyn Backend> = Arc::new(LocalBackend::open(dir.path()).unwrap());
        let repo2 = Arc::new(Repo::open(be2.clone(), "пароль-репо").await.unwrap());
        let (_, m) = repo2.manifest(&ids[2]).await.unwrap();
        repo2.restore_stream(&m.streams[0], std::io::sink()).await.unwrap();
        // Приём записи из файла: тот же снапшот — без ошибки, изменённый — конфликт, чужие блоки — отказ.
        let lb2 = LocalBackend::open(dir.path()).unwrap();
        let rec = lb2.get_snapshot(&ids[2]).await.unwrap();
        let put = |rec: &SnapshotRecord| { let t = lb2.tmp_file(); std::fs::write(&t, serde_json::to_vec(rec).unwrap()).unwrap(); lb2.put_snapshot_file(t, rec.header.id.clone()) };
        put(&rec).await.unwrap();
        let mut changed = rec.clone();
        changed.header.size += 1;
        assert!(put(&changed).await.unwrap_err().downcast_ref::<Conflict>().is_some());
        let mut alien = rec.clone();
        alien.header.id = "alien".into();
        alien.refs.push(ChunkId([7; 32]));
        assert!(put(&alien).await.is_err());
        assert!(std::fs::read_dir(dir.path().join("tmp")).unwrap().next().is_none(), "временные файлы убраны");
    }

    /// Образ фиксированными блоками: запись, дедупликация неизменных блоков через Ref, восстановление.
    #[tokio::test]
    async fn image_fixed_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let be: Arc<dyn Backend> = Arc::new(LocalBackend::open(dir.path()).unwrap());
        let repo = Arc::new(Repo::init(be.clone(), "пароль-репо").await.unwrap());
        let block = 4 << 20u32;
        let size = block as u64 * 3 + 123; // три полных блока и короткий хвост
        let b0 = data(block as usize, 1);
        let b1 = data(block as usize, 2);
        let tail = data(123, 3);
        // Первая копия: блок 0 — данные, блок 1 — данные, блок 2 — нули, хвост — данные.
        let mut w = repo.writer();
        let mut st = w.begin_fixed(size, block);
        w.fixed_push(&mut st, Block::Data(&b0)).await.unwrap();
        w.fixed_push(&mut st, Block::Data(&b1)).await.unwrap();
        w.fixed_push(&mut st, Block::Zero).await.unwrap();
        w.fixed_push(&mut st, Block::Data(&tail)).await.unwrap();
        let img1 = w.fixed_end("sda", st).await.unwrap();
        let (h1, _) = w.finish("m1", "machine", "full", serde_json::json!({}), serde_json::json!({})).await.unwrap();
        assert_eq!(h1.size, size);
        let expect: Vec<u8> = [&b0[..], &b1[..], &vec![0u8; block as usize], &tail[..]].concat();

        // Восстановление образа.
        let (_, m1) = repo.manifest(&h1.id).await.unwrap();
        let img1r = &m1.images[0];
        let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
        struct W(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl Write for W { fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) } fn flush(&mut self) -> std::io::Result<()> { Ok(()) } }
        let n = repo.restore_image(img1r, W(out.clone())).await.unwrap();
        assert_eq!(n, size);
        assert!(*out.lock() == expect);

        // Вторая копия: блок 1 изменился, блок 0 — тот же (передаём Ref на старый id), остальное без изменений.
        let ids = repo.read_image_map(&img1).await.unwrap();
        let b1b = data(block as usize, 22);
        let mut w = repo.writer();
        let mut st = w.begin_fixed(size, block);
        w.fixed_push(&mut st, Block::Ref(ids[0])).await.unwrap();
        w.fixed_push(&mut st, Block::Data(&b1b)).await.unwrap();
        w.fixed_push(&mut st, Block::Zero).await.unwrap();
        w.fixed_push(&mut st, Block::Ref(ids[3])).await.unwrap();
        w.fixed_end("sda", st).await.unwrap();
        let (h2, s2) = w.finish("m1", "machine", "full", serde_json::json!({}), serde_json::json!({})).await.unwrap();
        // Новыми уходят только изменившийся блок и крошечная карта блоков — не блоки 0/2/3.
        assert!(s2.new_bytes >= block as u64 && s2.new_bytes < block as u64 * 2, "новым должен быть только изменённый блок, ушло {}", s2.new_bytes);
        let expect2: Vec<u8> = [&b0[..], &b1b[..], &vec![0u8; block as usize], &tail[..]].concat();
        let (_, m2) = repo.manifest(&h2.id).await.unwrap();
        let out2 = Arc::new(parking_lot::Mutex::new(Vec::new()));
        repo.restore_image(&m2.images[0], W(out2.clone())).await.unwrap();
        assert!(*out2.lock() == expect2);
    }

    #[tokio::test]
    async fn objects_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let be: Arc<dyn Backend> = Arc::new(LocalBackend::open(dir.path()).unwrap());
        let repo = Repo::init(be.clone(), "пароль-репо").await.unwrap();
        repo.put_object("wal/i1", "000000010000000000000001", b"segment".to_vec()).await.unwrap();
        assert_eq!(repo.get_object("wal/i1", "000000010000000000000001").await.unwrap().unwrap(), b"segment");
        assert!(repo.get_object("wal/i1", "nope").await.unwrap().is_none());
        // Повтор с другим содержимым — отказ (репозиторий только дописывается).
        assert!(repo.put_object("wal/i1", "000000010000000000000001", b"other".to_vec()).await.is_err());
    }
}
