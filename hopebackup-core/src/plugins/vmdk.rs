//! Чтение VMDK формата streamOptimized (экспорт vSphere через NFC, OVA) как сплошного образа диска.
//!
//! Поток: заголовок разреженного экстента (512 байт, «KDMV»), описание, затем по порядку маркеры:
//! зерно (LBA в секторах, размер сжатых данных, данные zlib) или метаданные (размер 0 и тип: конец потока,
//! таблица зёрен, каталог, подвал) — всё выровнено по сектору. Отсутствующие зёрна — нули.
//! Читатель отдаёт ровно `capacity` байт: зёрна по месту, промежутки нулями — память не растёт с диском.

use anyhow::{bail, Result};
use std::io::Read;

const SECTOR: u64 = 512;
const MAGIC: u32 = 0x564d_444b; // «KDMV»
const MARKER_EOS: u32 = 0;

pub struct StreamOptReader<R: Read> {
    r: R,
    /// Размер диска, байт.
    pub capacity: u64,
    grain: u64,
    /// Позиция в выдаваемом образе.
    pos: u64,
    /// Позиция в потоке VMDK (для выравнивания по сектору).
    spos: u64,
    /// Текущее распакованное зерно и его начало в образе.
    cur: Vec<u8>,
    cur_at: u64,
    eos: bool,
    zbuf: Vec<u8>,
}

fn u32le(b: &[u8]) -> u32 { u32::from_le_bytes(b[..4].try_into().unwrap()) }
fn u64le(b: &[u8]) -> u64 { u64::from_le_bytes(b[..8].try_into().unwrap()) }

impl<R: Read> StreamOptReader<R> {
    pub fn new(mut r: R) -> Result<Self> {
        let mut h = [0u8; 512];
        r.read_exact(&mut h)?;
        if u32le(&h[0..]) != MAGIC { bail!("не VMDK с разреженным экстентом (нет KDMV)"); }
        let flags = u32le(&h[8..]);
        let capacity = u64le(&h[12..]) * SECTOR;
        let grain = u64le(&h[20..]) * SECTOR;
        let overhead = u64le(&h[64..]);
        let compress = u16::from_le_bytes([h[77], h[78]]);
        // Флаги: бит 16 — сжатые зёрна, бит 17 — маркеры (streamOptimized).
        if flags & (1 << 16) == 0 || flags & (1 << 17) == 0 || compress != 1 { bail!("VMDK не в формате streamOptimized (флаги {flags:#x}, сжатие {compress})"); }
        if grain == 0 || grain > 64 << 20 { bail!("VMDK: размер зерна {grain}"); }
        let mut s = StreamOptReader { r, capacity, grain, pos: 0, spos: SECTOR, cur: Vec::new(), cur_at: 0, eos: false, zbuf: Vec::new() };
        s.skip(overhead * SECTOR - SECTOR)?;
        Ok(s)
    }

    /// Исходный поток (чтобы дочитать метаданные после последнего зерна).
    pub fn into_inner(self) -> R { self.r }

    fn skip(&mut self, mut n: u64) -> Result<()> {
        let mut b = [0u8; 8192];
        while n > 0 {
            let k = n.min(b.len() as u64) as usize;
            self.r.read_exact(&mut b[..k])?;
            n -= k as u64;
            self.spos += k as u64;
        }
        Ok(())
    }

    fn align(&mut self) -> Result<()> {
        let pad = (SECTOR - self.spos % SECTOR) % SECTOR;
        self.skip(pad)
    }

    /// Следующее зерно (или конец потока).
    fn next_grain(&mut self) -> Result<bool> {
        loop {
            if self.eos { return Ok(false); }
            let mut m = [0u8; 12];
            match self.r.read_exact(&mut m) {
                Ok(()) => {}
                // Поток без маркера конца (обрезан после последнего зерна) — остаток нулями.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => { self.eos = true; return Ok(false); }
                Err(e) => return Err(e.into()),
            }
            self.spos += 12;
            let val = u64le(&m[0..]);
            let size = u32le(&m[8..]) as u64;
            if size == 0 {
                let mut t = [0u8; 4];
                self.r.read_exact(&mut t)?;
                self.spos += 4;
                self.align()?;
                if u32le(&t) == MARKER_EOS { self.eos = true; return Ok(false); }
                // Таблица зёрен, каталог, подвал: `val` секторов метаданных.
                self.skip(val * SECTOR)?;
                continue;
            }
            if size > self.grain * 2 + 4096 { bail!("VMDK: сжатое зерно {size} байт больше допустимого"); }
            let at = val * SECTOR;
            if at < self.pos { bail!("VMDK: зёрна не по порядку (LBA {val}) — поток не поддерживается"); }
            if at >= self.capacity { bail!("VMDK: зерно за пределами диска (LBA {val})"); }
            self.zbuf.resize(size as usize, 0);
            self.r.read_exact(&mut self.zbuf)?;
            self.spos += size;
            self.align()?;
            let mut out = Vec::with_capacity(self.grain as usize);
            flate2::read::ZlibDecoder::new(&self.zbuf[..]).take(self.grain).read_to_end(&mut out)?;
            let lim = (self.capacity - at).min(self.grain) as usize;
            out.truncate(lim);
            self.cur = out;
            self.cur_at = at;
            return Ok(true);
        }
    }
}

impl<R: Read> Read for StreamOptReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.capacity || out.is_empty() { return Ok(0); }
        let io = |e: anyhow::Error| std::io::Error::other(format!("{e:#}"));
        // Текущее зерно кончилось — следующее.
        if self.cur_at <= self.pos && self.pos >= self.cur_at + self.cur.len() as u64 {
            let more = !self.eos && self.next_grain().map_err(io)?;
            if !more { self.cur.clear(); self.cur_at = self.capacity; }
        }
        let end_cur = self.cur_at + self.cur.len() as u64;
        let n = if self.pos < self.cur_at {
            // Промежуток до зерна — нули.
            let k = (self.cur_at.min(self.capacity) - self.pos).min(out.len() as u64) as usize;
            out[..k].fill(0);
            k
        } else if self.pos < end_cur {
            let off = (self.pos - self.cur_at) as usize;
            let k = (self.cur.len() - off).min(out.len());
            out[..k].copy_from_slice(&self.cur[off..off + k]);
            k
        } else {
            // После последнего зерна (или зерно короче, чем его место) — нули до следующего или до конца.
            let k = (self.capacity - self.pos).min(out.len() as u64) as usize;
            out[..k].fill(0);
            k
        };
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Собрать streamOptimized вручную: заголовок, два зерна с промежутком, таблица, конец.
    fn build(cap_sectors: u64, grains: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0u8; 512];
        v[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        v[4..8].copy_from_slice(&3u32.to_le_bytes());
        v[8..12].copy_from_slice(&((1u32 << 16) | (1 << 17) | 1).to_le_bytes());
        v[12..20].copy_from_slice(&cap_sectors.to_le_bytes());
        v[20..28].copy_from_slice(&128u64.to_le_bytes());
        v[64..72].copy_from_slice(&2u64.to_le_bytes());
        v[77..79].copy_from_slice(&1u16.to_le_bytes());
        v.extend(vec![0u8; 512]); // «описание»
        for (lba, data) in grains {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            z.write_all(data).unwrap();
            let z = z.finish().unwrap();
            v.extend(lba.to_le_bytes());
            v.extend((z.len() as u32).to_le_bytes());
            v.extend(&z);
            while v.len() % 512 != 0 { v.push(0); }
        }
        // Метаданные: таблица зёрен на 1 сектор.
        let mut m = vec![0u8; 512];
        m[0..8].copy_from_slice(&1u64.to_le_bytes());
        m[12..16].copy_from_slice(&1u32.to_le_bytes());
        v.extend(m);
        v.extend(vec![7u8; 512]);
        v.extend(vec![0u8; 512]); // конец потока
        v
    }

    #[test]
    fn grains_and_gaps() {
        let g = 128 * 512;
        let a: Vec<u8> = (0..g).map(|i| (i % 251) as u8).collect();
        let b = vec![9u8; g];
        let cap = 128 * 5 + 7; // хвост — неполное зерно
        let s = build(cap, &[(0, a.clone()), (128 * 3, b.clone()), (128 * 5, vec![5u8; g])]);
        let mut r = StreamOptReader::new(&s[..]).unwrap();
        assert_eq!(r.capacity, cap * 512);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out.len() as u64, cap * 512);
        assert_eq!(&out[..g], &a[..]);
        assert!(out[g..3 * g].iter().all(|x| *x == 0));
        assert_eq!(&out[3 * g..4 * g], &b[..]);
        assert!(out[4 * g..5 * g].iter().all(|x| *x == 0));
        assert!(out[5 * g..].iter().all(|x| *x == 5));
    }

    #[test]
    fn not_stream_optimized() {
        assert!(StreamOptReader::new(&[0u8; 512][..]).is_err());
    }
}
