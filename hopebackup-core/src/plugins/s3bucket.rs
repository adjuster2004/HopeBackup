//! Бакеты S3 (MinIO, Ceph RGW, Yandex Object Storage, AWS S3): объекты каждого бакета — одним
//! tar-потоком (дедупликация: неизменённые объекты дают те же блоки), тип содержимого и
//! пользовательские метаданные — в `s3-meta.json`. Мелкие объекты скачиваются параллельно, большие
//! идут потоком. Восстановление — в тот же или другой бакет/хранилище, большие объекты — частями.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;

use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::cloud::s3::{head_of, S3};

pub struct S3Bucket;

const SMALL: u64 = 8 << 20;
const PART: usize = 64 << 20;

pub fn schema() -> Value {
    json!({ "type": "object", "required": ["endpoint", "access_key", "secret_key"], "properties": {
        "endpoint": { "type": "string", "title": "Адрес S3", "description": "https://minio:9000, https://storage.yandexcloud.net, https://s3.eu-central-1.amazonaws.com" },
        "region": { "type": "string", "title": "Регион", "default": "us-east-1" },
        "access_key": { "type": "string", "title": "Access key (только чтение)" },
        "secret_key": { "type": "string", "format": "secret", "title": "Secret key" },
        "buckets": { "type": "array", "items": { "type": "string" }, "title": "Бакеты", "description": "Пусто — все бакеты, доступные ключу" },
        "prefix": { "type": "string", "title": "Префикс объектов", "description": "Только объекты с этим префиксом" },
        "exclude": { "type": "array", "items": { "type": "string" }, "title": "Исключения", "description": "Части ключа: tmp/, *.log" },
        "path_style": { "type": "boolean", "title": "Адресация по пути (MinIO, Ceph)", "default": true },
        "fingerprint": { "type": "string", "title": "Отпечаток сертификата", "description": "Для самоподписанного сертификата; пусто — системные CA" }
    } })
}

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "target_bucket": { "type": "string", "title": "Целевой бакет", "description": "Пусто — те же имена бакетов" },
        "endpoint": { "type": "string", "title": "Адрес S3 цели", "description": "Пусто — как у экземпляра" },
        "access_key": { "type": "string", "title": "Access key цели (запись)" },
        "secret_key": { "type": "string", "format": "secret", "title": "Secret key цели" },
        "prefix": { "type": "string", "title": "Только объекты с префиксом" },
        "create_bucket": { "type": "boolean", "title": "Создать бакет, если его нет", "default": true },
        "overwrite": { "type": "boolean", "title": "Перезаписывать существующие объекты", "default": true }
    } })
}

pub fn defaults() -> Value { json!({ "region": "us-east-1", "path_style": true, "buckets": [] }) }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ObjMeta {
    size: u64,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    meta: Vec<(String, String)>,
    #[serde(default)]
    etag: String,
    mtime: i64,
}

fn client(p: &Value, bucket: &str) -> Result<S3> {
    let ep = cfg_str(p, "endpoint").ok_or_else(|| anyhow!("не задан адрес S3"))?;
    S3::new(&ep, &cfg_str(p, "region").unwrap_or_default(), bucket, &cfg_str(p, "access_key").unwrap_or_default(), &cfg_str(p, "secret_key").unwrap_or_default(), p.get("path_style").and_then(|v| v.as_bool()).unwrap_or(true), &cfg_str(p, "fingerprint").unwrap_or_default())
}

fn excluded(key: &str, ex: &[String]) -> bool {
    ex.iter().filter(|e| !e.is_empty()).any(|e| if let Some(s) = e.strip_prefix('*') { key.ends_with(s) } else { key.contains(e.as_str()) })
}

enum Item {
    Data(String, ObjMeta, Box<dyn Read + Send>),
}

async fn buckets(p: &Value) -> Result<Vec<String>> {
    let b = crate::connectors::net::p_list(p, "buckets");
    if !b.is_empty() { return Ok(b); }
    client(p, "")?.list_buckets().await.context("список бакетов (ключу нужно s3:ListAllMyBuckets — или перечислите бакеты)")
}

#[async_trait]
impl Plugin for S3Bucket {
    fn id(&self) -> &'static str { "s3" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let mut out = Vec::new();
        for b in buckets(cfg).await? {
            let c = client(cfg, &b)?;
            match c.list(&cfg_str(cfg, "prefix").unwrap_or_default(), None).await {
                Ok((objs, _)) => out.push(json!({ "bucket": b, "objects": objs.len(), "bytes": objs.iter().map(|o| o.size).sum::<u64>() })),
                Err(e) => out.push(json!({ "bucket": b, "error": format!("{e:#}") })),
            }
        }
        Ok(json!({ "buckets": out }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let p = &ctx.config;
        let log = &ctx.log;
        let prefix = cfg_str(p, "prefix").unwrap_or_default();
        let ex = crate::connectors::net::p_list(p, "exclude");
        let mut w = ctx.repo.writer();
        let mut all_meta: HashMap<String, HashMap<String, ObjMeta>> = HashMap::new();
        let mut total = (0u64, 0u64);
        for bucket in buckets(p).await? {
            let c = client(p, &bucket)?;
            let (objs, _) = c.list(&prefix, None).await.with_context(|| format!("бакет {bucket}"))?;
            let objs: Vec<_> = objs.into_iter().filter(|o| !excluded(&o.key, &ex)).collect();
            log(&format!("бакет {bucket}: объектов {}, {}", objs.len(), crate::util::human_bytes(objs.iter().map(|o| o.size).sum())));
            let (r, wp) = std::io::pipe()?;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Item>(16);
            // Писатель tar — отдельный поток ОС (tar синхронный).
            let tarer = std::thread::spawn(move || -> Result<u64> {
                let mut b = tar::Builder::new(wp);
                let mut n = 0u64;
                while let Some(Item::Data(key, m, mut rd)) = rx.blocking_recv() {
                    let mut h = tar::Header::new_gnu();
                    h.set_mtime(m.mtime.max(0) as u64);
                    h.set_mode(0o644);
                    if key.ends_with('/') && m.size == 0 {
                        h.set_entry_type(tar::EntryType::Directory);
                        h.set_size(0);
                        b.append_data(&mut h, &key, std::io::empty())?;
                    } else {
                        h.set_size(m.size);
                        b.append_data(&mut h, &key, &mut rd).with_context(|| format!("объект {key}"))?;
                    }
                    n += 1;
                }
                b.finish()?;
                Ok(n)
            });
            let c2 = c.clone();
            // Мелкие объекты — по 16 параллельно, большие — потоком (порядок сохраняется).
            let producer = tokio::spawn(async move {
                let mut meta: HashMap<String, ObjMeta> = HashMap::new();
                let mut fetch = futures::stream::iter(objs).map(|o| {
                    let c = c2.clone();
                    async move {
                        let resp = c.get_resp(&o.key).await.with_context(|| format!("чтение {}", o.key))?;
                        let h = head_of(resp.headers());
                        let m = ObjMeta { size: h.size, content_type: h.content_type.clone(), meta: h.meta.clone(), etag: h.etag.clone(), mtime: o.last_modified.timestamp() };
                        let rd: Box<dyn Read + Send> = if h.size <= SMALL {
                            Box::new(std::io::Cursor::new(resp.bytes().await?))
                        } else {
                            let s = resp.bytes_stream().map(|x| x.map_err(std::io::Error::other));
                            Box::new(tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(Box::pin(s))))
                        };
                        Ok::<_, anyhow::Error>((o.key, m, rd))
                    }
                }).buffered(16);
                while let Some(x) = fetch.next().await {
                    let (k, m, rd) = x?;
                    meta.insert(k.clone(), m.clone());
                    if tx.send(Item::Data(k, m, rd)).await.is_err() { bail!("поток tar остановлен"); }
                }
                drop(tx);
                Ok::<_, anyhow::Error>(meta)
            });
            let res = w.add_stream(&format!("s3/{bucket}.tar"), r, None).await;
            let meta = producer.await.map_err(|e| anyhow!("{e}"))?;
            let n = tarer.join().map_err(|_| anyhow!("поток tar аварийно завершился"))?;
            let meta = meta?;
            let n = n?;
            res?;
            total.0 += n;
            total.1 += meta.values().map(|m| m.size).sum::<u64>();
            all_meta.insert(bucket, meta);
        }
        let mj = serde_json::to_vec(&all_meta)?;
        w.add_stream("s3-meta.json", std::io::Cursor::new(mj), None).await?;
        let list: Vec<&String> = all_meta.keys().collect();
        let meta = json!({ "buckets": list, "objects": total.0, "bytes": total.1, "endpoint": cfg_str(p, "endpoint") });
        let (header, stats) = w.finish(&ctx.instance, "s3", "full", json!({ "objects": total.0 }), meta).await?;
        log(&format!("объектов {}, {}", total.0, crate::util::human_bytes(total.1)));
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let p = &ctx.params;
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let meta: HashMap<String, HashMap<String, ObjMeta>> = match m.streams.iter().find(|s| s.name == "s3-meta.json") {
            Some(s) => {
                let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W { fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) } fn flush(&mut self) -> std::io::Result<()> { Ok(()) } }
                ctx.repo.restore_stream(s, W(buf.clone())).await?;
                let b = buf.lock().clone();
                serde_json::from_slice(&b)?
            }
            None => HashMap::new(),
        };
        let map: HashMap<String, String> = p.get("bucket_map").and_then(|v| v.as_object()).map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default();
        let single = cfg_str(p, "target_bucket");
        let only = cfg_str(p, "prefix").unwrap_or_default();
        let overwrite = p.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(true);
        let create = p.get("create_bucket").and_then(|v| v.as_bool()).unwrap_or(true);
        let mut report = Vec::new();
        for s in m.streams.iter().filter(|s| s.name.starts_with("s3/") && s.name.ends_with(".tar")) {
            let src = s.name.trim_start_matches("s3/").trim_end_matches(".tar").to_string();
            let dst = map.get(&src).cloned().or_else(|| single.clone()).unwrap_or_else(|| src.clone());
            let c = client(p, &dst)?;
            if create && c.head_bucket().await.is_err() { log(&format!("создание бакета {dst}")); c.create_bucket().await?; }
            log(&format!("бакет {src} → {dst}"));
            let bm = meta.get(&src).cloned().unwrap_or_default();
            let (reader, fetch) = ctx.repo.stream_reader(s)?;
            enum Up { Small(String, Bytes), Start(String), Part(Bytes), End }
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Up>(8);
            let only2 = only.clone();
            let untar = tokio::task::spawn_blocking(move || -> Result<u64> {
                let mut a = tar::Archive::new(reader);
                let mut n = 0u64;
                for e in a.entries()? {
                    let mut e = e?;
                    let mut key = String::from_utf8_lossy(&e.path_bytes()).to_string();
                    if e.header().entry_type().is_dir() && !key.ends_with('/') { key.push('/'); }
                    if !only2.is_empty() && !key.starts_with(&only2) { continue; }
                    let size = e.header().size()?;
                    if size <= PART as u64 {
                        let mut b = Vec::with_capacity(size as usize);
                        e.read_to_end(&mut b)?;
                        tx.blocking_send(Up::Small(key, Bytes::from(b))).map_err(|_| anyhow!("загрузка остановлена"))?;
                    } else {
                        tx.blocking_send(Up::Start(key)).map_err(|_| anyhow!("загрузка остановлена"))?;
                        loop {
                            let mut b = vec![0u8; PART];
                            let mut got = 0;
                            while got < PART { let k = e.read(&mut b[got..])?; if k == 0 { break; } got += k; }
                            if got == 0 { break; }
                            b.truncate(got);
                            tx.blocking_send(Up::Part(Bytes::from(b))).map_err(|_| anyhow!("загрузка остановлена"))?;
                            if got < PART { break; }
                        }
                        tx.blocking_send(Up::End).map_err(|_| anyhow!("загрузка остановлена"))?;
                    }
                    n += 1;
                }
                let mut rest = a.into_inner();
                let _ = std::io::copy(&mut rest, &mut std::io::sink());
                Ok(n)
            });
            let headers = |key: &str| -> Vec<(String, String)> {
                let mut h = Vec::new();
                if let Some(m) = bm.get(key) {
                    if !m.content_type.is_empty() { h.push(("content-type".into(), m.content_type.clone())); }
                    for (k, v) in &m.meta { h.push((format!("x-amz-meta-{k}"), v.clone())); }
                }
                h
            };
            let (mut put, mut skipped, mut bytes) = (0u64, 0u64, 0u64);
            let mut cur: Option<(String, String, Vec<(u32, String)>)> = None;
            let r: Result<()> = async {
                while let Some(u) = rx.recv().await {
                    match u {
                        Up::Small(k, b) => {
                            if !overwrite && c.head(&k).await?.is_some() { skipped += 1; continue; }
                            bytes += b.len() as u64;
                            c.put(&k, b, headers(&k)).await.with_context(|| format!("запись {k}"))?;
                            put += 1;
                        }
                        Up::Start(k) => { let id = c.multipart_create(&k, headers(&k)).await?; cur = Some((k, id, vec![])); }
                        Up::Part(b) => {
                            let (k, id, parts) = cur.as_mut().ok_or_else(|| anyhow!("часть без начала"))?;
                            bytes += b.len() as u64;
                            let n = parts.len() as u32 + 1;
                            let et = c.multipart_part(k, id, n, b).await?;
                            parts.push((n, et));
                        }
                        Up::End => {
                            let (k, id, parts) = cur.take().ok_or_else(|| anyhow!("конец без начала"))?;
                            c.multipart_complete(&k, &id, &parts).await?;
                            put += 1;
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let (Err(_), Some((k, id, _))) = (&r, &cur) { c.multipart_abort(k, id).await; }
            drop(rx);
            let (u, f) = tokio::join!(untar, fetch);
            r?;
            u??;
            f??;
            log(&format!("{dst}: записано объектов {put} ({}){}", crate::util::human_bytes(bytes), if skipped > 0 { format!(", пропущено существующих {skipped}") } else { String::new() }));
            report.push(json!({ "source": src, "bucket": dst, "objects": put, "bytes": bytes, "skipped": skipped }));
        }
        if report.is_empty() { bail!("в копии нет бакетов"); }
        Ok(json!({ "buckets": report }))
    }
}
