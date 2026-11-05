//! «Сервер Linux целиком»: опись того, что работает на сервере, и копия всего, что нужно, чтобы
//! поднять его заново: /etc, пользователи с паролями и ключами SSH, домашние каталоги, свои скрипты
//! и программы (найденные по процессам, cron, таймерам и службам systemd), compose-проекты Docker,
//! отмеченные администратором диски и сетевые каталоги, список пакетов.
//!
//! Потоки снапшота:
//! * `inventory.json` — опись (что найдено и откуда);
//! * `accounts.json` — passwd, shadow, group, gshadow целиком;
//! * `system.tar` — /etc, /root, /usr/local (bin, sbin, etc, lib/systemd), спулы cron и at;
//! * `homes.tar` — домашние каталоги пользователей (или только .ssh и профили — в system.tar);
//! * `apps.tar` — каталоги и файлы приложений, дополнительные пути, compose-проекты;
//! * `docker.tar` — тома и привязанные каталоги контейнеров (если включено);
//! * `mount-<путь>.tar` — по потоку на каждый отмеченный диск или сетевой каталог.

pub mod analysis;
pub mod inventory;
pub mod system;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};
use crate::connectors::net::{p_bool, p_list};
use crate::exclude::Excludes;
use inventory::{Inventory, Opts};

pub struct Host;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "home": { "type": "string", "title": "Домашние каталоги", "enum": ["all", "ssh", "none"], "default": "all", "description": "all — целиком; ssh — только ключи SSH и профили; none — не копировать" },
            "include_root": { "type": "boolean", "title": "Каталог /root", "default": true },
            "auto_apps": { "type": "boolean", "title": "Найденные приложения и скрипты", "default": true, "description": "Каталоги программ и скриптов не из пакетов: запущенные процессы, cron, таймеры и службы systemd" },
            "app_limit_mb": { "type": "integer", "title": "Предел каталога приложения, МБ", "default": 2048, "description": "Больше — копируются только файлы запуска (каталог можно добавить в «Дополнительные пути»)" },
            "extra_paths": { "type": "array", "items": { "type": "string" }, "title": "Дополнительные пути", "description": "Каталоги данных, которые нужно сохранить вместе с сервером" },
            "mounts": { "type": "array", "items": { "type": "string" }, "title": "Диски и сетевые каталоги", "description": "Точки монтирования для копирования (по умолчанию отдельные диски и NFS/SMB не копируются) — список в описи сервера" },
            "docker": { "type": "string", "title": "Docker", "enum": ["compose", "volumes", "none"], "default": "compose", "description": "compose — файлы проектов; volumes — ещё и тома с привязанными каталогами" },
            "skip_db_data": { "type": "boolean", "title": "Не копировать каталоги данных СУБД", "default": true, "description": "Файлы работающей СУБД несогласованны — для неё добавьте отдельный продукт (PostgreSQL и т. д.)" },
            "exclude": { "type": "array", "items": { "type": "string" }, "title": "Исключения", "description": "/var/log — путь целиком; *.log, node_modules — по имени на любой глубине" },
            "use_global_exclude": { "type": "boolean", "title": "Общие исключения сервера", "default": true, "description": "Добавить список из «Настройки → Исключения»" },
            "snapshot": { "type": "string", "title": "Снимок файловой системы", "enum": ["auto", "required", "off"], "default": "auto", "description": "auto — читать из снимков LVM/ZFS/btrfs, где возможно (все файлы в одном состоянии), иначе «на ходу»; required — без снимков не копировать; off — без снимков" },
            "snapshot_size_pct": { "type": "integer", "title": "Размер снимка LVM, % тома", "default": 10, "description": "Место под изменения на время копии, не меньше 1 ГиБ на том; свободное место нужно в группе томов — сколько, подскажет анализ машины" },
            "pre_snapshot": { "type": "string", "title": "Команда перед снимком", "description": "Например, сбросить буферы приложения на диск" },
            "post_snapshot": { "type": "string", "title": "Команда после снимка", "description": "Выполняется сразу после создания снимков (даже при ошибке)" },
            "max_file_mb": { "type": "integer", "title": "Не копировать файлы больше, МБ", "description": "Пусто — без ограничения" }
        }
    })
}

pub fn defaults() -> Value {
    json!({ "home": "all", "include_root": true, "auto_apps": true, "app_limit_mb": 2048, "extra_paths": [], "mounts": [], "docker": "compose", "skip_db_data": true, "exclude": [], "use_global_exclude": true })
}

pub fn restore_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "title": "Что восстановить", "enum": ["users", "files", "full"], "default": "users", "description": "users — пользователи, пароли, ключи SSH, sudo; files — выбранные части в каталог; full — сервер целиком на новой машине" },
            "users": { "type": "array", "items": { "type": "string" }, "title": "Пользователи", "description": "Пусто — все обычные пользователи (UID 1000+)" },
            "set_passwords": { "type": "boolean", "title": "Пароли из копии", "default": true },
            "root_keys": { "type": "boolean", "title": "Ключи SSH пользователя root", "default": true, "description": "Дописываются к текущим authorized_keys" },
            "root_password": { "type": "boolean", "title": "Пароль root из копии", "default": false },
            "sudoers": { "type": "boolean", "title": "Правила sudo (/etc/sudoers.d)", "default": true },
            "streams": { "type": "array", "items": { "type": "string" }, "title": "Части копии", "description": "system, homes, apps, docker, mount-… (пусто — все)" },
            "only": { "type": "array", "items": { "type": "string" }, "title": "Только пути", "description": "Например /opt/app, /etc/nginx; пусто — всё из выбранных частей" },
            "root": { "type": "string", "title": "Каталог восстановления", "default": "/", "description": "Для files: / — на место, иначе — в отдельный каталог (без замены файлов системы)" },
            "host_keys": { "type": "boolean", "title": "Ключи хоста SSH (/etc/ssh/ssh_host_*)", "default": false, "description": "Включите, если новая машина заменяет старую под тем же именем" },
            "system_auth": { "type": "boolean", "title": "Заменить настройки входа (sshd_config, PAM, sudoers, nsswitch)", "default": false, "description": "Без флажка настройки входа цели сохраняются: старые могут закрыть вход на новую машину" },
            "install_packages": { "type": "boolean", "title": "Установить пакеты из описи", "default": false, "description": "Только для того же дистрибутива; нужен доступ к репозиториям" },
            "enable_services": { "type": "boolean", "title": "Включить свои службы и таймеры systemd", "default": true },
            "start_services": { "type": "boolean", "title": "Запустить их сразу", "default": false }
        }
    })
}

// ------------------------------------------------------------------ план копии

struct Plan {
    system: Vec<String>,
    homes: Vec<String>,
    apps: Vec<String>,
    docker: Vec<String>,
    mounts: Vec<String>,
    skip_dirs: HashSet<String>,
    ex: Excludes,
}

fn plan(cfg: &Value, inv: &Inventory) -> Plan {
    let home = cfg_str(cfg, "home").unwrap_or_else(|| "all".into());
    let mut system: Vec<String> = ["/etc", "/usr/local/bin", "/usr/local/sbin", "/usr/local/etc", "/usr/local/lib/systemd", "/var/spool/cron", "/var/spool/at", "/var/spool/anacron", "/var/lib/systemd/linger"].iter().map(|s| s.to_string()).collect();
    if p_bool(cfg, "include_root", true) { system.push("/root".into()); }
    let user_homes: Vec<String> = inv.users.iter().filter(|u| u.uid >= 1000 && u.login && !u.home.is_empty() && u.home != "/" && !inventory::is_system_path(&u.home) && !u.home.starts_with("/var/") && Path::new(&u.home).is_dir()).map(|u| u.home.clone()).collect();
    let mut homes = Vec::new();
    match home.as_str() {
        "all" => homes = user_homes,
        "ssh" => {
            for h in user_homes {
                for f in [".ssh", ".config/systemd", ".bashrc", ".profile", ".bash_profile", ".bash_aliases", ".zshrc"] {
                    let p = format!("{h}/{f}");
                    if Path::new(&p).exists() { system.push(p); }
                }
            }
        }
        _ => {}
    }
    // Ключи root — даже без /root целиком.
    if !p_bool(cfg, "include_root", true) && Path::new("/root/.ssh").exists() { system.push("/root/.ssh".into()); }
    let mut apps: Vec<String> = Vec::new();
    if p_bool(cfg, "auto_apps", true) {
        for a in &inv.apps { if a.include { apps.push(a.path.clone()) } else { apps.extend(a.files.clone()) } }
    }
    // Что уже попадает в system.tar или homes.tar, повторно не копируется.
    let covered: Vec<String> = system.iter().chain(homes.iter()).cloned().collect();
    apps.retain(|a| !covered.iter().any(|c| a == c || a.starts_with(&format!("{c}/"))));
    apps.extend(p_list(cfg, "extra_paths"));
    let docker_mode = cfg_str(cfg, "docker").unwrap_or_else(|| "compose".into());
    let mut docker = Vec::new();
    if docker_mode != "none" {
        for c in &inv.containers {
            if !c.compose_dir.is_empty() { apps.push(c.compose_dir.clone()); }
            if docker_mode == "volumes" {
                for m in &c.mounts {
                    let src = m.source.trim_end_matches('/');
                    // Мониторинговые контейнеры монтируют /, /proc, /sys, хранилище Docker — их не копируем.
                    let system = src.is_empty() || src == "/" || !src.starts_with('/') || inventory::is_system_path(src)
                        || ["/proc", "/sys", "/dev", "/run", "/var/run", "/var/lib/docker/overlay2", "/var/lib/docker/image", "/var/lib/containerd", "/etc", "/boot", "/var/log", "/tmp"].iter().any(|d| src == *d || (src.starts_with(&format!("{d}/")) && *d != "/etc" && *d != "/var/log" && *d != "/tmp"))
                        || src == "/var/lib/docker";
                    if !system && !docker.contains(&src.to_string()) { docker.push(src.to_string()); }
                }
            }
        }
    }
    let mounts = p_list(cfg, "mounts");
    // Не заходить в сетевые, служебные и bind-монтирования, если их не отметили.
    let skip_dirs: HashSet<String> = inv.mounts.iter().filter(|m| matches!(m.class.as_str(), "network" | "pseudo" | "docker" | "bind") && !mounts.contains(&m.target)).map(|m| m.target.clone()).collect();
    let mut ex_list = p_list(cfg, "exclude");
    if p_bool(cfg, "use_global_exclude", true) { ex_list.extend(p_list(cfg, "global_exclude")); }
    if p_bool(cfg, "skip_db_data", true) { ex_list.extend(inv.db_data_dirs.iter().cloned()); }
    // Копии копий не нужны.
    ex_list.extend(["/var/lib/hopebackup", "/var/lib/hopebackup-node", "/run/hopebackup"].iter().map(|s| s.to_string()));
    let mb = cfg.get("max_file_mb").and_then(|v| v.as_u64()).filter(|m| *m > 0);
    let ex = Excludes::new(&ex_list, mb.map(|m| m * 1024 * 1024));
    Plan { system, homes, apps, docker, mounts, skip_dirs, ex }
}

fn mount_stream(target: &str) -> String {
    let s: String = target.trim_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("mount-{}.tar", if s.is_empty() { "root".into() } else { s })
}

fn accounts() -> Value {
    let rd = |p: &str| std::fs::read_to_string(p).ok();
    json!({ "passwd": rd("/etc/passwd"), "shadow": rd("/etc/shadow"), "group": rd("/etc/group"), "gshadow": rd("/etc/gshadow") })
}

async fn inventory_of(cfg: &Value) -> Result<Inventory> {
    let limit = cfg.get("app_limit_mb").and_then(|v| v.as_u64()).unwrap_or(2048) * 1024 * 1024;
    let ex = Excludes::from_config(cfg);
    tokio::task::spawn_blocking(move || inventory::collect(&Opts { app_limit: limit, ex })).await.map_err(|e| anyhow!("опись: {e}"))
}

async fn small_stream(ctx_repo: &std::sync::Arc<crate::store::Repo>, s: &crate::store::types::StreamEntry) -> Result<Vec<u8>> {
    let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for W {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().extend_from_slice(b); Ok(b.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    ctx_repo.restore_stream(s, W(buf.clone())).await?;
    let b = buf.lock().clone();
    Ok(b)
}

#[async_trait]
impl Plugin for Host {
    fn id(&self) -> &'static str { "host" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        log("опись сервера: процессы, расписания, службы, монтирования, контейнеры, пользователи");
        let mut inv = inventory_of(cfg).await?;
        inv.method = cfg_str(cfg, "_method").unwrap_or_else(|| "agent".into());
        let p = plan(cfg, &inv);
        log(&format!(
            "процессов {}, приложений не из пакетов {}, заданий по расписанию {}, своих служб {}, монтирований {}, контейнеров {}, пользователей {}",
            inv.processes.len(), inv.apps.len(), inv.schedules.len(), inv.services.len(), inv.mounts.iter().filter(|m| m.class != "pseudo" && m.class != "docker").count(), inv.containers.len(), inv.users.len()
        ));
        for w in &inv.warnings { log(&format!("внимание: {w}")); }
        let mut v = serde_json::to_value(&inv)?;
        v["analysis"] = serde_json::to_value(analysis::analyze(&inv, inv.method == "agent"))?;
        v["plan"] = json!({ "system": p.system, "homes": p.homes, "apps": p.apps, "docker": p.docker, "mounts": p.mounts, "exclude": p.ex.list() });
        Ok(v)
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = ctx.log.clone();
        if !crate::util::is_root() { log("внимание: агент работает не от root — /etc/shadow, чужие домашние каталоги и часть файлов будут пропущены"); }
        log("опись сервера…");
        let inv = inventory_of(&ctx.config).await?;
        let p = plan(&ctx.config, &inv);
        log(&format!("найдено: приложений {}, заданий по расписанию {}, своих служб {}, пользователей {}, контейнеров {}", inv.apps.len(), inv.schedules.len(), inv.services.len(), inv.users.len(), inv.containers.len()));
        for w in &inv.warnings { log(&format!("внимание: {w}")); }
        if !p.ex.list().is_empty() { log(&format!("исключения: {}", p.ex.list().join(", "))); }
        let mut wr = ctx.repo.writer();
        wr.add_stream("inventory.json", std::io::Cursor::new(serde_json::to_vec_pretty(&inv)?), None).await?;
        wr.add_stream("accounts.json", std::io::Cursor::new(serde_json::to_vec(&accounts())?), None).await?;
        let mut parts: Vec<(String, Vec<String>)> = vec![("system.tar".into(), p.system.clone())];
        if !p.homes.is_empty() { parts.push(("homes.tar".into(), p.homes.clone())); }
        if !p.apps.is_empty() { parts.push(("apps.tar".into(), p.apps.clone())); }
        if !p.docker.is_empty() { parts.push(("docker.tar".into(), p.docker.clone())); }
        for m in &p.mounts { parts.push((mount_stream(m), vec![m.clone()])); }
        // Снимки ФС под все части копии сразу: система, домашние каталоги, приложения и диски — в одном состоянии.
        let all: Vec<String> = parts.iter().flat_map(|(_, p)| p.iter().cloned()).collect();
        let snaps = super::files::snapshot_for(&all, &ctx.config, &ctx.job_tag(), log.clone()).await?;
        let mut summary = serde_json::Map::new();
        let mut files_total = 0u64;
        let mut skipped_total = 0u64;
        for (name, paths) in parts {
            log(&format!("{name}: {}", paths.iter().take(8).cloned().collect::<Vec<_>>().join(", ") + if paths.len() > 8 { " …" } else { "" }));
            let (r, w) = std::io::pipe()?;
            let (ex, skip, remap) = (p.ex.clone(), p.skip_dirs.clone(), snaps.remap.clone());
            let paths2 = paths.clone();
            let producer = std::thread::spawn(move || super::files::write_tar_remap(w, &paths2, &ex, &skip, false, &remap));
            let res = wr.add_stream(&name, r, None).await;
            let tj = producer.join();
            // Сначала ошибка записи на площадку: «обрыв канала» у чтения файлов — лишь её следствие.
            res?;
            let ts = tj.map_err(|_| anyhow!("поток tar аварийно завершился"))??;
            log(&format!("  {}", ts.summary()));
            for e in ts.errors.iter().take(5) { log(&format!("  пропущено: {e}")); }
            files_total += ts.files;
            skipped_total += ts.skipped;
            summary.insert(name, json!({ "paths": paths, "stats": ts }));
        }
        snaps.check()?;
        let fs_snapshot = !snaps.is_empty();
        drop(snaps);
        let extra = json!({ "files": files_total, "skipped": skipped_total, "users": inv.users.len(), "apps": inv.apps.len(), "os": inv.os, "fs_snapshot": fs_snapshot });
        let meta = json!({ "parts": summary, "os": inv.os, "os_id": inv.os_id, "host": inv.host });
        let (header, stats) = wr.finish(&ctx.instance, "host", "host", extra, meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let p = &ctx.params;
        let log = ctx.log.clone();
        let mode = cfg_str(p, "mode").unwrap_or_else(|| "users".into());
        if !["users", "files", "full"].contains(&mode.as_str()) { bail!("режим восстановления: users, files или full"); }
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let find = |n: &str| m.streams.iter().find(|s| s.name == n).cloned();
        let inv: Inventory = match find("inventory.json") { Some(s) => serde_json::from_slice(&small_stream(&ctx.repo, &s).await?)?, None => Inventory::default() };
        log(&format!("копия сервера {} ({}), снята {}", inv.host, inv.os, inv.time));
        let mut report = serde_json::Map::new();
        let root = cfg_str(p, "root").unwrap_or_else(|| "/".into());
        let in_place = mode != "files" || root == "/";
        let acc: Option<Value> = match find("accounts.json") { Some(s) => Some(serde_json::from_slice(&small_stream(&ctx.repo, &s).await?)?), None => None };
        if mode == "users" || mode == "full" {
            if !crate::util::is_root() { bail!("восстановление пользователей — только от root"); }
            let acc = acc.as_ref().ok_or_else(|| anyhow!("в копии нет учётных записей"))?;
            let r = restore_users(ctx, p, acc, &m).await?;
            report.insert("users".into(), r);
        }
        if mode == "full" && p_bool(p, "install_packages", false) {
            report.insert("packages".into(), install_packages(&inv, &log).await?);
        }
        if mode == "files" || mode == "full" {
            let want = p_list(p, "streams");
            let only = p_list(p, "only");
            // Владельцы файлов — по именам: UID/GID на цели могут отличаться от исходных.
            let remap = if in_place && crate::util::is_root() { acc.as_ref().map(remap_of).unwrap_or_default() } else { Remap::default() };
            if !remap.is_empty() { log(&format!("владельцы файлов: другие UID/GID на цели — {} пользователей, {} групп", remap.uid.len(), remap.gid.len())); }
            let skip = if in_place { identity_skip(p_bool(p, "host_keys", false), p_bool(p, "system_auth", false)) } else { vec![] };
            let mut done = Vec::new();
            for s in m.streams.iter().filter(|s| s.name.ends_with(".tar")) {
                let short = s.name.trim_end_matches(".tar");
                if !want.is_empty() && !want.iter().any(|w| w == short || *w == s.name) { continue; }
                log(&format!("{} → {root}", s.name));
                let st = extract(ctx, s, &root, &only, &skip, &remap).await?;
                log(&format!("  файлов: {}{}", st.files, if st.unsafe_skipped > 0 { format!(", пропущено из-за символьных ссылок: {}", st.unsafe_skipped) } else { String::new() }));
                for e in &st.notes { log(&format!("  {e}")); }
                done.push(json!({ "stream": s.name, "files": st.files, "unsafe_skipped": st.unsafe_skipped }));
            }
            report.insert("files".into(), json!(done));
            if in_place && mode == "full" {
                fix_user_homes(&log);
                if inventory::have("visudo") {
                    match std::process::Command::new("visudo").arg("-c").output() {
                        Ok(o) if o.status.success() => {}
                        Ok(o) => log(&format!("ВНИМАНИЕ: visudo -c: {}", String::from_utf8_lossy(&o.stderr).trim())),
                        Err(_) => {}
                    }
                }
            }
        }
        if mode == "full" && p_bool(p, "enable_services", true) {
            report.insert("services".into(), enable_services(&inv, p_bool(p, "start_services", false), &log).await);
        }
        Ok(Value::Object(report))
    }
}

// ------------------------------------------------------------------ восстановление файлов

/// Файлы, которые определяют саму машину или вход на неё: при восстановлении «на место» не заменяются.
fn identity_skip(host_keys: bool, system_auth: bool) -> Vec<String> {
    let mut v: Vec<String> = [
        "etc/passwd", "etc/shadow", "etc/group", "etc/gshadow", "etc/passwd-", "etc/shadow-", "etc/group-", "etc/gshadow-", "etc/subuid", "etc/subgid",
        "etc/fstab", "etc/crypttab", "etc/hostname", "etc/hosts", "etc/machine-id", "etc/resolv.conf", "etc/netplan", "etc/network", "etc/NetworkManager/system-connections",
        "etc/sysconfig/network-scripts", "etc/systemd/network", "etc/default/grub", "etc/udev/rules.d/70-persistent-net.rules", "etc/mtab", "etc/adjtime",
        "etc/cloud", "etc/initramfs-tools/conf.d/resume", "etc/mdadm", "etc/lvm",
        // Ключи root дописываются отдельно (не заменяют ключ, которым сервер вошёл на цель).
        "root/.ssh/authorized_keys", "root/.ssh/authorized_keys2",
    ].iter().map(|s| s.to_string()).collect();
    if !host_keys { v.push("etc/ssh/ssh_host_".into()); }
    if !system_auth {
        // Настройки входа исходного сервера могут закрыть вход на новую машину.
        v.extend(["etc/ssh/sshd_config", "etc/ssh/sshd_config.d", "etc/pam.d", "etc/sudoers", "etc/sudoers.d", "etc/security", "etc/nsswitch.conf", "etc/sssd", "etc/krb5.conf", "etc/login.defs"].iter().map(|s| s.to_string()));
    }
    v
}

fn skipped(name: &str, skip: &[String]) -> bool {
    skip.iter().any(|s| if s.ends_with('_') { name.starts_with(s.as_str()) } else { name == s || name.starts_with(&format!("{s}/")) })
}

/// Перенумерация владельцев: исходный UID/GID → UID/GID того же имени на цели.
#[derive(Default, Clone)]
struct Remap {
    uid: HashMap<u32, u32>,
    gid: HashMap<u32, u32>,
}

impl Remap {
    fn is_empty(&self) -> bool { self.uid.is_empty() && self.gid.is_empty() }
}

fn remap_of(acc: &Value) -> Remap {
    let mut r = Remap::default();
    for u in inventory::parse_passwd(acc["passwd"].as_str().unwrap_or("")) {
        if let Some((nu, _)) = crate::util::user_ids(&u.name) { if nu != u.uid { r.uid.insert(u.uid, nu); } }
    }
    for (n, (g, _)) in inventory::parse_group(acc["group"].as_str().unwrap_or("")) {
        if let Ok(c) = std::ffi::CString::new(n) {
            let gr = unsafe { libc::getgrnam(c.as_ptr()) };
            if !gr.is_null() { let ng = unsafe { (*gr).gr_gid }; if ng != g { r.gid.insert(g, ng); } }
        }
    }
    r
}

#[derive(Default)]
struct ExtractStats {
    files: u64,
    unsafe_skipped: u64,
    notes: Vec<String>,
}

/// Путь внутри `root` проходит через символьную ссылку, созданную не root: запись по нему могла бы
/// попасть куда угодно (например, ~/.ssh → /root/.ssh).
fn through_foreign_symlink(root: &Path, rel: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut cur = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    for (i, c) in parts.iter().enumerate() {
        cur.push(c);
        let Ok(md) = std::fs::symlink_metadata(&cur) else { return false };
        // Последний элемент-ссылку tar заменяет, а не проходит.
        if md.file_type().is_symlink() && i + 1 < parts.len() && md.uid() != 0 { return true; }
    }
    false
}

async fn extract(ctx: &RestoreCtx, s: &crate::store::types::StreamEntry, root: &str, only: &[String], skip: &[String], remap: &Remap) -> Result<ExtractStats> {
    std::fs::create_dir_all(root)?;
    let (reader, fetch) = ctx.repo.stream_reader(s)?;
    let dst = PathBuf::from(root);
    let only: Vec<String> = only.iter().map(|o| o.trim().trim_matches('/').to_string()).filter(|o| !o.is_empty()).collect();
    let skip = skip.to_vec();
    let remap = remap.clone();
    let unpack = tokio::task::spawn_blocking(move || -> Result<ExtractStats> {
        let mut a = tar::Archive::new(reader);
        a.set_preserve_permissions(true);
        a.set_preserve_ownerships(crate::util::is_root());
        a.set_overwrite(true);
        let mut st = ExtractStats::default();
        for e in a.entries()? {
            let mut e = e?;
            let name = e.path()?.to_string_lossy().trim_start_matches('/').trim_end_matches('/').to_string();
            if !only.is_empty() && !only.iter().any(|o| name == *o || name.starts_with(&format!("{o}/")) || o.starts_with(&format!("{name}/"))) { continue; }
            if skipped(&name, &skip) { continue; }
            if through_foreign_symlink(&dst, &name) {
                st.unsafe_skipped += 1;
                if st.notes.len() < 10 { st.notes.push(format!("пропущен {name}: путь проходит через символьную ссылку пользователя")); }
                continue;
            }
            let (ou, og) = (e.header().uid().unwrap_or(0) as u32, e.header().gid().unwrap_or(0) as u32);
            let is_file = e.header().entry_type().is_file();
            if e.unpack_in(&dst)? {
                if is_file { st.files += 1; }
                if !remap.is_empty() {
                    let (nu, ng) = (remap.uid.get(&ou).copied(), remap.gid.get(&og).copied());
                    if nu.is_some() || ng.is_some() { let _ = std::os::unix::fs::lchown(dst.join(&name), nu.or(Some(ou)), ng.or(Some(og))); }
                }
            }
        }
        let mut rest = a.into_inner();
        let _ = std::io::copy(&mut rest, &mut std::io::sink());
        Ok(st)
    });
    let (u, f) = tokio::join!(unpack, fetch);
    let st = u??;
    f??;
    Ok(st)
}

// ------------------------------------------------------------------ пользователи

async fn run_cmd(log: &LogFn, prog: &str, args: &[&str]) -> Result<()> {
    log(&format!("$ {prog} {}", args.iter().map(|a| crate::util::shq(a)).collect::<Vec<_>>().join(" ")));
    crate::util::run(tokio::process::Command::new(prog).args(args)).await.map(|_| ())
}

/// Пароль по хэшу — через stdin chpasswd (хэш не виден в списке процессов).
async fn set_hash(log: &LogFn, name: &str, hash: &str) -> Result<()> {
    if inventory::have("chpasswd") {
        log(&format!("$ chpasswd -e  # пароль {name}"));
        let mut c = tokio::process::Command::new("chpasswd").arg("-e").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
        if let Some(mut si) = c.stdin.take() {
            use tokio::io::AsyncWriteExt;
            si.write_all(format!("{name}:{hash}\n").as_bytes()).await?;
        }
        let o = c.wait_with_output().await?;
        if !o.status.success() { bail!("chpasswd: {}", String::from_utf8_lossy(&o.stderr).trim()); }
        return Ok(());
    }
    log(&format!("$ usermod -p <хэш> {name}"));
    crate::util::run(tokio::process::Command::new("usermod").args(["-p", hash, name])).await.map(|_| ())
}

fn gid_free(g: u32) -> bool { unsafe { libc::getgrgid(g).is_null() } }
fn uid_free(u: u32) -> bool { unsafe { libc::getpwuid(u).is_null() } }
fn group_exists(n: &str) -> bool { std::ffi::CString::new(n).map(|c| unsafe { !libc::getgrnam(c.as_ptr()).is_null() }).unwrap_or(false) }

/// Имя пользователя или группы, которое можно передать useradd/groupadd.
fn valid_account(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && !n.starts_with('-') && !n.chars().all(|c| c.is_ascii_digit())
        && n.chars().enumerate().all(|(i, c)| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' || (c == '$' && i == n.len() - 1))
}

/// Домашний каталог, которому можно рекурсивно сменить владельца: не корень, не системный, не общий.
fn safe_home(home: &str, all: &[inventory::Passwd]) -> bool {
    let h = home.trim_end_matches('/');
    let depth = h.split('/').filter(|s| !s.is_empty()).count();
    if depth < 2 || !h.starts_with('/') || h.contains("/../") || inventory::is_system_path(h) { return false; }
    if ["/home", "/var", "/opt", "/srv", "/usr", "/etc", "/root", "/var/lib", "/var/www", "/mnt", "/media", "/tmp", "/var/tmp", "/run", "/proc", "/sys", "/dev"].iter().any(|d| h == *d) { return false; }
    if h.starts_with("/proc/") || h.starts_with("/sys/") || h.starts_with("/dev/") || h.starts_with("/run/") || h.starts_with("/etc/") || h.starts_with("/usr/") || h.starts_with("/var/lib/") { return false; }
    all.iter().filter(|u| u.home.trim_end_matches('/') == h).count() <= 1
}

/// Сменить владельца дерева, не выходя за пределы файловой системы и не следуя ссылкам.
fn chown_tree(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::fs::MetadataExt;
    let Ok(md0) = std::fs::symlink_metadata(path) else { return };
    if md0.file_type().is_symlink() { return; }
    let dev = md0.dev();
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.dev() != dev { continue; }
        let _ = std::os::unix::fs::lchown(&p, Some(uid), Some(gid));
        if md.is_dir() { if let Ok(rd) = std::fs::read_dir(&p) { for e in rd.flatten() { stack.push(e.path()); } } }
    }
}

async fn restore_users(ctx: &RestoreCtx, p: &Value, acc: &Value, m: &crate::store::types::Manifest) -> Result<Value> {
    let log = ctx.log.clone();
    for t in ["useradd", "groupadd", "usermod"] { if !inventory::have(t) { bail!("на цели нет {t} (пакет passwd/shadow-utils)"); } }
    let pw = inventory::parse_passwd(acc["passwd"].as_str().unwrap_or(""));
    let gr = inventory::parse_group(acc["group"].as_str().unwrap_or(""));
    let shadow: HashMap<String, String> = acc["shadow"].as_str().unwrap_or("").lines().filter_map(|l| { let mut f = l.split(':'); Some((f.next()?.to_string(), f.next()?.to_string())) }).collect();
    if shadow.is_empty() { log("внимание: в копии нет /etc/shadow (агент работал не от root) — пароли не восстанавливаются"); }
    let only = p_list(p, "users");
    let set_pw = p_bool(p, "set_passwords", true);
    let sel: Vec<&inventory::Passwd> = pw.iter().filter(|u| if only.is_empty() { (1000..60000).contains(&u.uid) && inventory::login_shell(&u.shell) } else { only.contains(&u.name) }).filter(|u| {
        if u.name == "root" { return false; }
        if !valid_account(&u.name) { log(&format!("  пропущен пользователь с недопустимым именем «{}»", u.name)); return false; }
        true
    }).collect();
    let gid_name: HashMap<u32, String> = gr.iter().map(|(n, (g, _))| (*g, n.clone())).collect();
    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut mapped: Vec<(String, u32, u32)> = Vec::new();
    let mut need_groups: BTreeMap<String, u32> = BTreeMap::new();
    for u in &sel {
        if let Some(n) = gid_name.get(&u.gid) { need_groups.insert(n.clone(), u.gid); }
        for (n, (g, mem)) in &gr { if mem.contains(&u.name) && *g >= 1000 { need_groups.insert(n.clone(), *g); } }
    }
    for (n, g) in &need_groups {
        if group_exists(n) || !valid_account(n) { continue; }
        let gs = g.to_string();
        if gid_free(*g) { run_cmd(&log, "groupadd", &["-g", &gs, n]).await?; } else { run_cmd(&log, "groupadd", &[n]).await?; }
    }
    for u in &sel {
        let exists = crate::util::user_ids(&u.name).is_some();
        if !exists {
            let shell = if Path::new(&u.shell).exists() { u.shell.clone() } else if Path::new("/bin/bash").exists() { "/bin/bash".into() } else { "/bin/sh".into() };
            let group = gid_name.get(&u.gid).filter(|g| group_exists(g)).cloned().unwrap_or_else(|| "users".into());
            let uid = u.uid.to_string();
            let gecos: String = u.gecos.chars().filter(|c| !c.is_control() && *c != ':').collect();
            let mut args: Vec<&str> = vec!["-M", "-d", &u.home, "-s", &shell, "-g", &group];
            if !gecos.is_empty() { args.extend(["-c", gecos.as_str()]); }
            if uid_free(u.uid) { args.extend(["-u", uid.as_str()]); }
            args.push(&u.name);
            run_cmd(&log, "useradd", &args).await?;
            created.push(u.name.clone());
        } else {
            updated.push(u.name.clone());
        }
        let (nu, ng) = crate::util::user_ids(&u.name).ok_or_else(|| anyhow!("пользователь {} не создан", u.name))?;
        if nu != u.uid { mapped.push((u.name.clone(), nu, ng)); log(&format!("  {}: UID {} занят — назначен {nu}", u.name, u.uid)); }
        if set_pw {
            if let Some(h) = shadow.get(&u.name) { if !h.is_empty() && !h.contains(':') && !h.contains('\n') { set_hash(&log, &u.name, h).await?; } }
        }
        let extra: Vec<String> = gr.iter().filter(|(n, (_, mem))| mem.contains(&u.name) && group_exists(n)).map(|(n, _)| n.clone()).collect();
        if !extra.is_empty() { run_cmd(&log, "usermod", &["-a", "-G", &extra.join(","), &u.name]).await?; }
        if !Path::new(&u.home).exists() && safe_home(&u.home, &pw) {
            std::fs::create_dir_all(&u.home)?;
            std::fs::set_permissions(&u.home, std::os::unix::fs::PermissionsExt::from_mode(0o750))?;
            std::os::unix::fs::lchown(&u.home, Some(nu), Some(ng))?;
        }
    }
    if p_bool(p, "root_password", false) {
        if let Some(h) = shadow.get("root") { if !h.is_empty() && !h.starts_with('!') && !h.starts_with('*') && !h.contains(':') { set_hash(&log, "root", h).await?; } }
    }
    // Ключи SSH: из копии во временный каталог, затем дописываются к существующим.
    let tmp = tempfile::Builder::new().prefix("hb-restore-").tempdir()?;
    let troot = tmp.path().to_string_lossy().to_string();
    let mut key_paths: Vec<String> = sel.iter().map(|u| format!("{}/.ssh", u.home.trim_end_matches('/'))).collect();
    if p_bool(p, "root_keys", true) { key_paths.push("/root/.ssh".into()); }
    let sudo_wanted = p_bool(p, "sudoers", true);
    let mut want = key_paths.clone();
    if sudo_wanted { want.push("/etc/sudoers.d".into()); }
    for name in ["homes.tar", "system.tar"] {
        if let Some(s) = m.streams.iter().find(|s| s.name == name) {
            // Во временный каталог — без смены владельцев (всё принадлежит root, каталог 0700).
            extract(ctx, s, &troot, &want, &[], &Remap::default()).await?;
        }
    }
    let mut keys_done = 0u64;
    let mut targets: Vec<(String, String)> = sel.iter().map(|u| (u.name.clone(), u.home.clone())).collect();
    if p_bool(p, "root_keys", true) { targets.push(("root".into(), "/root".into())); }
    for (name, home) in &targets {
        let src = tmp.path().join(home.trim_start_matches('/')).join(".ssh");
        if !src.is_dir() { continue; }
        let Some((u, g)) = crate::util::user_ids(name) else { continue };
        match merge_ssh(&src, Path::new(home), u, g) {
            Ok(n) => keys_done += n,
            Err(e) => log(&format!("  ключи SSH {name}: {e:#}")),
        }
    }
    for (name, u, g) in &mapped {
        if let Some(pu) = pw.iter().find(|x| &x.name == name) {
            if Path::new(&pu.home).is_dir() && safe_home(&pu.home, &pw) { chown_tree(Path::new(&pu.home), *u, *g); }
        }
    }
    // sudo: файлы /etc/sudoers.d, которых нет на цели, после проверки visudo.
    let mut sudo_done = Vec::new();
    if sudo_wanted {
        if let Ok(rd) = std::fs::read_dir(tmp.path().join("etc/sudoers.d")) {
            for e in rd.flatten() {
                let fname = e.file_name().to_string_lossy().to_string();
                let dst = Path::new("/etc/sudoers.d").join(&fname);
                let src_md = std::fs::symlink_metadata(e.path());
                if std::fs::symlink_metadata(&dst).is_ok() || !src_md.map(|m| m.is_file()).unwrap_or(false) || fname.contains('.') || fname.ends_with('~') { continue; }
                let ok = !inventory::have("visudo") || std::process::Command::new("visudo").args(["-cf"]).arg(e.path()).output().map(|o| o.status.success()).unwrap_or(false);
                if !ok { log(&format!("  sudoers.d/{fname}: не прошёл visudo — пропущен")); continue; }
                std::fs::create_dir_all("/etc/sudoers.d")?;
                std::fs::copy(e.path(), &dst)?;
                std::fs::set_permissions(&dst, std::os::unix::fs::PermissionsExt::from_mode(0o440))?;
                let _ = std::os::unix::fs::lchown(&dst, Some(0), Some(0));
                sudo_done.push(fname);
            }
        }
    }
    log(&format!("пользователи: создано {}, уже были {}, файлов ключей SSH {keys_done}, правил sudo {}", created.len(), updated.len(), sudo_done.len()));
    Ok(json!({ "created": created, "existing": updated, "uid_changed": mapped.iter().map(|m| &m.0).collect::<Vec<_>>(), "ssh_files": keys_done, "sudoers": sudo_done }))
}

/// Дописать ключи SSH из копии: authorized_keys объединяется с текущим (ключ, которым вошёл сервер,
/// остаётся), прочие файлы (ключи, config, known_hosts) пишутся, только если их нет. Через символьные
/// ссылки не пишем.
fn merge_ssh(src: &Path, home: &Path, uid: u32, gid: u32) -> Result<u64> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let hm = std::fs::symlink_metadata(home).with_context(|| format!("нет домашнего каталога {}", home.display()))?;
    if hm.file_type().is_symlink() && hm.uid() != 0 { bail!("домашний каталог {} — символьная ссылка", home.display()); }
    let ssh = home.join(".ssh");
    match std::fs::symlink_metadata(&ssh) {
        Ok(m) if m.file_type().is_symlink() => bail!("{} — символьная ссылка, ключи не записываются", ssh.display()),
        Ok(m) if !m.is_dir() => bail!("{} — не каталог", ssh.display()),
        Ok(_) => {}
        Err(_) => {
            std::fs::create_dir(&ssh)?;
            std::os::unix::fs::lchown(&ssh, Some(uid), Some(gid))?;
        }
    }
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700))?;
    let mut n = 0u64;
    for e in std::fs::read_dir(src)?.flatten() {
        let fname = e.file_name().to_string_lossy().to_string();
        let smd = std::fs::symlink_metadata(e.path())?;
        if !smd.is_file() { continue; }
        let dst = ssh.join(&fname);
        let dmd = std::fs::symlink_metadata(&dst).ok();
        if dmd.as_ref().map(|m| m.file_type().is_symlink()).unwrap_or(false) { continue; }
        let body = std::fs::read(e.path())?;
        let data = if fname == "authorized_keys" || fname == "authorized_keys2" {
            let cur = if dmd.is_some() { std::fs::read_to_string(&dst).unwrap_or_default() } else { String::new() };
            let mut lines: Vec<String> = cur.lines().map(|l| l.to_string()).collect();
            let mut added = false;
            for l in String::from_utf8_lossy(&body).lines() {
                if !l.trim().is_empty() && !lines.iter().any(|x| x.trim() == l.trim()) { lines.push(l.to_string()); added = true; }
            }
            if !added && dmd.is_some() { continue; }
            (lines.join("\n") + "\n").into_bytes()
        } else {
            if dmd.is_some() { continue; }
            body
        };
        // Запись во временный файл рядом и переименование: существующая ссылка заменяется, а не проходится.
        let tmpf = ssh.join(format!(".{fname}.hb-{}", std::process::id()));
        {
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmpf)?;
            f.write_all(&data)?;
        }
        let mode = if fname.ends_with(".pub") || fname == "known_hosts" { 0o644 } else { 0o600 };
        std::fs::set_permissions(&tmpf, std::fs::Permissions::from_mode(mode))?;
        std::os::unix::fs::lchown(&tmpf, Some(uid), Some(gid))?;
        std::fs::rename(&tmpf, &dst)?;
        n += 1;
    }
    Ok(n)
}

/// После распаковки домашних каталогов — владельцы по текущим UID (только безопасные пути) и права ~/.ssh.
fn fix_user_homes(log: &LogFn) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let all = inventory::parse_passwd(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    for u in &all {
        if !(1000..60000).contains(&u.uid) || !safe_home(&u.home, &all) { continue; }
        let Ok(md) = std::fs::symlink_metadata(&u.home) else { continue };
        if md.file_type().is_symlink() { continue; }
        if md.uid() != u.uid {
            log(&format!("  владелец {} → {}", u.home, u.name));
            chown_tree(Path::new(&u.home), u.uid, u.gid);
        }
        let ssh = Path::new(&u.home).join(".ssh");
        if let Ok(m) = std::fs::symlink_metadata(&ssh) {
            if m.is_dir() {
                let _ = std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700));
                if let Ok(rd) = std::fs::read_dir(&ssh) {
                    for e in rd.flatten() {
                        let Ok(fm) = std::fs::symlink_metadata(e.path()) else { continue };
                        if !fm.is_file() { continue; }
                        let n = e.file_name().to_string_lossy().to_string();
                        let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(if n.ends_with(".pub") || n == "known_hosts" { 0o644 } else { 0o600 }));
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ пакеты и службы

async fn install_packages(inv: &Inventory, log: &LogFn) -> Result<Value> {
    let osr = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let id = osr.lines().find_map(|l| l.strip_prefix("ID=")).unwrap_or("").trim_matches('"').to_string();
    if id != inv.os_id { log(&format!("пакеты не устанавливаются: на цели {id}, в копии {} — разные дистрибутивы", inv.os_id)); return Ok(json!({ "skipped": "другой дистрибутив" })); }
    let pk = &inv.packages.manual;
    if pk.is_empty() { return Ok(json!({ "installed": 0 })); }
    log(&format!("установка пакетов: {} шт.", pk.len()));
    let (prog, mut base): (&str, Vec<&str>) = match inv.packages.manager.as_str() {
        "dpkg" => {
            let _ = run_cmd(log, "apt-get", &["update", "-q"]).await;
            ("apt-get", vec!["install", "-y", "-q", "--no-install-recommends"])
        }
        "rpm" => (if inventory::have("dnf") { "dnf" } else { "yum" }, vec!["install", "-y", "--setopt=strict=0", "--skip-broken"]),
        "apk" => ("apk", vec!["add"]),
        m => bail!("неизвестный менеджер пакетов «{m}»"),
    };
    let mut cmd = tokio::process::Command::new(prog);
    if prog == "apt-get" { cmd.env("DEBIAN_FRONTEND", "noninteractive"); }
    base.extend(pk.iter().map(|s| s.as_str()));
    let out = cmd.args(&base).output().await.with_context(|| format!("запуск {prog}"))?;
    if out.status.success() { return Ok(json!({ "installed": pk.len() })); }
    // Один отсутствующий пакет валит apt целиком — тогда по одному.
    log("общая установка не удалась — по одному пакету");
    let mut failed = Vec::new();
    for p in pk {
        let mut c = tokio::process::Command::new(prog);
        if prog == "apt-get" { c.env("DEBIAN_FRONTEND", "noninteractive"); }
        let mut a: Vec<&str> = base[..base.len() - pk.len()].to_vec();
        a.push(p);
        if !c.args(&a).output().await.map(|o| o.status.success()).unwrap_or(false) { failed.push(p.clone()); }
    }
    if !failed.is_empty() { log(&format!("не установлены: {}", failed.join(", "))); }
    Ok(json!({ "installed": pk.len() - failed.len(), "failed": failed }))
}

async fn enable_services(inv: &Inventory, start: bool, log: &LogFn) -> Value {
    if !inventory::have("systemctl") { return json!({ "skipped": "нет systemd" }); }
    let _ = run_cmd(log, "systemctl", &["daemon-reload"]).await;
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut units: Vec<String> = inv.services.iter().filter(|s| s.enabled).map(|s| s.unit.clone()).collect();
    units.extend(inv.schedules.iter().filter(|s| s.kind == "timer").filter_map(|s| s.source.rsplit('/').next().map(|x| x.to_string())));
    units.sort();
    units.dedup();
    for u in units {
        let mut args = vec!["enable"];
        if start { args.push("--now"); }
        args.push(&u);
        match run_cmd(log, "systemctl", &args).await { Ok(()) => done.push(u), Err(e) => { log(&format!("  {u}: {e:#}")); failed.push(u) } }
    }
    for c in ["cron", "crond"] { let _ = tokio::process::Command::new("systemctl").args(["try-restart", c]).output().await; }
    json!({ "enabled": done, "failed": failed })
}
