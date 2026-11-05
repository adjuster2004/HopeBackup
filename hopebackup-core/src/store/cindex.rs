//! Компактный индекс блоков: где какой блок лежит.
//!
//! Отсортированный массив записей по 44 байта (идентификатор 32 + номер pack-файла, смещение, длина) —
//! около 38 МБ на 1 ТБ уникальных данных, без накладных расходов хэш-таблицы. Новые записи сначала
//! попадают в небольшую хэш-таблицу и периодически вливаются в массив. Номер записи в массиве даёт
//! очистке битовую карту живых блоков: 1 бит на блок вместо множества идентификаторов.

use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::types::{ChunkId, IndexEntry};

#[derive(Clone, Copy, Debug)]
struct Ent {
    id: [u8; 32],
    pack: u32,
    offset: u32,
    len: u32,
}

/// Положение блока: номер pack-файла (см. [`ChunkIndex::pack_hex`]), смещение, длина.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub pack: u32,
    pub offset: u32,
    pub len: u32,
}

/// Сколько новых записей держать вне массива, прежде чем влить их.
const DELTA_MAX: usize = 32 * 1024;

#[derive(Default)]
pub struct ChunkIndex {
    /// Номер → идентификатор pack-файла; удалённые — нули.
    packs: Vec<[u8; 32]>,
    by_pack: HashMap<[u8; 32], u32>,
    main: Vec<Ent>,
    delta: HashMap<[u8; 32], Loc>,
}

fn pack_bytes(hex: &str) -> Result<[u8; 32]> {
    let mut a = [0u8; 32];
    hex::decode_to_slice(hex, &mut a).map_err(|_| anyhow!("неверный идентификатор pack-файла {hex}"))?;
    Ok(a)
}

impl ChunkIndex {
    pub fn new() -> Self { Self::default() }

    /// Заранее выделить место (при загрузке — по размеру файлов индекса).
    pub fn reserve(&mut self, entries: usize) { self.main.reserve(entries); }

    fn pack_no(&mut self, hex: &str) -> Result<u32> {
        let b = pack_bytes(hex)?;
        if let Some(&n) = self.by_pack.get(&b) { return Ok(n); }
        let n = u32::try_from(self.packs.len()).map_err(|_| anyhow!("слишком много pack-файлов"))?;
        self.packs.push(b);
        self.by_pack.insert(b, n);
        Ok(n)
    }

    /// Загрузка: записи добавляются без проверок, порядок наводит [`finish_load`](Self::finish_load).
    pub fn load_pack(&mut self, pack_hex: &str, entries: &[IndexEntry]) -> Result<()> {
        let pack = self.pack_no(pack_hex)?;
        self.main.extend(entries.iter().map(|e| Ent { id: e.id.0, pack, offset: e.offset, len: e.len }));
        Ok(())
    }

    pub fn finish_load(&mut self) {
        // Без доп. памяти; при повторе блока в нескольких pack-файлах остаётся любой из них.
        self.main.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        self.main.dedup_by(|b, a| a.id == b.id);
        self.main.shrink_to_fit();
    }

    /// Новый pack-файл: блоки, которых ещё нет в индексе.
    pub fn add_pack(&mut self, pack_hex: &str, entries: &[IndexEntry]) -> Result<()> {
        let pack = self.pack_no(pack_hex)?;
        for e in entries {
            if self.get(&e.id).is_none() { self.delta.insert(e.id.0, Loc { pack, offset: e.offset, len: e.len }); }
        }
        if self.delta.len() >= DELTA_MAX { self.compact(); }
        Ok(())
    }

    /// Влить новые записи в отсортированный массив.
    pub fn compact(&mut self) {
        if self.delta.is_empty() { return; }
        let mut add: Vec<Ent> = self.delta.drain().map(|(id, l)| Ent { id, pack: l.pack, offset: l.offset, len: l.len }).collect();
        add.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        // Слияние с конца на месте: без второй копии массива.
        let (old, n) = (self.main.len(), add.len());
        self.main.resize(old + n, Ent { id: [0; 32], pack: 0, offset: 0, len: 0 });
        let (mut i, mut j, mut k) = (old, n, old + n);
        while j > 0 {
            if i > 0 && self.main[i - 1].id > add[j - 1].id {
                self.main[k - 1] = self.main[i - 1];
                i -= 1;
            } else {
                self.main[k - 1] = add[j - 1];
                j -= 1;
            }
            k -= 1;
        }
    }

    pub fn get(&self, id: &ChunkId) -> Option<Loc> {
        if let Ok(p) = self.main.binary_search_by(|e| e.id.cmp(&id.0)) {
            let e = &self.main[p];
            return Some(Loc { pack: e.pack, offset: e.offset, len: e.len });
        }
        self.delta.get(&id.0).copied()
    }

    pub fn contains(&self, id: &ChunkId) -> bool { self.get(id).is_some() }

    pub fn len(&self) -> usize { self.main.len() + self.delta.len() }

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn pack_hex(&self, no: u32) -> String { hex::encode(self.packs[no as usize]) }

    pub fn pack_of(&self, hex: &str) -> Option<u32> { pack_bytes(hex).ok().and_then(|b| self.by_pack.get(&b).copied()) }

    /// Число номеров pack-файлов (вместе с удалёнными).
    pub fn pack_slots(&self) -> usize { self.packs.len() }

    pub fn pack_alive(&self, no: u32) -> bool { self.packs.get(no as usize).map(|b| self.by_pack.contains_key(b)).unwrap_or(false) }

    /// Номер блока в массиве (после [`compact`](Self::compact)) — для битовой карты живых блоков.
    pub fn position(&self, id: &ChunkId) -> Option<usize> { self.main.binary_search_by(|e| e.id.cmp(&id.0)).ok() }

    /// Битовая карта под все блоки массива.
    pub fn new_bitmap(&self) -> Bitmap { Bitmap(vec![0u64; self.main.len().div_ceil(64)]) }

    /// Для каждого pack-файла: есть ли в нём живые блоки и есть ли мёртвые (по битовой карте).
    /// Pack-файл без записей в индексе (все его блоки повторяют блоки других) — без живых.
    pub fn pack_liveness(&self, live: &Bitmap) -> (Vec<bool>, Vec<bool>) {
        let mut any_live = vec![false; self.packs.len()];
        let mut any_dead = vec![false; self.packs.len()];
        for (p, e) in self.main.iter().enumerate() {
            if live.get(p) { any_live[e.pack as usize] = true; } else { any_dead[e.pack as usize] = true; }
        }
        (any_live, any_dead)
    }

    /// Убрать pack-файлы и их блоки из индекса.
    pub fn remove_packs(&mut self, dead: &[u32]) {
        if dead.is_empty() { return; }
        self.compact();
        let mut gone = vec![false; self.packs.len()];
        for &d in dead {
            gone[d as usize] = true;
            let b = self.packs[d as usize];
            self.by_pack.remove(&b);
            self.packs[d as usize] = [0; 32];
        }
        self.main.retain(|e| !gone[e.pack as usize]);
    }

    /// Память под индекс, байт (для журнала и замеров).
    pub fn mem_bytes(&self) -> usize {
        self.main.capacity() * std::mem::size_of::<Ent>() + self.delta.capacity() * 48 + self.packs.capacity() * 32 + self.by_pack.capacity() * 48
    }
}

/// Битовая карта по номерам блоков индекса.
pub struct Bitmap(Vec<u64>);

impl Bitmap {
    pub fn set(&mut self, i: usize) { self.0[i / 64] |= 1 << (i % 64); }
    pub fn get(&self, i: usize) -> bool { self.0[i / 64] & (1 << (i % 64)) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ChunkId {
        let mut a = [0u8; 32];
        a[..4].copy_from_slice(&n.to_be_bytes());
        a[31] = (n % 251) as u8;
        ChunkId(blake3::hash(&a).into())
    }
    fn ph(n: u8) -> String { hex::encode([n; 32]) }

    #[test]
    fn index_ops() {
        let mut ix = ChunkIndex::new();
        let e = |a: u32, b: u32| (a..b).map(|n| IndexEntry { id: id(n), offset: n, len: 10 }).collect::<Vec<_>>();
        ix.load_pack(&ph(1), &e(0, 1000)).unwrap();
        ix.load_pack(&ph(2), &e(500, 1500)).unwrap(); // половина повторяет первый
        ix.finish_load();
        assert_eq!(ix.len(), 1500);
        for k in 0..100u32 { ix.add_pack(&ph(3 + (k % 200) as u8), &e(1500 + k * 400, 1900 + k * 400)).unwrap(); }
        ix.compact();
        assert_eq!(ix.len(), 1500 + 100 * 400);
        assert!(ix.main.windows(2).all(|w| w[0].id < w[1].id), "массив отсортирован после слияния");
        assert_eq!(ix.get(&id(1500 + 99 * 400 + 5)).unwrap().offset, 1500 + 99 * 400 + 5);
        assert!(!ix.contains(&id(999_999)));
        // Живые: блоки 0..100 (только в pack 0). pack 0 — частично живой, у pack 1 живых нет.
        let mut live = ix.new_bitmap();
        for n in 0..100 { live.set(ix.position(&id(n)).unwrap()); }
        let (al, ad) = ix.pack_liveness(&live);
        assert!(al[0] && ad[0] && !al[1]);
        ix.remove_packs(&[1]);
        assert!(ix.get(&id(1200)).is_none(), "блоки только из pack 1 ушли");
        assert!(!ix.pack_alive(1) && ix.pack_alive(0));
        assert_eq!(std::mem::size_of::<Ent>(), 44);
    }
}
