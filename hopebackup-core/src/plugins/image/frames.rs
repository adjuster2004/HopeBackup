//! Кадры «сервер ↔ программа на машине» для образа машины по SSH.
//!
//! Программа на машине (`hopebackup disk-read`) читает со stdin карты прошлой копии (по 16-байтному хэшу
//! на блок, см. `write_prev_maps`), а на stdout выдаёт поток кадров: для каждого диска — заголовок, затем
//! по порядку номеров блоков — данные изменившихся блоков, пробеги нулевых и неизменных блоков,
//! предупреждения и итоговая статистика. Сервер читает кадры (`Reader`) и складывает блоки в репозиторий,
//! подставляя для неизменных блоков идентификаторы из прошлой карты (`repo::Block::Ref`).
//!
//! Всё — с порядком байт little-endian. Данные блоков по каналу не сжимаются: их сжимает репозиторий при
//! записи (zstd + шифрование), а транспорт SSH и так сжат.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};

/// Хэш блока для карты изменений (как в блочной реплике): усечённый BLAKE3, у нулевого блока — нули.
pub const HASH: usize = 16;

const MAGIC: &[u8; 8] = b"HBIMGFR2";
const PREV_MAGIC: &[u8; 8] = b"HBIMGPM1";

// Виды кадров.
const F_DISK: u8 = 1;
const F_DATA: u8 = 2;
const F_ZERO: u8 = 3; // пробег нулевых блоков
const F_REF: u8 = 4; // пробег неизменных блоков (брать из прошлой копии)
const F_WARN: u8 = 5;
const F_STAT: u8 = 6;
const F_END: u8 = 7;

/// Хэш блока: у полностью нулевого блока — нули (такой блок не передаётся и не хранится).
pub fn block_hash(b: &[u8]) -> [u8; HASH] {
    if b.iter().all(|x| *x == 0) { return [0; HASH]; }
    let h = blake3::hash(b);
    let mut o = [0u8; HASH];
    o.copy_from_slice(&h.as_bytes()[..HASH]);
    o
}

fn w_u32<W: Write>(w: &mut W, v: u32) -> Result<()> { w.write_all(&v.to_le_bytes())?; Ok(()) }
fn w_u64<W: Write>(w: &mut W, v: u64) -> Result<()> { w.write_all(&v.to_le_bytes())?; Ok(()) }
fn w_bytes<W: Write>(w: &mut W, b: &[u8]) -> Result<()> { w_u32(w, b.len() as u32)?; w.write_all(b)?; Ok(()) }

fn r_u8<R: Read>(r: &mut R) -> Result<Option<u8>> {
    let mut b = [0u8; 1];
    match r.read_exact(&mut b) { Ok(()) => Ok(Some(b[0])), Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None), Err(e) => Err(e.into()) }
}
fn r_u32<R: Read>(r: &mut R) -> Result<u32> { let mut b = [0u8; 4]; r.read_exact(&mut b)?; Ok(u32::from_le_bytes(b)) }
fn r_u64<R: Read>(r: &mut R) -> Result<u64> { let mut b = [0u8; 8]; r.read_exact(&mut b)?; Ok(u64::from_le_bytes(b)) }
fn r_bytes<R: Read>(r: &mut R, limit: usize) -> Result<Vec<u8>> {
    let n = r_u32(r)? as usize;
    if n > limit { bail!("кадр длиной {n} больше предела {limit}"); }
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(v)
}

/// Заголовок диска в потоке кадров.
#[derive(Clone, Debug)]
pub struct DiskHdr { pub name: String, pub size: u64, pub block: u32, pub blocks: u64 }

/// Что отдаёт `Reader`.
#[derive(Debug)]
pub enum Item {
    Disk(DiskHdr),
    /// Данные изменившегося блока.
    Data(Vec<u8>),
    /// Пробег нулевых блоков.
    Zero(u64),
    /// Пробег неизменных блоков (брать из прошлой копии).
    Ref(u64),
    Warn(String),
    Stat(serde_json::Value),
    End,
}

/// Запись кадров (на машине).
pub struct Writer<W: Write> { w: W }

impl<W: Write> Writer<W> {
    pub fn new(mut w: W) -> Result<Self> { w.write_all(MAGIC)?; Ok(Writer { w }) }
    pub fn disk(&mut self, h: &DiskHdr) -> Result<()> {
        self.w.write_all(&[F_DISK])?;
        w_bytes(&mut self.w, h.name.as_bytes())?;
        w_u64(&mut self.w, h.size)?;
        w_u32(&mut self.w, h.block)?;
        w_u64(&mut self.w, h.blocks)?;
        Ok(())
    }
    pub fn data(&mut self, b: &[u8]) -> Result<()> { self.w.write_all(&[F_DATA])?; w_bytes(&mut self.w, b)?; Ok(()) }
    pub fn zero_run(&mut self, n: u64) -> Result<()> { if n == 0 { return Ok(()); } self.w.write_all(&[F_ZERO])?; w_u64(&mut self.w, n) }
    pub fn ref_run(&mut self, n: u64) -> Result<()> { if n == 0 { return Ok(()); } self.w.write_all(&[F_REF])?; w_u64(&mut self.w, n) }
    pub fn warn(&mut self, m: &str) -> Result<()> { self.w.write_all(&[F_WARN])?; w_bytes(&mut self.w, m.as_bytes()) }
    pub fn stat(&mut self, v: &serde_json::Value) -> Result<()> { self.w.write_all(&[F_STAT])?; w_bytes(&mut self.w, serde_json::to_vec(v)?.as_slice()) }
    pub fn end(&mut self) -> Result<()> { self.w.write_all(&[F_END])?; self.w.flush()?; Ok(()) }
}

/// Чтение кадров (на сервере). Предел данных одного блока — 64 МиБ (блоки образа ≤ 4 МиБ, с запасом).
pub struct Reader<R: Read> { r: R, started: bool }

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Reader<R> { Reader { r, started: false } }

    pub fn next(&mut self) -> Result<Option<Item>> {
        if !self.started {
            let mut m = [0u8; 8];
            self.r.read_exact(&mut m).context("поток кадров пуст или программа на машине не запустилась")?;
            if &m != MAGIC { bail!("это не поток кадров образа"); }
            self.started = true;
        }
        let Some(k) = r_u8(&mut self.r)? else { return Ok(None) };
        Ok(Some(match k {
            F_DISK => {
                let name = String::from_utf8_lossy(&r_bytes(&mut self.r, 4096)?).to_string();
                let size = r_u64(&mut self.r)?;
                let block = r_u32(&mut self.r)?;
                let blocks = r_u64(&mut self.r)?;
                Item::Disk(DiskHdr { name, size, block, blocks })
            }
            F_DATA => Item::Data(r_bytes(&mut self.r, 64 << 20)?),
            F_ZERO => Item::Zero(r_u64(&mut self.r)?),
            F_REF => Item::Ref(r_u64(&mut self.r)?),
            F_WARN => Item::Warn(String::from_utf8_lossy(&r_bytes(&mut self.r, 1 << 20)?).to_string()),
            F_STAT => Item::Stat(serde_json::from_slice(&r_bytes(&mut self.r, 1 << 20)?).unwrap_or(serde_json::Value::Null)),
            F_END => Item::End,
            _ => bail!("неизвестный кадр {k}"),
        }))
    }
}

/// Карта прошлой копии диска: размер блока и по 16-байтному хэшу на блок.
#[derive(Clone, Debug, Default)]
pub struct PrevMap { pub block: u32, pub hashes: Vec<[u8; HASH]> }

impl PrevMap {
    pub fn get(&self, i: u64) -> Option<[u8; HASH]> { self.hashes.get(i as usize).copied() }
}

/// Записать карты прошлой копии (сервер → stdin машины): по диску — имя, размер блока, число блоков, хэши.
pub fn write_prev_maps<W: Write>(mut w: W, maps: &[(String, PrevMap)]) -> Result<()> {
    w.write_all(PREV_MAGIC)?;
    w_u32(&mut w, maps.len() as u32)?;
    for (name, m) in maps {
        w_bytes(&mut w, name.as_bytes())?;
        w_u32(&mut w, m.block)?;
        w_u64(&mut w, m.hashes.len() as u64)?;
        for h in &m.hashes { w.write_all(h)?; }
    }
    w.flush()?;
    Ok(())
}

/// Прочитать карты прошлой копии (на машине из stdin). Пустой ввод — карт нет (первая копия).
pub fn read_prev_maps<R: Read>(mut r: R) -> Result<std::collections::HashMap<String, PrevMap>> {
    let mut m = [0u8; 8];
    match r.read_exact(&mut m) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Default::default()), Err(e) => return Err(e.into()) }
    if &m != PREV_MAGIC { bail!("это не карты прошлой копии"); }
    let n = r_u32(&mut r)?;
    let mut out = std::collections::HashMap::new();
    for _ in 0..n {
        let name = String::from_utf8_lossy(&r_bytes(&mut r, 4096)?).to_string();
        let block = r_u32(&mut r)?;
        let cnt = r_u64(&mut r)?;
        if cnt > (1u64 << 34) { bail!("слишком большая карта: {cnt} блоков"); }
        let mut hashes = Vec::with_capacity(cnt as usize);
        for _ in 0..cnt { let mut h = [0u8; HASH]; r.read_exact(&mut h)?; hashes.push(h); }
        out.insert(name, PrevMap { block, hashes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let mut buf = Vec::new();
        {
            let mut w = Writer::new(&mut buf).unwrap();
            w.disk(&DiskHdr { name: "sda".into(), size: 100, block: 10, blocks: 10 }).unwrap();
            w.ref_run(2).unwrap();
            w.data(b"hello").unwrap();
            w.zero_run(3).unwrap();
            w.data(b"x").unwrap();
            w.warn("раздел /data читается на ходу").unwrap();
            w.stat(&serde_json::json!({ "changed": 2 })).unwrap();
            w.end().unwrap();
        }
        let mut r = Reader::new(&buf[..]);
        let mut items = Vec::new();
        while let Some(i) = r.next().unwrap() { let end = matches!(i, Item::End); items.push(i); if end { break; } }
        assert!(matches!(&items[0], Item::Disk(h) if h.name == "sda" && h.blocks == 10));
        assert!(matches!(&items[1], Item::Ref(2)));
        assert!(matches!(&items[2], Item::Data(d) if d == b"hello"));
        assert!(matches!(&items[3], Item::Zero(3)));
        assert!(matches!(&items[4], Item::Data(d) if d == b"x"));
        assert!(matches!(&items[5], Item::Warn(m) if m.contains("на ходу")));
        assert!(matches!(&items[6], Item::Stat(_)));
        assert!(matches!(&items[7], Item::End));
    }

    #[test]
    fn prev_maps_roundtrip() {
        let maps = vec![("sda".to_string(), PrevMap { block: 4096, hashes: vec![[1u8; HASH], [0u8; HASH]] })];
        let mut buf = Vec::new();
        write_prev_maps(&mut buf, &maps).unwrap();
        let got = read_prev_maps(&buf[..]).unwrap();
        assert_eq!(got["sda"].block, 4096);
        assert_eq!(got["sda"].hashes.len(), 2);
        assert_eq!(got["sda"].hashes[0], [1u8; HASH]);
        // Пустой ввод — карт нет.
        assert!(read_prev_maps(&[][..]).unwrap().is_empty());
    }

    #[test]
    fn hash_zero() {
        assert_eq!(block_hash(&[0u8; 100]), [0u8; HASH]);
        assert_ne!(block_hash(&[1u8; 100]), [0u8; HASH]);
    }
}
