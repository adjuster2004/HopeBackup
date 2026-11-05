//! Redis (и совместимые: Valkey, KeyDB): снимок RDB по протоколу репликации (redis-cli --rdb) — без
//! остановки и без доступа к каталогу сервера, плюс redis.conf и файл ACL. Пароль — через REDISCLI_AUTH.
//!
//! Восстановление на место: служба (или контейнер) останавливается, RDB кладётся в каталог данных; если
//! включён AOF — RDB становится базой AOF (Redis 7: appendonlydir с манифестом; до 7 — файл AOF с
//! преамбулой RDB), иначе сервер загрузил бы прежний AOF вместо копии. Прежние файлы — рядом.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::appkit::{self as kit, svc, Cmd, WorkDir};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Redis;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "host": { "type": "string", "title": "Адрес", "default": "127.0.0.1" },
        "port": { "type": "integer", "title": "Порт", "default": 6379 },
        "socket": { "type": "string", "title": "Сокет", "description": "Вместо адреса и порта" },
        "user": { "type": "string", "title": "Пользователь ACL", "description": "Пусто — default. Нужны команды SYNC/PSYNC, INFO, CONFIG GET" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "tls": { "type": "boolean", "title": "TLS", "default": false },
        "container": { "type": "string", "title": "Контейнер Docker" },
        "service": { "type": "string", "title": "Служба", "description": "Для восстановления на место; пусто — redis-server, redis или valkey" },
        "config_file": { "type": "string", "title": "Файл конфигурации", "description": "Пусто — из INFO server (config_file)" }
    } })
}

pub fn defaults() -> Value { json!({ "host": "127.0.0.1", "port": 6379 }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "target_dir": { "type": "string", "title": "Только файл в каталог", "description": "Положить dump.rdb (и конфигурацию) в этот каталог, сервер не трогать" },
        "dir": { "type": "string", "title": "Каталог данных сервера", "description": "Пусто — как в копии (CONFIG GET dir)" },
        "restore_config": { "type": "boolean", "title": "Восстановить redis.conf и ACL", "default": false },
        "start": { "type": "boolean", "title": "Запустить сервер после восстановления", "default": true }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["host", "port", "socket", "user", "password", "tls", "container", "service"] }

#[derive(Clone)]
struct Conn { host: String, port: u16, socket: String, user: String, password: String, tls: bool, container: Option<String> }

impl Conn {
    fn from(v: &Value) -> Conn {
        Conn {
            host: cfg_str(v, "host").unwrap_or_else(|| "127.0.0.1".into()),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(6379) as u16,
            socket: cfg_str(v, "socket").unwrap_or_default(),
            user: cfg_str(v, "user").unwrap_or_default(),
            password: cfg_str(v, "password").unwrap_or_default(),
            tls: kit::bool_param(v, "tls", false),
            container: cfg_str(v, "container"),
        }
    }
    fn cli(&self) -> Cmd {
        let mut c = Cmd::new("redis-cli").container(self.container.clone());
        if !self.socket.is_empty() { c = c.args(["-s", &self.socket]); } else { c = c.args(["-h", &self.host, "-p", &self.port.to_string()]); }
        if !self.user.is_empty() { c = c.args(["--user", &self.user]); }
        if !self.password.is_empty() { c = c.env("REDISCLI_AUTH", &self.password); }
        if self.tls { c = c.args(["--tls", "--insecure"]); }
        c
    }
    async fn cmd(&self, args: &[&str], log: &LogFn) -> Result<String> {
        let out = kit::output(&self.cli().args(args.iter().copied()), log).await?;
        if out.starts_with("ERR") || out.starts_with("NOAUTH") || out.starts_with("WRONGPASS") || out.starts_with("NOPERM") { bail!("redis: {}", out.trim()); }
        Ok(out)
    }
    async fn info(&self, log: &LogFn) -> Result<std::collections::HashMap<String, String>> {
        let out = self.cmd(&["INFO"], log).await?;
        Ok(out.lines().filter_map(|l| l.trim().split_once(':')).map(|(a, b)| (a.to_string(), b.to_string())).collect())
    }
    async fn config(&self, k: &str, log: &LogFn) -> Option<String> {
        let out = self.cmd(&["CONFIG", "GET", k], log).await.ok()?;
        let l: Vec<&str> = out.lines().collect();
        (l.len() >= 2 && l[0] == k).then(|| l[1].to_string())
    }
}

/// Значение параметра из redis.conf (последнее вхождение).
fn conf_value(text: &str, k: &str) -> Option<String> {
    text.lines().filter_map(|l| { let l = l.trim(); let (a, b) = l.split_once(char::is_whitespace)?; (a.eq_ignore_ascii_case(k)).then(|| b.trim().trim_matches('"').to_string()) }).last()
}

/// Вывод `redis-cli --rdb -`: в режиме «EOF-метки» (бездисковая репликация) в конце потока остаются 40 байт
/// метки — на файл redis-cli их обрезает, в stdout — нет. Последние 40 байт придерживаются и отбрасываются,
/// если redis-cli сообщил о таком режиме.
struct StripMarker<R> { inner: R, pend: Vec<u8>, eof: bool, marker: Arc<AtomicBool>, err_done: Arc<AtomicBool> }

const MARK: usize = 40;

impl<R: Read> Read for StripMarker<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pend.len() > MARK {
                let n = (self.pend.len() - MARK).min(out.len());
                out[..n].copy_from_slice(&self.pend[..n]);
                self.pend.drain(..n);
                return Ok(n);
            }
            if self.eof {
                if self.pend.is_empty() { return Ok(0); }
                // Дождаться, пока прочитан stderr (сообщение о режиме приходит до данных).
                for _ in 0..100 { if self.err_done.load(Ordering::SeqCst) { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }
                if self.marker.load(Ordering::SeqCst) { self.pend.clear(); return Ok(0); }
                let n = self.pend.len().min(out.len());
                out[..n].copy_from_slice(&self.pend[..n]);
                self.pend.drain(..n);
                return Ok(n);
            }
            let mut b = vec![0u8; 1 << 16];
            let n = self.inner.read(&mut b)?;
            if n == 0 { self.eof = true; } else { self.pend.extend_from_slice(&b[..n]); }
        }
    }
}

async fn stream_rdb(w: &mut crate::store::repo::SnapshotWriter, c: &Conn, v7: bool, log: &LogFn) -> Result<u64> {
    if !v7 {
        // До Redis 7 вывод в stdout не поддерживается — через файл.
        let wd = WorkDir::new(None, None)?;
        let f = wd.join("dump.rdb");
        if c.container.is_some() {
            let tmp = format!("/tmp/hopebackup-{}.rdb", crate::util::short_id());
            kit::run(&c.cli().args(["--rdb", &tmp]), log).await?;
            let e = kit::stream_cmd(w, "dump.rdb", &Cmd::new("cat").arg(&tmp).container(c.container.clone()), log).await;
            let _ = kit::run(&Cmd::new("rm").args(["-f", &tmp]).container(c.container.clone()), log).await;
            return Ok(e?.size);
        }
        kit::run(&c.cli().args(["--rdb", &f.to_string_lossy()]), log).await?;
        let e = w.add_stream("dump.rdb", std::fs::File::open(&f)?, None).await?;
        return Ok(e.size);
    }
    let cmd = c.cli().args(["--rdb", "-"]);
    log(&format!("$ {}", cmd.display()));
    let mut x = cmd.std(false);
    x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = x.spawn().context("запуск redis-cli")?;
    let (marker, err_done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (m2, d2) = (marker.clone(), err_done.clone());
    let mut err = ch.stderr.take().unwrap();
    let et = std::thread::spawn(move || {
        let mut s = String::new();
        let mut b = [0u8; 4096];
        loop {
            match err.read(&mut b) { Ok(0) | Err(_) => break, Ok(n) => { s.push_str(&String::from_utf8_lossy(&b[..n])); if s.contains("EOF marker") { m2.store(true, Ordering::SeqCst); } } }
        }
        d2.store(true, Ordering::SeqCst);
        s
    });
    let r = StripMarker { inner: ch.stdout.take().unwrap(), pend: Vec::new(), eof: false, marker, err_done };
    let res = w.add_stream("dump.rdb", r, None).await;
    let st = tokio::task::spawn_blocking(move || ch.wait()).await??;
    let err = et.join().unwrap_or_default();
    if !st.success() || !err.contains("success") { bail!("redis-cli --rdb: {}", err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ошибка")); }
    let e = res?;
    log(&format!("  dump.rdb: {}", crate::util::human_bytes(e.size)));
    Ok(e.size)
}

fn service_of(cfg: &Value) -> Option<String> {
    cfg_str(cfg, "service").or_else(|| ["redis-server", "redis", "valkey-server", "valkey", "keydb-server"].iter().find(|s| svc::exists(s)).map(|s| s.to_string()))
}

#[async_trait]
impl Plugin for Redis {
    fn id(&self) -> &'static str { "redis" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let c = Conn::from(cfg);
        let i = c.info(log).await?;
        Ok(json!({
            "version": i.get("redis_version").or(i.get("valkey_version")), "mode": i.get("redis_mode"), "role": i.get("role"),
            "used_memory": i.get("used_memory_human"), "aof": i.get("aof_enabled").map(|x| x == "1"), "rdb_last_save": i.get("rdb_last_save_time"),
            "config_file": i.get("config_file"), "dir": c.config("dir", log).await, "keys": i.iter().filter(|(k, _)| k.starts_with("db")).map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>(),
            "service": service_of(cfg),
        }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let c = Conn::from(&ctx.config);
        let i = c.info(log).await.context("подключение к Redis")?;
        let ver = i.get("redis_version").cloned().unwrap_or_default();
        if i.get("redis_mode").map(|m| m == "cluster").unwrap_or(false) { log("! Redis Cluster: копируется только этот узел (его слоты) — добавьте экземпляры для остальных мастеров"); }
        let major: u32 = ver.split('.').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let keys: u64 = i.iter().filter(|(k, _)| k.starts_with("db") && k[2..].chars().all(|c| c.is_ascii_digit())).filter_map(|(_, v)| v.split(',').next()?.strip_prefix("keys=")?.parse::<u64>().ok()).sum();
        let config_file = cfg_str(&ctx.config, "config_file").or_else(|| i.get("config_file").cloned().filter(|s| !s.is_empty()));
        let conf_text = config_file.as_ref().filter(|_| c.container.is_none()).and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        let get = |k: &'static str| { let (c, t, log) = (c.clone(), conf_text.clone(), log.clone()); async move { match c.config(k, &log).await { Some(v) => Some(v), None => conf_value(&t, k) } } };
        let dir = get("dir").await.unwrap_or_else(|| "/var/lib/redis".into());
        let dbfilename = get("dbfilename").await.unwrap_or_else(|| "dump.rdb".into());
        let aof = i.get("aof_enabled").map(|x| x == "1").unwrap_or(false);
        let appenddirname = get("appenddirname").await.unwrap_or_else(|| "appendonlydir".into());
        let appendfilename = get("appendfilename").await.unwrap_or_else(|| "appendonly.aof".into());
        let aclfile = get("aclfile").await.filter(|s| !s.is_empty());
        log(&format!("Redis {ver} · ключей {keys} · каталог {dir} · AOF {}", if aof { "включён" } else { "выключен" }));
        let mut w = ctx.repo.writer();
        let size = stream_rdb(&mut w, &c, major >= 7, log).await?;
        let mut conf_paths: Vec<String> = Vec::new();
        if c.container.is_none() {
            conf_paths.extend(config_file.iter().cloned());
            conf_paths.extend(aclfile.iter().cloned());
            conf_paths.retain(|p| Path::new(p).exists());
            if !conf_paths.is_empty() { kit::tar_paths(&mut w, "config.tar", &conf_paths, Default::default(), Default::default(), log).await?; }
        }
        let owner = std::fs::metadata(&dir).ok().map(|m| { use std::os::unix::fs::MetadataExt; m.uid() }).and_then(crate::util::user_name);
        let meta = json!({ "version": ver, "keys": keys, "dir": dir, "dbfilename": dbfilename, "aof": aof, "appenddirname": appenddirname, "appendfilename": appendfilename,
            "config_file": config_file, "aclfile": aclfile, "config": conf_paths, "owner": owner, "rdb_bytes": size, "major": major });
        let (header, stats) = w.finish(&ctx.instance, "redis", "full", json!({ "version": ver, "keys": keys }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let meta = &m.meta;
        let s = kit::stream(&m, "dump.rdb")?;
        if let Some(d) = cfg_str(&ctx.params, "target_dir") {
            std::fs::create_dir_all(&d)?;
            kit::to_file(&ctx.repo, s, &Path::new(&d).join("dump.rdb")).await?;
            if let Ok(cs) = kit::stream(&m, "config.tar") { kit::untar(&ctx.repo, cs, Path::new(&d), Default::default(), log).await?; }
            log(&format!("dump.rdb → {d}"));
            return Ok(json!({ "target_dir": d }));
        }
        let c = Conn::from(&ctx.params);
        let dir = cfg_str(&ctx.params, "dir").or_else(|| meta["dir"].as_str().map(|x| x.to_string())).ok_or_else(|| anyhow!("каталог данных неизвестен — укажите его"))?;
        let dbf = meta["dbfilename"].as_str().unwrap_or("dump.rdb").to_string();
        // AOF — по текущему состоянию цели, если она отвечает; иначе — как в копии.
        let aof = match c.info(log).await { Ok(i) => i.get("aof_enabled").map(|x| x == "1").unwrap_or(false), Err(_) => meta["aof"] == true };
        let major = meta["major"].as_u64().unwrap_or(7);
        let (adir, afile) = (meta["appenddirname"].as_str().unwrap_or("appendonlydir").to_string(), meta["appendfilename"].as_str().unwrap_or("appendonly.aof").to_string());
        let wd = WorkDir::new(None, None)?;
        let staged = wd.join("stage");
        std::fs::create_dir_all(&staged)?;
        kit::to_file(&ctx.repo, s, &staged.join(&dbf)).await?;
        if aof {
            if major >= 7 {
                let ad = staged.join(&adir);
                std::fs::create_dir_all(&ad)?;
                std::fs::copy(staged.join(&dbf), ad.join(format!("{afile}.1.base.rdb")))?;
                std::fs::write(ad.join(format!("{afile}.manifest")), format!("file {afile}.1.base.rdb seq 1 type b\n"))?;
            } else {
                std::fs::copy(staged.join(&dbf), staged.join(&afile))?;
            }
            log("AOF включён: копия станет базой AOF (иначе сервер загрузил бы прежний AOF)");
        }
        let start = kit::bool_param(&ctx.params, "start", true);
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut moved = Vec::new();
        match &c.container {
            Some(ct) => {
                kit::run(&Cmd::new("docker").args(["stop", ct]), log).await?;
                // Прежний AOF перекрывается манифестом с одной базой; dump.rdb заменяется.
                kit::run(&Cmd::new("docker").args(["cp", &format!("{}/.", staged.display()), &format!("{ct}:{dir}")]), log).await?;
                if start { kit::run(&Cmd::new("docker").args(["start", ct]), log).await?; }
            }
            None => {
                let service = service_of(&ctx.params).ok_or_else(|| anyhow!("не найдена служба Redis — укажите «Служба»"))?;
                if svc::active(&service) { svc::stop(&service, log).await?; }
                for n in [dbf.clone(), adir.clone(), afile.clone()] {
                    let p = Path::new(&dir).join(&n);
                    if p.exists() {
                        let a = kit::aside_name(&p.to_string_lossy(), &ts);
                        std::fs::rename(&p, &a)?;
                        moved.push(a);
                    }
                }
                std::fs::create_dir_all(&dir)?;
                for e in std::fs::read_dir(&staged)?.flatten() {
                    let dst = Path::new(&dir).join(e.file_name());
                    if e.path().is_dir() { copy_dir(&e.path(), &dst)?; } else { std::fs::copy(e.path(), &dst)?; }
                }
                let owner = meta["owner"].as_str().map(|s| s.to_string()).filter(|u| crate::util::user_ids(u).is_some()).or_else(|| ["redis", "valkey"].iter().find(|u| crate::util::user_ids(u).is_some()).map(|s| s.to_string()));
                if let Some(o) = owner {
                    let mut ps = vec![Path::new(&dir).join(&dbf).to_string_lossy().to_string()];
                    if aof { ps.push(Path::new(&dir).join(if major >= 7 { &adir } else { &afile }).to_string_lossy().to_string()); }
                    kit::chown_to(&ps, &o, log)?;
                }
                if kit::bool_param(&ctx.params, "restore_config", false) {
                    if let Ok(cs) = kit::stream(&m, "config.tar") { kit::untar(&ctx.repo, cs, Path::new("/"), Default::default(), log).await?; }
                }
                if start { svc::start(&service, log).await?; }
            }
        }
        let mut keys = Value::Null;
        if start {
            for _ in 0..40 {
                if let Ok(o) = c.cmd(&["DBSIZE"], log).await { if let Ok(n) = o.trim().trim_start_matches("(integer) ").parse::<u64>() { keys = json!(n); break; } }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            log(&format!("ключей после запуска: {} (в копии {})", keys, meta["keys"]));
        }
        Ok(json!({ "dir": dir, "aof": aof, "keys": keys, "keys_in_backup": meta["keys"], "moved_aside": moved }))
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let d = dst.join(e.file_name());
        if e.path().is_dir() { copy_dir(&e.path(), &d)?; } else { std::fs::copy(e.path(), &d)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marker_and_conf() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        for marker in [true, false] {
            let r = StripMarker { inner: std::io::Cursor::new(data.clone()), pend: Vec::new(), eof: false, marker: Arc::new(AtomicBool::new(marker)), err_done: Arc::new(AtomicBool::new(true)) };
            let mut out = Vec::new();
            std::io::BufReader::new(r).read_to_end(&mut out).unwrap();
            assert_eq!(out.len(), if marker { 960 } else { 1000 });
            assert_eq!(&out[..], &data[..out.len()]);
        }
        assert_eq!(conf_value("dir /var/lib/redis\n# x\ndbfilename \"d.rdb\"\ndir /srv/r\n", "dir").as_deref(), Some("/srv/r"));
    }
}
