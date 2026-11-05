//! Передача готовых файлов образов (qcow2 и др.) на удалённую площадку только изменениями.
//!
//! Файл делится на участки по 4 МиБ; у каждого — сумма (BLAKE3, 16 байт; у участка из одних нулей — нули).
//! Площадка готовит новый набор копией прошлого (разрежённо, на своём диске) и отдаёт суммы участков; сервер
//! сравнивает со своими и передаёт только отличающиеся участки (сжатыми), нулевые — командой «обнулить» (дыра).
//! После — суммы сверяются ещё раз целиком.
//!
//! Чтобы изменения оставались на своих местах, qcow2 собирается с `preallocation=metadata`: раскладка файла
//! фиксирована (смещение данных зависит только от смещения в диске), а нетронутые области — дыры. Обычный
//! «плотный» qcow2 сдвигается при любом новом блоке и передавался бы целиком.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::frames::{block_hash, HASH};

/// Участок сравнения и передачи.
pub const CHUNK: u64 = 4 << 20;

/// Участки файла с данными (SEEK_DATA/SEEK_HOLE). Если ФС этого не умеет — весь файл.
pub fn data_ranges(f: &File, size: u64) -> Vec<(u64, u64)> {
    let fd = f.as_raw_fd();
    let mut out = Vec::new();
    let mut pos: i64 = 0;
    while (pos as u64) < size {
        let d = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
        if d < 0 {
            // ENXIO — дальше данных нет; другая ошибка — ФС не поддерживает: считаем данными всё.
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENXIO) { break; }
            return vec![(0, size)];
        }
        let h = unsafe { libc::lseek(fd, d, libc::SEEK_HOLE) };
        let end = if h < 0 { size as i64 } else { h.min(size as i64) };
        if end > d { out.push((d as u64, end as u64)); }
        pos = end;
    }
    out
}

/// Суммы участков файла по CHUNK байт (участки без данных — нулевые суммы без чтения).
pub fn sums(path: &Path) -> Result<Vec<[u8; HASH]>> {
    let f = File::open(path).with_context(|| format!("файл {}", path.display()))?;
    let size = f.metadata()?.len();
    let ranges = data_ranges(&f, size);
    let n = size.div_ceil(CHUNK);
    let mut out = vec![[0u8; HASH]; n as usize];
    let mut buf = vec![0u8; CHUNK as usize];
    let mut ri = 0usize;
    for i in 0..n {
        let (a, b) = (i * CHUNK, ((i + 1) * CHUNK).min(size));
        while ri < ranges.len() && ranges[ri].1 <= a { ri += 1; }
        if ri >= ranges.len() || ranges[ri].0 >= b { continue; }
        let len = (b - a) as usize;
        f.read_exact_at(&mut buf[..len], a)?;
        out[i as usize] = block_hash(&buf[..len]);
    }
    Ok(out)
}

pub fn sums_bytes(s: &[[u8; HASH]]) -> Vec<u8> { s.iter().flat_map(|h| h.iter().copied()).collect() }

pub fn sums_from(b: &[u8]) -> Result<Vec<[u8; HASH]>> {
    if b.len() % HASH != 0 { bail!("суммы участков: {} байт — не кратно {HASH}", b.len()); }
    Ok(b.chunks(HASH).map(|c| { let mut h = [0u8; HASH]; h.copy_from_slice(c); h }).collect())
}

/// Скопировать файл разрежённо (только участки с данными) и задать размер.
pub fn copy_sparse(src: &Path, dst: &Path) -> Result<()> {
    let s = File::open(src).with_context(|| format!("{}", src.display()))?;
    let size = s.metadata()?.len();
    let d = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(dst).with_context(|| format!("{}", dst.display()))?;
    d.set_len(size)?;
    let mut buf = vec![0u8; CHUNK as usize];
    for (a, b) in data_ranges(&s, size) {
        let mut off = a;
        while off < b {
            let n = (b - off).min(CHUNK) as usize;
            s.read_exact_at(&mut buf[..n], off)?;
            // Нули не пишем: в новом файле там и так дыра.
            if buf[..n].iter().any(|x| *x != 0) { d.write_all_at(&buf[..n], off)?; }
            off += n as u64;
        }
    }
    d.sync_all()?;
    Ok(())
}

/// Обнулить участок файла: пробить дыру (или записать нули, если ФС не умеет).
pub fn zero_range(f: &File, off: u64, len: u64) -> Result<()> {
    let r = unsafe { libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, off as i64, len as i64) };
    if r == 0 { return Ok(()); }
    let z = vec![0u8; CHUNK.min(len) as usize];
    let mut p = off;
    while p < off + len { let n = (off + len - p).min(z.len() as u64) as usize; f.write_all_at(&z[..n], p)?; p += n as u64; }
    Ok(())
}

/// Имя экземпляра или файла образа на площадке: буквы, цифры, «.-_», без «..».
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && !n.starts_with('.') && !n.contains("..") && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Номера участков, которые нужно передать или обнулить: (номер, данные нулевые?).
pub fn plan(local: &[[u8; HASH]], remote: &[[u8; HASH]]) -> Vec<(u64, bool)> {
    local.iter().enumerate().filter(|(i, h)| remote.get(*i) != Some(*h)).map(|(i, h)| (i as u64, *h == [0u8; HASH])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_sums_copy_and_plan() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let f = File::create(&a).unwrap();
        f.set_len(CHUNK * 5 + 100).unwrap();
        f.write_all_at(&[7u8; 1000], CHUNK + 10).unwrap();
        f.write_all_at(&[9u8; 50], CHUNK * 5 + 10).unwrap();
        drop(f);
        let s = sums(&a).unwrap();
        assert_eq!(s.len(), 6);
        assert_eq!(s[0], [0u8; HASH]);
        assert_ne!(s[1], [0u8; HASH]);
        assert_ne!(s[5], [0u8; HASH]);
        let b = d.path().join("b");
        copy_sparse(&a, &b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_eq!(sums(&b).unwrap(), s);
        // Изменения: участок 2 получил данные, участок 1 обнулён.
        let g = std::fs::OpenOptions::new().write(true).open(&b).unwrap();
        g.write_all_at(&[1u8; 10], CHUNK * 2).unwrap();
        zero_range(&g, CHUNK, CHUNK).unwrap();
        drop(g);
        let s2 = sums(&b).unwrap();
        assert_eq!(plan(&s2, &s), vec![(1, true), (2, false)]);
        assert_eq!(sums_from(&sums_bytes(&s2)).unwrap(), s2);
        assert!(valid_name("sda.qcow2") && valid_name("machine-srv01") && !valid_name("../x") && !valid_name(".part") && !valid_name("a/b"));
    }
}
