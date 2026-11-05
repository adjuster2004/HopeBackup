//! Замер репликации площадки (не входит в обычные тесты):
//!   BENCH_MB=512 cargo test --profile fast repl_bench -- --ignored --nocapture
//! Два узла HopeBackup в этом же процессе, к принимающему — через «канал» с задержкой и ограничением скорости
//! (как VPN между ЦОД). Сравниваются: обычная репликация (pack-файлы целиком); репликация с исключённым продуктом —
//! как в 0.9.4 (блоки по одному, отправка после чтения) и как сейчас (пачки блоков, отправка в фоне); возврат к
//! обычной после репликации с исключениями.
#![cfg(test)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::replication::{replicate, Known};
use crate::node::{NodeConfig, NodeToken, Role};
use crate::plugins::LogFn;
use crate::store::http::{HttpBackend, Remote};
use crate::store::types::*;
use crate::store::Backend;

const TOKEN: &str = "bench-token";

/// Узел на 127.0.0.1: порт.
async fn node(dir: &std::path::Path) -> u16 {
    std::fs::create_dir_all(dir.join("repos")).unwrap();
    let cfg = NodeConfig { listen: String::new(), tokens: vec![NodeToken { name: "admin".into(), sha256: crate::util::sha256_hex(TOKEN), role: Role::Admin, repo: String::new(), created: chrono::Utc::now() }] };
    let app = crate::node::router(dir.to_path_buf(), cfg);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    port
}

/// «Канал»: задержка в одну сторону и общая для всех соединений скорость (байт/с, 0 — без ограничения).
struct Link { delay: Duration, rate: u64, bytes: AtomicU64, next: [parking_lot::Mutex<Instant>; 2] }

async fn proxy(to: u16, link: Arc<Link>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (a, _) = l.accept().await.unwrap();
            let b = tokio::net::TcpStream::connect(("127.0.0.1", to)).await.unwrap();
            let (ar, aw) = a.into_split();
            let (br, bw) = b.into_split();
            tokio::spawn(pump(ar, bw, link.clone(), 0));
            tokio::spawn(pump(br, aw, link.clone(), 1));
        }
    });
    port
}

async fn pump(mut r: tokio::net::tcp::OwnedReadHalf, mut w: tokio::net::tcp::OwnedWriteHalf, link: Arc<Link>, dir: usize) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let l2 = link.clone();
    tokio::spawn(async move {
        while let Some((at, buf)) = rx.recv().await {
            // Пропускная способность: общая очередь направления.
            let ready = if l2.rate > 0 {
                let mut n = l2.next[dir].lock();
                let start = (*n).max(at);
                *n = start + Duration::from_secs_f64(buf.len() as f64 / l2.rate as f64);
                *n
            } else { at };
            tokio::time::sleep_until((ready + l2.delay).into()).await;
            l2.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
            if w.write_all(&buf).await.is_err() { return; }
        }
        let _ = w.shutdown().await;
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => { if tx.send((Instant::now(), buf[..n].to_vec())).is_err() { return; } }
        }
    }
}

fn http(port: u16) -> Arc<HttpBackend> { Arc::new(HttpBackend::new(&Remote::node(&format!("http://127.0.0.1:{port}"), "main", TOKEN, "")).unwrap()) }

fn blob(i: u64, size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    blake3::Hasher::new().update(&i.to_le_bytes()).finalize_xof().fill(&mut v);
    v
}

fn snap(id: &str, inst: &str, t: i64, refs: Vec<ChunkId>) -> SnapshotRecord {
    let h: SnapshotHeader = serde_json::from_value(serde_json::json!({ "id": id, "time": chrono::DateTime::from_timestamp(1_700_000_000 + t, 0).unwrap(), "instance": inst, "product": "files", "kind": "full", "host": "h", "size": 0, "new_bytes": 0, "uploaded": 0, "chunks": refs.len() })).unwrap();
    SnapshotRecord { header: h, refs, body: "AAAA".into() }
}

/// Исходный репозиторий: `mb` МБ блоками по 512 КиБ в pack-файлах по 16 МиБ; 7 копий продукта «keep» (границы копий не совпадают с pack-файлами) и одна
/// копия исключаемого «img» (своими блоками, ~1/8 объёма).
async fn fill_src(dir: &std::path::Path, mb: u64) {
    let be: Arc<dyn Backend> = Arc::new(crate::store::local::LocalBackend::open(dir.join("repos/main")).unwrap());
    crate::store::repo::Repo::init(be.clone(), "bench-password").await.unwrap();
    let bs = 512 * 1024usize;
    let n = mb * 1024 * 1024 / bs as u64;
    let img = n / 8;
    let mut ids = Vec::new();
    for p in (0..n + img).collect::<Vec<_>>().chunks(32) {
        let mut data = Vec::new();
        let mut e = Vec::new();
        for &i in p {
            let b = blob(i, bs);
            let id = ChunkId(*blake3::hash(&b).as_bytes());
            e.push(IndexEntry { id, offset: data.len() as u32, len: b.len() as u32 });
            data.extend_from_slice(&b);
            ids.push(id);
        }
        let pid = blake3::hash(&data).to_hex().to_string();
        be.put_pack(&pid, bytes::Bytes::from(data), e).await.unwrap();
    }
    let keep = &ids[..n as usize];
    for (k, part) in keep.chunks((keep.len() + 6) / 7).enumerate() { be.put_snapshot(&snap(&format!("s{k:02}"), "keep", k as i64, part.to_vec())).await.unwrap(); }
    be.put_snapshot(&snap("img00", "img", 100, ids[n as usize..].to_vec())).await.unwrap();
}

/// Как в 0.9.4: блоки по одному (8 параллельно), pack-файл отправляется после чтения, чтение ждёт отправки.
async fn old_copy_blobs(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, ids: Vec<ChunkId>) -> u64 {
    let mut st = futures::stream::iter(ids).map(|id| { let src = src.clone(); async move { src.get_blob(&id).await.map(|b| (id, b)) } }).buffered(8);
    let (mut pack, mut entries): (Vec<u8>, Vec<IndexEntry>) = (Vec::new(), Vec::new());
    let mut bytes = 0u64;
    while let Some(r) = st.next().await {
        let (id, b) = r.unwrap();
        entries.push(IndexEntry { id, offset: pack.len() as u32, len: b.len() as u32 });
        bytes += b.len() as u64;
        pack.extend_from_slice(&b);
        if pack.len() >= crate::store::repo::PACK_TARGET {
            let d = std::mem::take(&mut pack);
            dst.put_pack(&blake3::hash(&d).to_hex(), bytes::Bytes::from(d), std::mem::take(&mut entries)).await.unwrap();
        }
    }
    if !entries.is_empty() { dst.put_pack(&blake3::hash(&pack).to_hex(), bytes::Bytes::from(pack), entries).await.unwrap(); }
    bytes
}

async fn old_exclusion(src: Arc<dyn Backend>, dst: Arc<dyn Backend>, exclude: &HashSet<String>) {
    dst.write_config(&src.read_config().await.unwrap().unwrap()).await.unwrap();
    let mut todo = src.list_snapshots(None).await.unwrap();
    todo.retain(|h| !exclude.contains(&h.instance));
    for h in todo {
        let rec = src.get_snapshot(&h.id).await.unwrap();
        let mut need = Vec::new();
        for p in rec.refs.chunks(4096) { need.extend(dst.missing(p.to_vec()).await.unwrap()); }
        old_copy_blobs(src.clone(), dst.clone(), need).await;
        dst.put_snapshot(&rec).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn repl_bench() {
    let mb: u64 = std::env::var("BENCH_MB").ok().and_then(|x| x.parse().ok()).unwrap_or(256);
    let t = tempfile::tempdir().unwrap();
    fill_src(&t.path().join("src"), mb).await;
    let src_node = node(&t.path().join("src")).await;
    // BENCH_SRC_FAR=1: сервер далеко и от исходной площадки (тот же канал), иначе — рядом с ней.
    let far = std::env::var("BENCH_SRC_FAR").map(|v| v == "1").unwrap_or(false);
    let log: LogFn = Arc::new(|m| if m.contains("пропущен") || m.contains("ошибк") { eprintln!("  журнал: {m}") });
    let excl: HashSet<String> = ["img".to_string()].into();
    println!("\nисточник: {mb} МБ продукта + {} МБ исключаемого образа; блоки 512 КиБ; сервер {} исходной площадки", mb / 8, if far { "далеко от" } else { "рядом с" });
    println!("{:<34} {:>12} {:>12} {:>12} {:>12}", "канал до принимающей", "обычная", "искл. 0.9.4", "искл. сейчас", "возврат");
    for (rtt, rate_mb) in [(1u64, 0u64), (50, 0), (50, 40), (100, 40)] {
        let mut cols = Vec::new();
        let mut back = String::new();
        for mode in ["packs", "old", "new"] {
            let d = t.path().join(format!("dst-{rtt}-{rate_mb}-{mode}"));
            let dport = node(&d).await;
            let link = Arc::new(Link { delay: Duration::from_millis(rtt / 2), rate: rate_mb * 1_000_000, bytes: AtomicU64::new(0), next: [parking_lot::Mutex::new(Instant::now()), parking_lot::Mutex::new(Instant::now())] });
            let pport = proxy(dport, link.clone()).await;
            let src_link = Arc::new(Link { delay: if far { link.delay } else { Duration::ZERO }, rate: if far { link.rate } else { 0 }, bytes: AtomicU64::new(0), next: [parking_lot::Mutex::new(Instant::now()), parking_lot::Mutex::new(Instant::now())] });
            let src_port = proxy(src_node, src_link).await;
            let (src, dst): (Arc<dyn Backend>, Arc<dyn Backend>) = (http(src_port), http(pport));
            let t0 = Instant::now();
            match mode {
                "packs" => { replicate(src.clone(), dst.clone(), &HashSet::new(), &Known::default(), &log).await.map_err(|e| format!("{e:#}")).unwrap(); }
                "old" => {
                    let (s, d): (Arc<dyn Backend>, Arc<dyn Backend>) = (Arc::new(HttpBackend::new(&Remote::node(&format!("http://127.0.0.1:{src_port}"), "main", TOKEN, "")).unwrap().without_batch()), Arc::new(HttpBackend::new(&Remote::node(&format!("http://127.0.0.1:{pport}"), "main", TOKEN, "")).unwrap().without_batch()));
                    old_exclusion(s, d, &excl).await;
                }
                _ => { replicate(src.clone(), dst.clone(), &excl, &Known::default(), &log).await.unwrap(); }
            }
            let secs = t0.elapsed().as_secs_f64();
            cols.push(format!("{secs:.1} с"));
            if mode == "new" {
                // Исключение сняли: обычная репликация на ту же площадку.
                let before = link.bytes.load(Ordering::Relaxed);
                let t1 = Instant::now();
                let st = replicate(src.clone(), dst.clone(), &HashSet::new(), &Known::default(), &log).await.unwrap();
                let sent = link.bytes.load(Ordering::Relaxed) - before;
                back = format!("{:.1} с, {} МБ (pack целиком {}, пропущено {})", t1.elapsed().as_secs_f64(), sent / 1_000_000, st.packs, st.packs_skipped);
            }
        }
        for mode in ["packs", "old", "new"] { let _ = std::fs::remove_dir_all(t.path().join(format!("dst-{rtt}-{rate_mb}-{mode}"))); }
        let ch = format!("RTT {rtt} мс, {}", if rate_mb == 0 { "без ограничения".to_string() } else { format!("{} Мбит/с", rate_mb * 8) });
        println!("{ch:<34} {:>12} {:>12} {:>12}   {back}", cols[0], cols[1], cols[2]);
    }
}
