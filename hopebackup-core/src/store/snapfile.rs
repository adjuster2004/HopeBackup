//! Потоковый разбор записи снапшота: заголовок и ссылки на блоки без загрузки файла в память.
//!
//! Запись снапшота растёт с объёмом источника (≈95 МБ на 1 ТБ: список ссылок и зашифрованное тело).
//! Площадке для списка точек нужен только заголовок, для очистки — только ссылки, по одной.

use anyhow::{bail, Result};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use std::fmt;

use super::types::{ChunkId, SnapshotHeader};

/// Что найдено в записи.
pub struct Scanned {
    pub header: SnapshotHeader,
    pub refs: u64,
    pub has_body: bool,
}

/// Прочитать запись снапшота из `r`: заголовок собирается, каждая ссылка передаётся в `on_ref`,
/// зашифрованное тело пропускается.
pub fn scan<R: std::io::Read>(r: R, mut on_ref: impl FnMut(ChunkId)) -> Result<Scanned> {
    let mut de = serde_json::Deserializer::from_reader(r);
    let (head, refs, has_body, had_refs) = Root { on_ref: &mut on_ref }.deserialize(&mut de)?;
    de.end()?;
    if !had_refs { bail!("в записи снапшота нет списка ссылок"); }
    let header: SnapshotHeader = serde_json::from_value(serde_json::Value::Object(head))?;
    Ok(Scanned { header, refs, has_body })
}

/// Только заголовок (ссылки и тело пропускаются).
pub fn header<R: std::io::Read>(r: R) -> Result<SnapshotHeader> { Ok(scan(r, |_| {})?.header) }

struct Root<'a, F: FnMut(ChunkId)> {
    on_ref: &'a mut F,
}

type RootOut = (serde_json::Map<String, serde_json::Value>, u64, bool, bool);

impl<'de, F: FnMut(ChunkId)> DeserializeSeed<'de> for Root<'_, F> {
    type Value = RootOut;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<RootOut, D::Error> { d.deserialize_map(self) }
}

impl<'de, F: FnMut(ChunkId)> Visitor<'de> for Root<'_, F> {
    type Value = RootOut;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("запись снапшота (объект JSON)") }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<RootOut, A::Error> {
        let mut head = serde_json::Map::new();
        let (mut n, mut body, mut had_refs) = (0u64, false, false);
        while let Some(k) = m.next_key::<String>()? {
            match k.as_str() {
                "refs" => { n += m.next_value_seed(Refs { on_ref: &mut *self.on_ref })?; had_refs = true; }
                "body" => { m.next_value::<IgnoredAny>()?; body = true; }
                _ => { let v: serde_json::Value = m.next_value()?; head.insert(k, v); }
            }
        }
        Ok((head, n, body, had_refs))
    }
}

struct Refs<'a, F: FnMut(ChunkId)> {
    on_ref: &'a mut F,
}

impl<'de, F: FnMut(ChunkId)> DeserializeSeed<'de> for Refs<'_, F> {
    type Value = u64;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<u64, D::Error> { d.deserialize_seq(self) }
}

impl<'de, F: FnMut(ChunkId)> Visitor<'de> for Refs<'_, F> {
    type Value = u64;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("список ссылок на блоки") }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<u64, A::Error> {
        let mut n = 0;
        while let Some(id) = s.next_element::<ChunkId>()? { (self.on_ref)(id); n += 1; }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::*;

    #[test]
    fn scan_record() {
        let rec = SnapshotRecord {
            header: SnapshotHeader { id: "s1".into(), time: chrono::Utc::now(), instance: "i".into(), product: "files".into(), kind: "files".into(), host: "h".into(), size: 5, new_bytes: 1, uploaded: 2, chunks: 3, extra: serde_json::json!({"wal_start": "00000001"}) },
            refs: vec![ChunkId([1; 32]), ChunkId([2; 32])],
            body: "QUJD".into(),
        };
        let b = serde_json::to_vec(&rec).unwrap();
        let mut got = Vec::new();
        let s = scan(&b[..], |id| got.push(id)).unwrap();
        assert_eq!(s.header.id, "s1");
        assert_eq!(s.header.extra["wal_start"], "00000001");
        assert_eq!(got, rec.refs);
        assert!(s.has_body && s.refs == 2);
        assert!(header(&b"{\"id\":\"x\"}"[..]).is_err(), "без ссылок — ошибка");
    }
}
