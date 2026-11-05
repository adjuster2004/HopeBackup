//! GitLab (Omnibus или Docker-образ gitlab/gitlab-ee|ce): `gitlab-backup create` + конфигурация и
//! секреты (`gitlab.rb`, `gitlab-secrets.json` — без них не расшифровать CI-переменные, токены, 2FA).
//!
//! Чтобы дедупликация работала между копиями, архив GitLab пишется без сжатия (`COMPRESS_CMD=cat`,
//! GitLab 16.7+) или с `GZIP_RSYNCABLE=yes` (старые версии): неизменённые репозитории и файлы дают те
//! же блоки. Локальный архив после отправки удаляется.
//!
//! Восстановление — строго в ту же версию GitLab (так требует gitlab-backup). На чистую машину
//! GitLab нужной версии можно поставить из packages.gitlab.com (параметр `install`).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;

use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Gitlab;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "mode": { "type": "string", "title": "Установка", "enum": ["omnibus", "docker"], "default": "omnibus", "description": "omnibus — пакет gitlab-ee/ce; docker — контейнер gitlab/gitlab-ee|ce" },
        "container": { "type": "string", "title": "Контейнер (для docker)", "default": "gitlab" },
        "backup_dir": { "type": "string", "title": "Каталог архивов GitLab", "default": "/var/opt/gitlab/backups", "description": "gitlab_rails['backup_path']; для docker — путь внутри контейнера" },
        "skip": { "type": "array", "items": { "type": "string" }, "title": "Не включать (SKIP)", "description": "registry, artifacts, lfs, builds, pages, uploads, packages, terraform_state, ci_secure_files" },
        "config_paths": { "type": "array", "items": { "type": "string" }, "title": "Конфигурация и секреты", "description": "По умолчанию /etc/gitlab/gitlab.rb, gitlab-secrets.json, ssl, trusted-certs" },
        "extra_env": { "type": "string", "title": "Доп. переменные gitlab-backup", "description": "Например: GITLAB_BACKUP_MAX_CONCURRENCY=4 GITLAB_BACKUP_MAX_STORAGE_CONCURRENCY=2" },
        "keep_local": { "type": "boolean", "title": "Оставлять архив GitLab на сервере", "default": false }
    } })
}

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "mode": { "type": "string", "title": "Установка на цели", "enum": ["omnibus", "docker"], "default": "omnibus" },
        "container": { "type": "string", "title": "Контейнер (для docker)", "default": "gitlab" },
        "install": { "type": "boolean", "title": "Установить GitLab той же версии, если его нет", "default": false, "description": "Из packages.gitlab.com (нужен доступ в интернет); только omnibus" },
        "external_url": { "type": "string", "title": "external_url для установки", "description": "После восстановления gitlab.rb возьмётся из копии" },
        "restore_config": { "type": "boolean", "title": "Восстановить gitlab.rb и gitlab-secrets.json", "default": true },
        "check": { "type": "boolean", "title": "Проверить (gitlab:check, doctor:secrets)", "default": true }
    } })
}

pub fn defaults() -> Value {
    json!({ "mode": "omnibus", "container": "gitlab", "backup_dir": "/var/opt/gitlab/backups", "skip": [], "config_paths": ["/etc/gitlab/gitlab.rb", "/etc/gitlab/gitlab-secrets.json", "/etc/gitlab/ssl", "/etc/gitlab/trusted-certs"], "keep_local": false })
}

struct Conf {
    docker: bool,
    container: String,
    backup_dir: String,
    skip: Vec<String>,
    config_paths: Vec<String>,
    extra_env: Vec<(String, String)>,
    keep_local: bool,
}

fn conf(v: &Value) -> Result<Conf> {
    let list = |k: &str| crate::connectors::net::p_list(v, k);
    let mut config_paths = list("config_paths");
    if config_paths.is_empty() { config_paths = vec!["/etc/gitlab/gitlab.rb".into(), "/etc/gitlab/gitlab-secrets.json".into(), "/etc/gitlab/ssl".into(), "/etc/gitlab/trusted-certs".into()]; }
    let mut extra_env = Vec::new();
    for kv in cfg_str(v, "extra_env").unwrap_or_default().split_whitespace() {
        let (k, val) = kv.split_once('=').ok_or_else(|| anyhow!("extra_env: ожидается ИМЯ=значение, получено «{kv}»"))?;
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { bail!("extra_env: недопустимое имя «{k}»"); }
        extra_env.push((k.to_string(), val.to_string()));
    }
    let skip = list("skip");
    for s in &skip { if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { bail!("skip: недопустимое значение «{s}»"); } }
    Ok(Conf {
        docker: cfg_str(v, "mode").as_deref() == Some("docker"),
        container: cfg_str(v, "container").unwrap_or_else(|| "gitlab".into()),
        backup_dir: cfg_str(v, "backup_dir").unwrap_or_else(|| "/var/opt/gitlab/backups".into()).trim_end_matches('/').to_string(),
        skip,
        config_paths,
        extra_env,
        keep_local: v.get("keep_local").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

/// Команда GitLab на хосте или в контейнере.
fn cmd(c: &Conf, prog: &str, args: &[&str], env: &[(String, String)], stdin: bool) -> tokio::process::Command {
    if c.docker {
        let mut x = tokio::process::Command::new("docker");
        x.arg("exec");
        if stdin { x.arg("-i"); }
        for (k, v) in env { x.arg("-e").arg(format!("{k}={v}")); }
        x.arg(&c.container).arg(prog).args(args);
        x
    } else {
        let mut x = tokio::process::Command::new(prog);
        x.args(args);
        for (k, v) in env { x.env(k, v); }
        x
    }
}

/// Выполнить с построчным выводом в журнал задания.
async fn run(mut x: tokio::process::Command, log: &LogFn, what: &str) -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    x.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = x.spawn().with_context(|| format!("запуск: {what}"))?;
    let (out, err) = (ch.stdout.take().unwrap(), ch.stderr.take().unwrap());
    let l2 = log.clone();
    let et = tokio::spawn(async move {
        let mut r = BufReader::new(err).lines();
        let mut tail = Vec::new();
        while let Ok(Some(l)) = r.next_line().await { if !l.trim().is_empty() { l2(&format!("  {l}")); tail.push(l); if tail.len() > 8 { tail.remove(0); } } }
        tail
    });
    let mut r = BufReader::new(out).lines();
    let mut all = String::new();
    while let Some(l) = r.next_line().await? {
        if !l.trim().is_empty() { log(&format!("  {l}")); }
        all.push_str(&l);
        all.push('\n');
    }
    let st = ch.wait().await?;
    let tail = et.await.unwrap_or_default();
    if !st.success() { bail!("{what}: код {} {}", st.code().unwrap_or(-1), tail.join(" | ")); }
    Ok(all)
}

/// Версия и редакция: первая строка /opt/gitlab/version-manifest.txt («gitlab-ee 16.11.1»).
async fn version(c: &Conf) -> Result<(String, String)> {
    let out = if c.docker {
        let mut x = cmd(c, "head", &["-1", "/opt/gitlab/version-manifest.txt"], &[], false);
        crate::util::run(&mut x).await?
    } else {
        std::fs::read_to_string("/opt/gitlab/version-manifest.txt").context("GitLab не найден: нет /opt/gitlab/version-manifest.txt")?
    };
    parse_manifest(&out).ok_or_else(|| anyhow!("не удалось разобрать версию GitLab: {}", out.lines().next().unwrap_or("")))
}

pub fn parse_manifest(s: &str) -> Option<(String, String)> {
    let l = s.lines().next()?.trim();
    let (ed, ver) = l.split_once(' ')?;
    let ed = ed.strip_prefix("gitlab-")?.to_string();
    let ver = ver.split_whitespace().next()?.to_string();
    if !ver.chars().next()?.is_ascii_digit() { return None; }
    Some((ed, ver))
}

/// GitLab 16.7+ умеет COMPRESS_CMD / DECOMPRESS_CMD.
pub fn has_compress_cmd(ver: &str) -> bool {
    let mut it = ver.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    a > 16 || (a == 16 && b >= 7)
}

#[async_trait]
impl Plugin for Gitlab {
    fn id(&self) -> &'static str { "gitlab" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let c = conf(cfg)?;
        let (ed, ver) = version(&c).await?;
        let status = crate::util::run(&mut cmd(&c, "gitlab-ctl", &["status"], &[], false)).await.unwrap_or_default();
        let services: Vec<Value> = status.lines().filter_map(|l| { let st = l.split(':').next()?.trim(); let name = l.split(':').nth(1)?.trim(); Some(json!({ "service": name, "state": st })) }).collect();
        Ok(json!({ "edition": ed, "version": ver, "mode": if c.docker { "docker" } else { "omnibus" }, "compress_cmd": has_compress_cmd(&ver), "services": services }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let c = conf(&ctx.config)?;
        let log = &ctx.log;
        let (ed, ver) = version(&c).await?;
        let name = format!("hb-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));
        let plain = has_compress_cmd(&ver);
        let mut env = vec![("GZIP_RSYNCABLE".to_string(), "yes".to_string())];
        if plain { env.push(("COMPRESS_CMD".into(), "cat".into())); }
        env.extend(c.extra_env.clone());
        let mut args = vec![format!("BACKUP={name}"), "STRATEGY=copy".to_string()];
        if !c.skip.is_empty() { args.push(format!("SKIP={}", c.skip.join(","))); }
        log(&format!("GitLab {ed} {ver}: gitlab-backup create ({})", if plain { "без сжатия — для дедупликации" } else { "gzip --rsyncable" }));
        let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut create = cmd(&c, "gitlab-backup", &["create"], &env, false);
        create.args(&a);
        run(create, log, "gitlab-backup create").await?;
        let tar = format!("{}/{name}_gitlab_backup.tar", c.backup_dir);
        let mut w = ctx.repo.writer();
        // Архив GitLab — потоком в репозиторий (с хоста или из контейнера).
        let res: Result<()> = async {
            if c.docker {
                let mut x = cmd(&c, "cat", &[&tar], &[], false);
                x.stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut ch = x.spawn().context("docker exec cat")?;
                let out = ch.stdout.take().unwrap();
                let rd = tokio_util::io::SyncIoBridge::new(out);
                w.add_stream("gitlab_backup.tar", rd, None).await?;
                let st = ch.wait().await?;
                if !st.success() { bail!("чтение {tar} из контейнера: код {}", st.code().unwrap_or(-1)); }
            } else {
                let f = std::fs::File::open(&tar).with_context(|| format!("архив GitLab {tar}"))?;
                w.add_stream("gitlab_backup.tar", f, None).await?;
            }
            // Конфигурация и секреты.
            let existing: Vec<String> = if c.docker { c.config_paths.clone() } else { c.config_paths.iter().filter(|p| std::path::Path::new(p).exists()).cloned().collect() };
            if c.docker {
                let rel: Vec<String> = existing.iter().map(|p| p.trim_start_matches('/').to_string()).collect();
                let mut args = vec!["-C", "/", "--ignore-failed-read", "-cf", "-"];
                args.extend(rel.iter().map(|s| s.as_str()));
                let mut x = cmd(&c, "tar", &args, &[], false);
                x.stdout(Stdio::piped()).stderr(Stdio::null());
                let mut ch = x.spawn()?;
                let rd = tokio_util::io::SyncIoBridge::new(ch.stdout.take().unwrap());
                w.add_stream("config.tar", rd, None).await?;
                let _ = ch.wait().await;
            } else if !existing.is_empty() {
                let (r, wp) = std::io::pipe()?;
                let paths = existing.clone();
                let prod = std::thread::spawn(move || super::files::write_tar(wp, &paths, &crate::exclude::Excludes::default()));
                w.add_stream("config.tar", r, None).await?;
                prod.join().map_err(|_| anyhow!("поток tar"))??;
            }
            if !existing.iter().any(|p| p.ends_with("gitlab-secrets.json")) { log("⚠ gitlab-secrets.json не найден — без него CI-переменные, токены и 2FA после восстановления не расшифровать"); }
            Ok(())
        }
        .await;
        if !c.keep_local {
            let _ = if c.docker { crate::util::run(&mut cmd(&c, "rm", &["-f", &tar], &[], false)).await.map(|_| ()) } else { std::fs::remove_file(&tar).map_err(Into::into) };
        }
        res?;
        let meta = json!({ "edition": ed, "version": ver, "backup_name": name, "compress": if plain { "none" } else { "gzip" }, "skip": c.skip, "config_paths": c.config_paths });
        let (header, stats) = w.finish(&ctx.instance, "gitlab", "full", json!({ "version": ver, "edition": ed }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let p = &ctx.params;
        let c = conf(p)?;
        let log = &ctx.log;
        let (header, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let ed = m.meta["edition"].as_str().unwrap_or("ee").to_string();
        let ver = m.meta["version"].as_str().ok_or_else(|| anyhow!("в копии нет версии GitLab"))?.to_string();
        let name = m.meta["backup_name"].as_str().unwrap_or("hb-restore").to_string();
        let plain = m.meta["compress"] == "none";
        log(&format!("копия GitLab {ed} {ver} от {}", header.time.format("%Y-%m-%d %H:%M UTC")));
        // GitLab на цели — той же версии (или поставить).
        match version(&c).await {
            Ok((_, tv)) if tv == ver => log(&format!("на цели GitLab {tv}")),
            Ok((_, tv)) => bail!("на цели GitLab {tv}, а копия сделана в {ver}: gitlab-backup восстанавливает только в ту же версию"),
            Err(_) if !c.docker && p.get("install").and_then(|v| v.as_bool()).unwrap_or(false) => install(&ed, &ver, cfg_str(p, "external_url").as_deref(), log).await?,
            Err(e) => bail!("{e:#} — установите GitLab {ed} {ver} или включите «Установить GitLab»"),
        }
        let restore_config = p.get("restore_config").and_then(|v| v.as_bool()).unwrap_or(true);
        if restore_config {
            if let Some(s) = m.streams.iter().find(|s| s.name == "config.tar") {
                log("восстановление gitlab.rb, gitlab-secrets.json и сертификатов");
                if c.docker {
                    let mut x = cmd(&c, "tar", &["-C", "/", "-xf", "-"], &[], true);
                    pipe_stream(ctx, s, &mut x).await?;
                } else {
                    let (reader, fetch) = ctx.repo.stream_reader(s)?;
                    let u = tokio::task::spawn_blocking(move || -> Result<()> { let mut a = tar::Archive::new(reader); a.set_preserve_permissions(true); a.set_preserve_ownerships(true); a.unpack("/")?; Ok(()) });
                    let (u, f) = tokio::join!(u, fetch);
                    u??; f??;
                }
                run(cmd(&c, "gitlab-ctl", &["reconfigure"], &[], false), log, "gitlab-ctl reconfigure").await?;
            }
        }
        for svc in ["puma", "sidekiq"] { run(cmd(&c, "gitlab-ctl", &["stop", svc], &[], false), log, &format!("gitlab-ctl stop {svc}")).await?; }
        let s = m.streams.iter().find(|s| s.name == "gitlab_backup.tar").ok_or_else(|| anyhow!("в копии нет архива GitLab"))?;
        let tar = format!("{}/{name}_gitlab_backup.tar", c.backup_dir);
        log(&format!("архив → {tar}"));
        if c.docker {
            let mut x = cmd(&c, "sh", &["-c", &format!("mkdir -p {d} && cat > {t} && chown git:git {t}", d = crate::util::shq(&c.backup_dir), t = crate::util::shq(&tar))], &[], true);
            pipe_stream(ctx, s, &mut x).await?;
        } else {
            std::fs::create_dir_all(&c.backup_dir)?;
            let f = std::fs::File::create(&tar)?;
            ctx.repo.restore_stream(s, f).await?;
            if let Some((u, g)) = crate::util::user_ids("git") { let _ = std::os::unix::fs::chown(&tar, Some(u), Some(g)); }
        }
        let mut env = vec![("GITLAB_ASSUME_YES".to_string(), "1".to_string())];
        if plain { env.push(("DECOMPRESS_CMD".into(), "cat".into())); }
        let mut x = cmd(&c, "gitlab-backup", &["restore"], &env, false);
        x.arg(format!("BACKUP={name}")).arg("force=yes");
        let r = run(x, log, "gitlab-backup restore").await;
        if !c.keep_local { let _ = if c.docker { crate::util::run(&mut cmd(&c, "rm", &["-f", &tar], &[], false)).await.map(|_| ()) } else { std::fs::remove_file(&tar).map_err(Into::into) }; }
        r?;
        run(cmd(&c, "gitlab-ctl", &["restart"], &[], false), log, "gitlab-ctl restart").await?;
        let mut checks = json!({});
        if p.get("check").and_then(|v| v.as_bool()).unwrap_or(true) {
            for (k, args) in [("secrets", vec!["gitlab:doctor:secrets"]), ("check", vec!["gitlab:check", "SANITIZE=true"])] {
                match run(cmd(&c, "gitlab-rake", &args, &[], false), log, &format!("gitlab-rake {}", args[0])).await {
                    Ok(_) => checks[k] = json!("ok"),
                    Err(e) => { log(&format!("⚠ {e:#}")); checks[k] = json!(format!("{e:#}")); }
                }
            }
        }
        Ok(json!({ "version": ver, "edition": ed, "backup": name, "checks": checks }))
    }
}

/// Поток снапшота — в stdin команды.
async fn pipe_stream(ctx: &RestoreCtx, s: &crate::store::types::StreamEntry, x: &mut tokio::process::Command) -> Result<()> {
    x.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut ch = x.spawn()?;
    let stdin = ch.stdin.take().unwrap();
    let w = tokio_util::io::SyncIoBridge::new(stdin);
    let r = ctx.repo.restore_stream(s, w).await;
    let out = ch.wait_with_output().await?;
    r?;
    if !out.status.success() { bail!("код {}: {}", out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

/// Установить GitLab точной версии из packages.gitlab.com (чистая машина при восстановлении).
async fn install(ed: &str, ver: &str, external_url: Option<&str>, log: &LogFn) -> Result<()> {
    if !matches!(ed, "ee" | "ce") { bail!("неизвестная редакция GitLab «{ed}»"); }
    if !ver.chars().all(|c| c.is_ascii_digit() || c == '.') { bail!("неверная версия «{ver}»"); }
    let url = external_url.unwrap_or("http://localhost");
    let script = format!(r#"set -e
export DEBIAN_FRONTEND=noninteractive EXTERNAL_URL={url}
if command -v apt-get >/dev/null 2>&1; then
  apt-get update -q && apt-get install -y -q curl ca-certificates
  curl -fsSL https://packages.gitlab.com/install/repositories/gitlab/gitlab-{ed}/script.deb.sh | bash
  apt-get install -y -q gitlab-{ed}={ver}-{ed}.0
else
  PM=$(command -v dnf || command -v yum)
  curl -fsSL https://packages.gitlab.com/install/repositories/gitlab/gitlab-{ed}/script.rpm.sh | bash
  $PM install -y gitlab-{ed}-{ver}-{ed}.0.el$(rpm -E %rhel)
fi
"#, url = crate::util::shq(url));
    log(&format!("установка GitLab {ed} {ver} из packages.gitlab.com"));
    let mut x = tokio::process::Command::new("sh");
    x.arg("-c").arg(script);
    run(x, log, "установка GitLab").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest() {
        assert_eq!(parse_manifest("gitlab-ee 16.11.1\n\nComponent ...").unwrap(), ("ee".into(), "16.11.1".into()));
        assert_eq!(parse_manifest("gitlab-ce 15.11.13").unwrap(), ("ce".into(), "15.11.13".into()));
        assert!(parse_manifest("garbage").is_none());
        assert!(has_compress_cmd("16.7.0") && has_compress_cmd("17.2.1") && !has_compress_cmd("16.6.9") && !has_compress_cmd("15.11.13"));
    }
}
