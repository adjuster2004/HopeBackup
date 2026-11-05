//! Прозрачный разбор tar-потока: байты идут дальше без изменений, а нужные небольшие файлы
//! (backup_label, backup_manifest) запоминаются по дороге — без временных файлов на диске.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

const MAX_CAPTURE: u64 = 64 * 1024 * 1024;

pub type Captured = Arc<Mutex<HashMap<String, Vec<u8>>>>;

pub struct TarSniffer<R> {
    inner: R,
    want: Vec<String>,
    out: Captured,
    header: Vec<u8>,
    data_left: u64,
    pad_left: u64,
    capture: Option<(String, Vec<u8>)>,
    long_name: Option<String>,
    long_buf: Option<Vec<u8>>,
}

impl<R: Read> TarSniffer<R> {
    pub fn new(inner: R, want: &[&str]) -> (Self, Captured) {
        let out: Captured = Arc::new(Mutex::new(HashMap::new()));
        (
            TarSniffer { inner, want: want.iter().map(|s| s.to_string()).collect(), out: out.clone(), header: Vec::with_capacity(512), data_left: 0, pad_left: 0, capture: None, long_name: None, long_buf: None },
            out,
        )
    }

    fn feed(&mut self, mut b: &[u8]) {
        while !b.is_empty() {
            if self.data_left > 0 {
                let n = (self.data_left as usize).min(b.len());
                if let Some((_, buf)) = &mut self.capture { buf.extend_from_slice(&b[..n]); }
                if let Some(buf) = &mut self.long_buf { buf.extend_from_slice(&b[..n]); }
                self.data_left -= n as u64;
                b = &b[n..];
                if self.data_left == 0 {
                    if let Some((name, buf)) = self.capture.take() { self.out.lock().insert(name, buf); }
                    if let Some(buf) = self.long_buf.take() {
                        let s = String::from_utf8_lossy(&buf);
                        self.long_name = Some(s.trim_end_matches('\0').to_string());
                    }
                }
                continue;
            }
            if self.pad_left > 0 {
                let n = (self.pad_left as usize).min(b.len());
                self.pad_left -= n as u64;
                b = &b[n..];
                continue;
            }
            let need = 512 - self.header.len();
            let n = need.min(b.len());
            self.header.extend_from_slice(&b[..n]);
            b = &b[n..];
            if self.header.len() == 512 {
                let h = std::mem::take(&mut self.header);
                self.parse(&h);
            }
        }
    }

    fn parse(&mut self, h: &[u8]) {
        if h.iter().all(|&x| x == 0) { return; }
        let size = parse_size(&h[124..136]);
        let typ = h[156];
        let cstr = |s: &[u8]| String::from_utf8_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]).to_string();
        let mut name = cstr(&h[0..100]);
        if &h[257..262] == b"ustar" {
            let prefix = cstr(&h[345..500]);
            if !prefix.is_empty() { name = format!("{prefix}/{name}"); }
        }
        if let Some(l) = self.long_name.take() { name = l; }
        let name = name.trim_start_matches("./").to_string();
        self.data_left = size;
        self.pad_left = (512 - size % 512) % 512;
        if typ == b'L' && size < 65536 {
            self.long_buf = Some(Vec::new());
            if size == 0 { self.long_buf = None; }
            return;
        }
        if (typ == b'0' || typ == 0) && size <= MAX_CAPTURE && self.want.iter().any(|w| *w == name) {
            if size == 0 { self.out.lock().insert(name, Vec::new()); } else { self.capture = Some((name, Vec::new())); }
        }
    }
}

fn parse_size(f: &[u8]) -> u64 {
    if f[0] & 0x80 != 0 {
        // base-256 (GNU) для файлов больше 8 ГиБ
        let mut v: u64 = (f[0] & 0x7f) as u64;
        for &c in &f[1..] { v = (v << 8) | c as u64; }
        return v;
    }
    let s: String = f.iter().take_while(|&&c| c != 0 && c != b' ').map(|&c| c as char).collect();
    u64::from_str_radix(s.trim(), 8).unwrap_or(0)
}

impl<R: Read> Read for TarSniffer<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.feed(&buf[..n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_files_from_stream() {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        };
        add("base/1/123", &vec![1u8; 10_000]);
        add("backup_label", b"START WAL LOCATION: 0/2000028 (file 000000010000000000000002)\n");
        add(&format!("{}/deep", "x".repeat(150)), b"long");
        add("backup_manifest", b"{\"WAL-Ranges\":[]}");
        let data = b.into_inner().unwrap();
        let (mut s, cap) = TarSniffer::new(std::io::Cursor::new(data.clone()), &["backup_label", "backup_manifest"]);
        let mut out = Vec::new();
        // Читаем мелкими кусками: заголовки режутся границами буферов.
        let mut buf = [0u8; 37];
        loop {
            let n = s.read(&mut buf).unwrap();
            if n == 0 { break; }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data);
        let c = cap.lock();
        assert!(String::from_utf8_lossy(&c["backup_label"]).contains("000000010000000000000002"));
        assert_eq!(&c["backup_manifest"], b"{\"WAL-Ranges\":[]}");
    }
}
