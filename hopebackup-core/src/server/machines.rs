//! «Серверы и ВМ»: машины, к которым сервер управления подключается по SSH (пароль или ключ), — опись
//! до начала копирования, анализ и рекомендации, установка агента, экземпляры по рекомендациям.
//!
//! Опись: сервер копирует свою программу во временный каталог машины (/var/tmp, затем домашний),
//! выполняет `hopebackup inventory` от root (sudo без пароля или с паролем) и удаляет программу.
//! Если программа не запускается (другая архитектура, Alpine без gcompat, noexec) — сокращённая опись
//! командами sh.

use anyhow::{anyhow, bail, Result};
use axum::extract::{Path, State};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::api::{bad, ApiError, R, MASK};
use super::auth::Who;
use super::Server;
use crate::plugins::host::analysis::{self, Analysis};
use crate::plugins::host::inventory::Inventory;
use crate::sshc::{self, Sudo};

type S = State<Arc<Server>>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MachineInv {
    pub time: Option<DateTime<Utc>>,
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub sudo: String,
    #[serde(default)]
    pub recommendations: usize,
    #[serde(default)]
    pub estimate: u64,
    /// Сколько пунктов подготовки машины требуют внимания (место под снимки, память…).
    #[serde(default)]
    pub prepare_todo: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    pub user: String,
    /// password | key
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub key_passphrase: String,
    #[serde(default)]
    pub sudo_password: String,
    /// Закреплённый отпечаток ключа хоста (SHA256:…), запоминается при первом подключении.
    #[serde(default)]
    pub host_key: String,
    /// Агент HopeBackup на этой машине (после установки).
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub last: Option<MachineInv>,
    pub created: DateTime<Utc>,
}
fn d_port() -> u16 { 22 }

impl Machine {
    fn auth(&self) -> sshc::Auth {
        sshc::Auth {
            user: self.user.clone(),
            password: if self.auth == "key" { String::new() } else { self.password.clone() },
            private_key: if self.auth == "key" { self.private_key.clone() } else { String::new() },
            key_passphrase: self.key_passphrase.clone(),
            sudo_password: self.sudo_password.clone(),
        }
    }
}

fn view(s: &Server, m: &Machine) -> Value {
    let agent = s.store.read(|st| st.agents.iter().find(|a| a.id == m.agent_id).map(|a| json!({ "id": a.id, "name": a.name, "last_seen": a.last_seen, "version": a.version })));
    let mk = |v: &str| if v.is_empty() { "" } else { MASK };
    json!({
        "id": m.id, "name": m.name, "host": m.host, "port": m.port, "user": m.user, "auth": m.auth,
        "password": mk(&m.password), "private_key": mk(&m.private_key), "key_passphrase": mk(&m.key_passphrase), "sudo_password": mk(&m.sudo_password),
        "host_key": m.host_key, "agent_id": m.agent_id, "agent": agent, "notes": m.notes, "last": m.last, "created": m.created
    })
}

fn inv_path(s: &Server, id: &str) -> std::path::PathBuf {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    s.dir.join("inventory").join(format!("{safe}.json"))
}

/// Скрыть секреты в описи: значения `password=…`, `--token …`, `-pСЕКРЕТ` у mysql, пароль в URL
/// (`proto://user:pass@host`). Опись видят и операторы с ролью «просмотр»: в параметрах fstab (CIFS),
/// командных строках процессов и заданий cron часто лежат пароли.
pub fn redact(v: &mut Value) {
    match v {
        Value::String(s) => { let r = redact_str(s); if r != *s { *s = r; } }
        Value::Array(a) => a.iter_mut().for_each(redact),
        Value::Object(o) => o.values_mut().for_each(redact),
        _ => {}
    }
}

fn secret_key(k: &str) -> bool {
    let k = k.trim_start_matches('-').to_ascii_lowercase();
    ["password", "passwd", "pass", "pwd", "secret", "token", "apikey", "api_key", "api-key", "access_key", "secret_key", "auth"].iter().any(|w| k == *w || k.ends_with(&format!("_{w}")) || k.ends_with(&format!("-{w}")) || k.contains("password") || k.contains("secret"))
}

pub fn redact_str(s: &str) -> String {
    if !s.contains('=') && !s.contains("://") && !s.contains(" -p") && !s.contains("--") { return s.to_string(); }
    let mysql = s.contains("mysql") || s.contains("mariadb");
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for w in s.split(' ') {
        if mask_next && !w.is_empty() { out.push(MASK.into()); mask_next = false; continue; }
        let parts: Vec<String> = w.split(',').map(|p| {
            if let Some((k, _)) = p.split_once('=') { if secret_key(k) { return format!("{k}={MASK}"); } }
            if let Some(i) = p.find("://") {
                let rest = &p[i + 3..];
                if let (Some(at), Some(colon)) = (rest.find('@'), rest.find(':')) {
                    if colon < at && !rest[..at].contains('/') { return format!("{}{}:{MASK}{}", &p[..i + 3], &rest[..colon], &rest[at..]); }
                }
            }
            if mysql && p.starts_with("-p") && p.len() > 2 && !p.starts_with("-p=") && !p.starts_with("--") { return format!("-p{MASK}"); }
            p.to_string()
        }).collect();
        let joined = parts.join(",");
        if w.starts_with('-') && !w.contains('=') && secret_key(w) && w.len() > 2 { mask_next = true; }
        out.push(joined);
    }
    out.join(" ")
}

#[derive(Deserialize)]
pub struct MachineReq {
    name: String,
    host: String,
    #[serde(default)]
    port: Option<u16>,
    user: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    private_key: String,
    #[serde(default)]
    key_passphrase: String,
    #[serde(default)]
    sudo_password: String,
    #[serde(default)]
    notes: String,
    /// Привязать уже установленного агента (или "" — отвязать); не задано — не менять.
    #[serde(default)]
    agent_id: Option<String>,
}

fn check(q: &MachineReq) -> R<()> {
    if q.name.trim().is_empty() { return Err(bad("укажите имя")); }
    crate::util::valid_host(q.host.trim()).map_err(|e| bad(e.to_string()))?;
    crate::util::valid_ssh_user(q.user.trim()).map_err(|e| bad(e.to_string()))?;
    if q.auth != "password" && q.auth != "key" { return Err(bad("вход: password или key")); }
    Ok(())
}

pub async fn list(State(s): S) -> Json<Vec<Value>> {
    let v = s.store.read(|st| st.machines.clone());
    Json(v.iter().map(|m| view(&s, m)).collect())
}

pub async fn create(State(s): S, Extension(w): Extension<Who>, Json(q): Json<MachineReq>) -> R<Json<Value>> {
    check(&q)?;
    if q.auth == "password" && q.password.is_empty() { return Err(bad("укажите пароль")); }
    if q.auth == "key" && q.private_key.trim().is_empty() { return Err(bad("вставьте закрытый ключ")); }
    if q.auth == "key" { russh::keys::decode_secret_key(q.private_key.trim(), if q.key_passphrase.is_empty() { None } else { Some(q.key_passphrase.as_str()) }).map_err(|e| bad(&format!("закрытый ключ: {e}")))?; }
    let m = Machine {
        id: crate::util::short_id(),
        name: q.name.trim().into(),
        host: q.host.trim().into(),
        port: q.port.unwrap_or(22),
        user: q.user.trim().into(),
        auth: q.auth.clone(),
        password: q.password.clone(),
        private_key: q.private_key.trim().to_string() + "\n",
        key_passphrase: q.key_passphrase.clone(),
        sudo_password: q.sudo_password.clone(),
        notes: q.notes.clone(),
        created: Utc::now(),
        ..Default::default()
    };
    let m = if q.auth == "password" { Machine { private_key: String::new(), ..m } } else { m };
    let m = Machine { agent_id: q.agent_id.clone().unwrap_or_default(), ..m };
    s.store.update(|st| {
        if !m.agent_id.is_empty() && !st.agents.iter().any(|a| a.id == m.agent_id) { bail!("агент не найден"); }
        st.machines.push(m.clone());
        Ok(())
    })?;
    s.audit(&w.user, "machine_create", format!("{} ({}@{})", m.name, m.user, m.host));
    Ok(Json(view(&s, &m)))
}

pub async fn update(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<MachineReq>) -> R<Json<Value>> {
    check(&q)?;
    let m = s.store.update(|st| {
        let m = st.machines.iter_mut().find(|m| m.id == id).ok_or_else(|| anyhow!("машина не найдена"))?;
        // Другой адрес — другой ключ хоста; сохранённые секреты на новый адрес или другому пользователю не
        // отправляются: нужен заново только секрет выбранного способа входа (пароль или ключ). Парольная фраза
        // ключа никуда не передаётся (расшифровывает ключ здесь) — сохраняется. Пароль sudo прежнего пользователя
        // или адреса сбрасывается: новому он не подходит, а отправлять его туда нельзя.
        let addr_moved = m.host != q.host.trim() || m.port != q.port.unwrap_or(22);
        let moved = addr_moved || m.user != q.user.trim();
        let by_key = q.auth == "key";
        let kept = |new: &str| new == MASK || new.is_empty();
        if moved && by_key && kept(&q.private_key) { bail!("адрес или пользователь изменились — вставьте закрытый ключ заново"); }
        if moved && !by_key && kept(&q.password) { bail!("адрес или пользователь изменились — введите пароль заново"); }
        // Ключ хоста принадлежит адресу, а не пользователю: сбрасываем только при смене адреса или порта.
        if addr_moved { m.host_key.clear(); }
        m.name = q.name.trim().into();
        m.host = q.host.trim().into();
        m.port = q.port.unwrap_or(22);
        m.user = q.user.trim().into();
        m.auth = q.auth.clone();
        m.notes = q.notes.clone();
        if let Some(a) = &q.agent_id { m.agent_id = a.clone(); }
        let keep = |new: &str, old: &str| if kept(new) { if moved { String::new() } else { old.to_string() } } else { new.to_string() };
        // Хранится только секрет выбранного способа входа.
        m.password = if by_key { String::new() } else { keep(&q.password, &m.password) };
        if by_key {
            let new_key = !kept(&q.private_key);
            m.private_key = { let k = if new_key { q.private_key.clone() } else { m.private_key.clone() }; if k.trim().is_empty() || k.ends_with('\n') { k } else { k + "\n" } };
            let typed = !kept(&q.key_passphrase);
            if typed { m.key_passphrase = q.key_passphrase.clone(); }
            // Новый ключ без шифрования, а сохранена фраза от прежнего — фраза не нужна.
            if new_key && !typed && !m.key_passphrase.is_empty() && russh::keys::decode_secret_key(m.private_key.trim(), None).is_ok() { m.key_passphrase.clear(); }
        } else {
            m.private_key = String::new();
            m.key_passphrase = String::new();
        }
        m.sudo_password = keep(&q.sudo_password, &m.sudo_password);
        if by_key && m.private_key.trim().is_empty() { bail!("вставьте закрытый ключ"); }
        if !by_key && m.password.is_empty() { bail!("укажите пароль"); }
        if m.auth == "key" && !m.private_key.trim().is_empty() {
            russh::keys::decode_secret_key(m.private_key.trim(), if m.key_passphrase.is_empty() { None } else { Some(m.key_passphrase.as_str()) }).map_err(|e| anyhow!("закрытый ключ: {e}"))?;
        }
        let m = m.clone();
        if !m.agent_id.is_empty() && !st.agents.iter().any(|a| a.id == m.agent_id) { bail!("агент не найден"); }
        Ok(m)
    })?;
    s.audit(&w.user, "machine_update", m.name.clone());
    Ok(Json(view(&s, &m)))
}

pub async fn delete(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    // Идентификатор идёт в путь файла описи — только существующая машина (никаких «../»).
    let m = machine(&s, &id).map_err(|e| ApiError(axum::http::StatusCode::NOT_FOUND, e.to_string()))?;
    s.store.update(|st| { st.machines.retain(|x| x.id != m.id); Ok(()) })?;
    let _ = std::fs::remove_file(inv_path(&s, &m.id));
    s.audit(&w.user, "machine_delete", id);
    Ok(Json(json!({ "ok": true })))
}

/// Сбросить закреплённый ключ хоста (машину переустановили).
pub async fn reset_key(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    s.store.update(|st| { if let Some(m) = st.machines.iter_mut().find(|m| m.id == id) { m.host_key.clear(); } Ok(()) })?;
    s.audit(&w.user, "machine_reset_key", id);
    Ok(Json(json!({ "ok": true })))
}

fn machine(s: &Server, id: &str) -> Result<Machine> { s.store.read(|st| st.machines.iter().find(|m| m.id == id).cloned()).ok_or_else(|| anyhow!("машина не найдена")) }

async fn connect(s: &Server, m: &Machine) -> Result<sshc::Conn> {
    let c = match sshc::connect(&m.host, m.port, &m.auth(), &m.host_key).await {
        Ok(c) => c,
        Err(e) => {
            let t = format!("{e:#}");
            if t.contains("ключ хоста") && t.contains("изменился") {
                super::notify::emit("machine", format!("Изменился ключ хоста: {}", m.name), format!("Машина «{}» ({}:{}) предъявила другой ключ SSH.\n{t}\n\nЕсли машину не переустанавливали — возможна подмена сервера: не сбрасывайте ключ, пока не выясните причину.", m.name, m.host, m.port));
            }
            return Err(e);
        }
    };
    if m.host_key.is_empty() && !c.host_key.is_empty() {
        // Закрепить ключ, только если за время подключения адрес машины не поменяли.
        let (id, fp, host, port) = (m.id.clone(), c.host_key.clone(), m.host.clone(), m.port);
        s.store.update(|st| { if let Some(x) = st.machines.iter_mut().find(|x| x.id == id && x.host == host && x.port == port && x.host_key.is_empty()) { x.host_key = fp.clone(); } Ok(()) })?;
    }
    Ok(c)
}

fn sudo_label(x: Sudo) -> &'static str {
    match x { Sudo::Root => "root", Sudo::NoPass => "sudo без пароля", Sudo::Pass => "sudo с паролем", Sudo::None => "нет прав root" }
}

/// Архитектура и библиотека C машины → подходящая сборка программы из комплекта.
async fn probe(c: &sshc::Conn) -> Result<(String, crate::dist::Libc)> {
    let o = c.exec(crate::dist::PROBE, None, Duration::from_secs(30)).await?;
    Ok(crate::dist::parse_probe(&o.text()))
}

/// Программа для машины (сборка по архитектуре и библиотеке C).
async fn program_for(c: &sshc::Conn, log: &crate::plugins::LogFn) -> Result<(String, Vec<u8>)> {
    let (arch, libc) = probe(c).await?;
    let (t, bin) = crate::dist::pick(&arch, &libc)?;
    log(&format!("машина: {arch}, {}; программа: {}", crate::dist::label_libc(&libc), crate::dist::label(&t)));
    Ok((t, bin))
}

/// Проверка: вход, права, ОС, архитектура.
pub async fn test(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let m = machine(&s, &id)?;
    let c = connect(&s, &m).await.map_err(|e| bad(&format!("{e:#}")))?;
    let o = c.exec("uname -srm; (cat /etc/os-release 2>/dev/null || cat /etc/redhat-release /etc/debian_version /etc/alpine-release 2>/dev/null) | head -20; (ldd --version 2>&1 | head -1) || true", None, Duration::from_secs(30)).await.map_err(|e| bad(&format!("{e:#}")))?;
    let t = o.text();
    let mut lines = t.lines();
    let uname = lines.next().unwrap_or("").to_string();
    let rest: String = lines.collect::<Vec<_>>().join("\n");
    let pretty = rest.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string()).unwrap_or_else(|| rest.lines().next().unwrap_or("").to_string());
    let glibc = rest.lines().find(|l| l.contains("GLIBC") || l.contains("GNU libc") || l.contains("glibc") || l.contains("musl")).unwrap_or("").to_string();
    let program = match probe(&c).await {
        Ok((arch, libc)) => match crate::dist::candidates(&arch, &libc).into_iter().find(|t| crate::dist::has(t)) {
            Some(t) => crate::dist::label(&t),
            None => format!("нет сборки для {arch} ({}) — будет сокращённая опись командами sh", crate::dist::label_libc(&libc)),
        },
        Err(_) => String::new(),
    };
    let res = json!({ "ok": true, "host_key": c.host_key, "sudo": sudo_label(c.sudo_mode()), "sudo_note": c.sudo_note(), "root": c.sudo_mode() != Sudo::None, "uname": uname, "os": pretty, "libc": glibc, "program": program });
    c.close().await;
    Ok(Json(res))
}

impl Server {
    /// Задание, которое выполняет сам сервер (`f` получает журнал).
    pub fn spawn_task<F, Fut>(self: &Arc<Self>, kind: &str, title: String, user: &str, f: F) -> String
    where
        F: FnOnce(Arc<Server>, crate::plugins::LogFn) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send + 'static,
    {
        let spec = crate::proto::JobSpec { id: String::new(), kind: kind.into(), instance: None, site: None, snapshot_id: None, mode: String::new(), params: Value::Null };
        let id = self.jobs.enqueue(spec, title, "server", user);
        let s = self.clone();
        let jid = id.clone();
        tokio::spawn(async move {
            if s.jobs.start_local(&jid).is_none() { return; }
            let s2 = s.clone();
            let j2 = jid.clone();
            let log: crate::plugins::LogFn = Arc::new(move |m: &str| s2.jobs.log(&j2, vec![m.to_string()]));
            match f(s.clone(), log.clone()).await {
                Ok(v) => { s.jobs.finish(&jid, true, v, None); }
                Err(e) => { log(&format!("ошибка: {e:#}")); s.jobs.finish(&jid, false, Value::Null, Some(format!("{e:#}"))); }
            }
        });
        id
    }
}

// ---------------------------------------------------------------- опись

pub async fn inventory_start(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>) -> R<Json<Value>> {
    let m = machine(&s, &id)?;
    let mid = m.id.clone();
    let jid = s.spawn_task("machine_inventory", format!("Опись «{}» ({})", m.name, m.host), &w.user, move |s, log| async move { run_inventory(&s, &mid, log).await });
    s.audit(&w.user, "machine_inventory", m.name);
    Ok(Json(json!({ "job_id": jid })))
}

pub async fn inventory_get(State(s): S, Path(id): Path<String>) -> R<Json<Value>> {
    let m = machine(&s, &id)?;
    let mut v = read_doc(&s, &m.id).map_err(|e| ApiError(axum::http::StatusCode::NOT_FOUND, e.to_string()))?;
    // Анализ — по свежему состоянию (агент мог появиться после описи).
    if let Ok(inv) = serde_json::from_value::<Inventory>(v["inventory"].clone()) {
        v["analysis"] = serde_json::to_value(analysis::analyze(&inv, has_agent(&s, &m))).unwrap_or_default();
    }
    redact(&mut v);
    Ok(Json(v))
}

/// Предел файла описи (опись крупного сервера — единицы МБ).
const MAX_DOC: u64 = 64 << 20;

/// Объём дисков машины по последней описи — чтобы заранее сказать, сколько места займёт образ.
/// `disks` — физические диски (полный размер образа), `used` — занято на локальных файловых системах (столько
/// данных будет в образе как минимум; больше — если на дисках остались блоки удалённых файлов), `estimate` —
/// рабочая оценка с запасом 25% (не больше суммы дисков).
pub fn image_size(s: &Server, machine_id: &str) -> Result<Value> {
    let m = machine(s, machine_id)?;
    let doc = match read_doc(s, &m.id) { Ok(d) => d, Err(_) => return Ok(json!({ "machine": m.name, "machine_id": m.id, "inventory": false })) };
    let inv: Inventory = serde_json::from_value(doc["inventory"].clone()).map_err(|e| anyhow!("опись машины: {e}"))?;
    let disks: Vec<Value> = inv.system.storage.disks.iter().filter(|d| d.size > 0).map(|d| json!({ "name": d.name, "size": d.size, "model": d.model })).collect();
    let total: u64 = inv.system.storage.disks.iter().map(|d| d.size).sum();
    let largest: u64 = inv.system.storage.disks.iter().map(|d| d.size).max().unwrap_or(0);
    let mut seen = std::collections::HashSet::new();
    let mut used = 0u64;
    let mut fss = Vec::new();
    for mt in inv.mounts.iter().filter(|x| (x.class == "disk" || x.class == "system") && x.total > 0) {
        if !seen.insert(mt.source.clone()) { continue; }
        used += mt.used;
        fss.push(json!({ "target": mt.target, "fstype": mt.fstype, "total": mt.total, "used": mt.used }));
    }
    let estimate = if total > 0 { (used + used / 4).min(total) } else { used + used / 4 };
    Ok(json!({ "machine": m.name, "machine_id": m.id, "inventory": true, "time": doc["time"], "disks": disks, "disk_total": total, "largest_disk": largest,
               "used": used, "estimate": estimate, "largest": largest.min(estimate), "filesystems": fss }))
}

pub(crate) fn read_doc(s: &Server, id: &str) -> Result<Value> {
    let p = inv_path(s, id);
    let len = std::fs::metadata(&p).map_err(|_| anyhow!("опись ещё не снималась"))?.len();
    if len > MAX_DOC { bail!("файл описи слишком большой ({})", crate::util::human_bytes(len)); }
    Ok(serde_json::from_slice(&std::fs::read(&p)?)?)
}

fn has_agent(s: &Server, m: &Machine) -> bool { !m.agent_id.is_empty() && s.store.read(|st| st.agents.iter().any(|a| a.id == m.agent_id)) }

async fn run_inventory(s: &Arc<Server>, id: &str, log: crate::plugins::LogFn) -> Result<Value> {
    let m = machine(s, id)?;
    log(&format!("подключение {}@{}:{} ({})", m.user, m.host, m.port, if m.auth == "key" { "ключ" } else { "пароль" }));
    let started = Utc::now();
    let r: Result<(Inventory, String)> = async {
        let c = connect(s, &m).await?;
        log(&format!("вход выполнен, ключ хоста {}; права: {}", c.host_key, sudo_label(c.sudo_mode())));
        cleanup_remote(&c, &log).await;
        let inv = match full_inventory(&c, &log).await {
            Ok(inv) => inv,
            Err(e) => {
                log(&format!("программа описи не запустилась: {e:#} — сокращённая опись командами sh"));
                shell_inventory(&c, &log).await?
            }
        };
        let sudo = sudo_label(c.sudo_mode()).to_string();
        c.close().await;
        Ok((inv, sudo))
    }
    .await;
    match r {
        Ok((inv, sudo)) => {
            let a: Analysis = analysis::analyze(&inv, has_agent(s, &m));
            let mut doc = json!({ "time": Utc::now(), "started": started, "method": inv.method, "inventory": inv, "analysis": a });
            // Секреты из командных строк и fstab не храним вовсе.
            redact(&mut doc["inventory"]);
            std::fs::create_dir_all(s.dir.join("inventory"))?;
            crate::util::write_json(&inv_path(s, id), &doc, true)?;
            let last = MachineInv { time: Some(Utc::now()), ok: true, error: String::new(), os: inv.system.os.pretty.clone(), arch: inv.system.kernel.arch.clone(), method: inv.method.clone(), sudo, recommendations: a.recommendations.len(), estimate: a.estimate.first, prepare_todo: a.prepare.iter().filter(|p| p.level == "bad" || p.level == "warn").count() };
            let mid = id.to_string();
            s.store.update(|st| { if let Some(x) = st.machines.iter_mut().find(|x| x.id == mid) { x.last = Some(last.clone()); } Ok(()) })?;
            log(&format!("опись: {} · {} · приложений {}, заданий {}, пользователей {}; рекомендаций {}", inv.system.os.pretty, inv.system.kernel.release, inv.apps.len(), inv.schedules.len(), inv.users.len(), a.recommendations.len()));
            Ok(json!({ "os": inv.system.os.pretty, "recommendations": a.recommendations.len(), "method": inv.method }))
        }
        Err(e) => {
            let last = MachineInv { time: Some(Utc::now()), ok: false, error: format!("{e:#}"), ..Default::default() };
            let mid = id.to_string();
            let _ = s.store.update(|st| { if let Some(x) = st.machines.iter_mut().find(|x| x.id == mid) { x.last = Some(last.clone()); } Ok(()) });
            Err(e)
        }
    }
}

fn sha256_bytes(b: &[u8]) -> String { use sha2::Digest; hex::encode(sha2::Sha256::digest(b)) }

/// Положить программу во временный каталог машины. При правах root каталог создаёт root (0700) и
/// файл пишется сразу от root — пользователь машины не может подменить его до запуска; затем
/// сверяется SHA-256. Без прав root — каталог пользователя (запуск тоже от пользователя).
/// Возвращает каталог; каталоги с noexec пропускаются.
async fn stage_bin(c: &sshc::Conn, bin: &[u8], log: &crate::plugins::LogFn) -> Result<String> {
    let root = c.sudo_mode() != Sudo::None;
    let sum = sha256_bytes(bin);
    let bases: &[&str] = if root { &["/var/tmp", "/tmp", "/dev/shm", "/root", "/run"] } else { &["/var/tmp", "$HOME", "/tmp", "/dev/shm"] };
    let mut last_err = String::from("нет подходящего временного каталога");
    for base in bases {
        let script = format!("umask 077; d=$(mktemp -d {base}/hopebackup.XXXXXX) || exit 3; cat > \"$d/hopebackup\" && chmod 700 \"$d/hopebackup\" && echo \"$d\"");
        log(&format!("копирование программы ({}) в {base}", crate::util::human_bytes(bin.len() as u64)));
        let o = if root { c.exec_root(&script, Some(bin), Duration::from_secs(600)).await? } else { c.exec(&script, Some(bin), Duration::from_secs(600)).await? };
        let dir = o.text().lines().last().unwrap_or("").trim().to_string();
        if o.code != 0 || !dir.starts_with('/') { last_err = format!("{base}: {}", o.err()); continue; }
        let q = crate::util::shq(&dir);
        let chk = format!("f={q}/hopebackup; (sha256sum \"$f\" 2>/dev/null || openssl dgst -sha256 -r \"$f\" 2>/dev/null) | cut -c1-64; \"$f\" --version >/dev/null 2>&1; echo \"rc=$?\"");
        let o = if root { c.exec_root(&chk, None, Duration::from_secs(60)).await? } else { c.exec(&chk, None, Duration::from_secs(60)).await? };
        let t = o.text();
        let got = t.lines().next().unwrap_or("").trim().to_string();
        let rc = t.lines().find_map(|l| l.strip_prefix("rc=")).unwrap_or("").trim().to_string();
        if got.len() == 64 && got != sum {
            let _ = rm_dir(c, &dir, root).await;
            bail!("программа на машине повреждена или подменена (SHA-256 {got}, ожидалось {sum})");
        }
        if rc == "0" { return Ok(dir); }
        let _ = rm_dir(c, &dir, root).await;
        last_err = format!("{base}: программа не запускается (код {rc}{})", if rc == "126" { ", каталог смонтирован с noexec" } else if rc == "127" { ", не подходит библиотека C машины" } else { "" });
        // noexec — пробуем следующий каталог; другие ошибки (не та библиотека C) от каталога не зависят.
        if rc != "126" { break; }
    }
    bail!("{last_err}")
}

/// Убрать временные каталоги программы, оставшиеся от прерванной описи или установки (старше часа).
async fn cleanup_remote(c: &sshc::Conn, log: &crate::plugins::LogFn) {
    let script = "n=0; for d in /var/tmp/hopebackup.* /tmp/hopebackup.* /dev/shm/hopebackup.* /root/hopebackup.* /run/hopebackup.* \"$HOME\"/hopebackup.*; do         [ -d \"$d\" ] || continue; case \"$d\" in */hopebackup.??????) ;; *) continue ;; esac;         if [ -n \"$(find \"$d\" -maxdepth 0 -mmin +60 2>/dev/null)\" ]; then rm -rf \"$d\" && n=$((n+1)); fi; done; echo \"removed=$n\"";
    let o = if c.sudo_mode() != Sudo::None { c.exec_root(script, None, Duration::from_secs(60)).await } else { c.exec(script, None, Duration::from_secs(60)).await };
    if let Ok(o) = o {
        if let Some(n) = o.text().lines().find_map(|l| l.strip_prefix("removed=")).and_then(|n| n.trim().parse::<u32>().ok()).filter(|n| *n > 0) {
            log(&format!("уборка: удалено временных каталогов прерванных описей — {n}"));
        }
    }
}

async fn rm_dir(c: &sshc::Conn, dir: &str, root: bool) -> Result<()> {
    let cmd = format!("rm -rf {}", crate::util::shq(dir));
    if root { c.exec_root(&cmd, None, Duration::from_secs(30)).await?; } else { c.exec(&cmd, None, Duration::from_secs(30)).await?; }
    Ok(())
}

/// Своя программа во временном каталоге машины: опись от root, затем удаление.
async fn full_inventory(c: &sshc::Conn, log: &crate::plugins::LogFn) -> Result<Inventory> {
    let root = c.sudo_mode() != Sudo::None;
    if !root { log(&format!("внимание: нет прав root — опись будет неполной (пароли, чужие каталоги){}", if c.sudo_note().is_empty() { String::new() } else { format!("; {}", c.sudo_note()) })); }
    let (_, bin) = program_for(c, log).await?;
    let dir = stage_bin(c, &bin, log).await?;
    let cmd = format!("{}/hopebackup inventory --config '{{\"_method\":\"ssh\"}}'", crate::util::shq(&dir));
    let o = if root { c.exec_root(&cmd, None, Duration::from_secs(900)).await } else { c.exec(&cmd, None, Duration::from_secs(900)).await };
    let _ = rm_dir(c, &dir, root).await;
    let o = o?;
    if o.code != 0 { bail!("код {}: {}", o.code, o.err().lines().last().unwrap_or("")); }
    let mut inv = serde_json::from_slice::<Inventory>(&o.stdout).map_err(|e| anyhow!("ответ описи: {e}"))?;
    inv.method = "ssh".into();
    Ok(inv)
}

const SHELL_INV: &str = r#"
s() { echo "==HB:$1=="; }
s os; cat /etc/os-release 2>/dev/null || cat /usr/lib/os-release 2>/dev/null
s redhat; cat /etc/redhat-release 2>/dev/null
s debian; cat /etc/debian_version 2>/dev/null
s alpine; cat /etc/alpine-release 2>/dev/null
s astra; cat /etc/astra_version 2>/dev/null
s uname; uname -r; uname -v; uname -m; hostname 2>/dev/null || cat /proc/sys/kernel/hostname
s cmdline; cat /proc/cmdline
s efi; [ -d /sys/firmware/efi ] && echo uefi || echo bios
s boot; for f in /boot/grub2/grub.cfg /boot/grub/grub.cfg /boot/grub/menu.lst /boot/loader/loader.conf /boot/extlinux/extlinux.conf /etc/zipl.conf; do [ -e "$f" ] && echo "$f"; done
s defgrub; cat /etc/default/grub 2>/dev/null
s init; cat /proc/1/comm 2>/dev/null
s cpu; grep -c ^processor /proc/cpuinfo; grep -m1 'model name' /proc/cpuinfo | cut -d: -f2
s mem; grep -E '^(MemTotal|MemAvailable|SwapTotal):' /proc/meminfo
s vgs; vgs --noheadings --nosuffix --units b --separator '|' -o vg_name,vg_size,vg_free 2>/dev/null
s lvs; lvs --noheadings --nosuffix --units b --separator '|' -o vg_name,lv_name,lv_size,lv_attr,lv_path,pool_lv,data_percent 2>/dev/null
s zpool; zpool list -Hp -o name,size,free 2>/dev/null
s dmi; cat /sys/class/dmi/id/sys_vendor /sys/class/dmi/id/product_name 2>/dev/null
s mounts; cat /proc/mounts
s df; df -P -k 2>/dev/null
s passwd; cat /etc/passwd
s ps; ps -eo pid=,ppid=,user=,comm=,args= 2>/dev/null || ps -o pid,ppid,user,comm,args
s cron; cat /etc/crontab /etc/cron.d/* 2>/dev/null | grep -v '^#' | grep -v '^$'
s units; ls /etc/systemd/system/*.service /etc/systemd/system/*.timer 2>/dev/null
s ip; ip -o addr 2>/dev/null || ifconfig 2>/dev/null
s dns; grep -E '^(nameserver|search)' /etc/resolv.conf 2>/dev/null
s selinux; cat /sys/fs/selinux/enforce 2>/dev/null
s tz; cat /etc/timezone 2>/dev/null || readlink /etc/localtime
s id; id -u
s end
"#;

/// Сокращённая опись без программы: ОС, ядро, загрузка, железо, диски, пользователи, процессы, cron.
async fn shell_inventory(c: &sshc::Conn, log: &crate::plugins::LogFn) -> Result<Inventory> {
    let o = if c.sudo_mode() == Sudo::None { c.exec("sh -s", Some(SHELL_INV.as_bytes()), Duration::from_secs(120)).await? } else { c.exec_root("sh -s", Some(SHELL_INV.as_bytes()), Duration::from_secs(120)).await? };
    let text = o.text();
    if !text.contains("==HB:end==") { bail!("сокращённая опись не выполнилась: {}", o.err()); }
    let inv = parse_shell(&text);
    log(&format!("сокращённая опись: {} · {}", inv.system.os.pretty, inv.system.kernel.release));
    Ok(inv)
}

pub fn parse_shell(text: &str) -> Inventory {
    use crate::plugins::host::{inventory as hi, system as hs};
    let mut sec: std::collections::HashMap<String, String> = Default::default();
    let mut cur = String::new();
    for l in text.lines() {
        if let Some(n) = l.strip_prefix("==HB:").and_then(|x| x.strip_suffix("==")) { cur = n.to_string(); continue; }
        if !cur.is_empty() { let e = sec.entry(cur.clone()).or_default(); e.push_str(l); e.push('\n'); }
    }
    let g = |k: &str| sec.get(k).cloned().unwrap_or_default();
    let files: std::collections::HashMap<&str, String> = [("/etc/os-release", g("os")), ("/etc/redhat-release", g("redhat")), ("/etc/debian_version", g("debian")), ("/etc/alpine-release", g("alpine")), ("/etc/astra_version", g("astra"))].into_iter().collect();
    let mut inv = Inventory { method: "shell".into(), time: Utc::now().to_rfc3339(), ..Default::default() };
    inv.system.os = hs::os_from(&|p: &str| files.get(p).cloned().unwrap_or_default());
    let un: Vec<String> = g("uname").lines().map(|s| s.to_string()).collect();
    inv.system.kernel.release = un.first().cloned().unwrap_or_default();
    inv.system.kernel.version = un.get(1).cloned().unwrap_or_default();
    inv.system.kernel.arch = un.get(2).cloned().unwrap_or_default();
    inv.host = un.get(3).cloned().unwrap_or_default();
    inv.system.network.hostname = inv.host.clone();
    inv.system.kernel.cmdline = g("cmdline").trim().to_string();
    inv.system.boot.firmware = g("efi").trim().to_string();
    let boot = g("boot");
    let first = boot.lines().next().unwrap_or("");
    inv.system.boot.bootloader = if first.contains("grub.cfg") { "grub2" } else if first.contains("menu.lst") { "grub-legacy" } else if first.contains("loader.conf") { "systemd-boot" } else if first.contains("extlinux") { "extlinux" } else if first.contains("zipl") { "zipl" } else { "unknown" }.into();
    inv.system.boot.config = first.to_string();
    let dg = g("defgrub");
    let kv = |k: &str| dg.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}="))).map(|v| v.trim_matches('"').to_string()).unwrap_or_default();
    inv.system.boot.grub_cmdline = format!("{} {}", kv("GRUB_CMDLINE_LINUX"), kv("GRUB_CMDLINE_LINUX_DEFAULT")).trim().to_string();
    inv.system.boot.init = g("init").trim().to_string();
    inv.system.boot.custom_params = hs::custom_params(&inv.system.kernel.cmdline);
    let cpu: Vec<String> = g("cpu").lines().map(|s| s.trim().to_string()).collect();
    inv.system.hardware.cpus = cpu.first().and_then(|x| x.parse().ok()).unwrap_or(0);
    inv.system.hardware.cpu_model = cpu.get(1).cloned().unwrap_or_default();
    for l in g("mem").lines() {
        let v = l.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
        if l.starts_with("MemTotal") { inv.system.hardware.mem_total = v; } else if l.starts_with("MemAvailable") { inv.system.hardware.mem_available = v; } else { inv.system.hardware.swap_total = v; }
    }
    inv.system.storage.vgs = hs::parse_vgs(&g("vgs"));
    inv.system.storage.lvs = hs::parse_lvs(&g("lvs"));
    inv.system.storage.zpools = hs::parse_zpools(&g("zpool"));
    let dmi: Vec<String> = g("dmi").lines().map(|s| s.trim().to_string()).collect();
    inv.system.hardware.vendor = dmi.first().cloned().unwrap_or_default();
    inv.system.hardware.product = dmi.get(1).cloned().unwrap_or_default();
    // Монтирования и занятое место.
    let df: std::collections::HashMap<String, (u64, u64)> = g("df").lines().skip(1).filter_map(|l| { let f: Vec<&str> = l.split_whitespace().collect(); (f.len() >= 6).then(|| (f[5].to_string(), (f[1].parse::<u64>().unwrap_or(0) * 1024, f[2].parse::<u64>().unwrap_or(0) * 1024))) }).collect();
    for l in g("mounts").lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 { continue; }
        let (src, target, fs) = (f[0], f[1].replace("\\040", " "), f[2]);
        let class = if ["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs", "glusterfs", "ceph"].contains(&fs) { "network" }
            else if ["proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs", "pstore", "bpf", "debugfs", "tracefs", "mqueue", "hugetlbfs", "configfs", "autofs", "efivarfs", "overlay", "squashfs", "binfmt_misc", "fusectl", "rpc_pipefs"].contains(&fs) || target.starts_with("/proc") || target.starts_with("/sys") || target.starts_with("/dev") || target.starts_with("/run") { "pseudo" }
            else if target.starts_with("/var/lib/docker") || target.starts_with("/snap/") { "docker" }
            else if ["/", "/boot", "/boot/efi", "/usr"].contains(&target.as_str()) { "system" } else { "disk" };
        let (total, used) = df.get(&target).copied().unwrap_or((0, 0));
        if target == "/" { inv.system.storage.root_device = src.into(); inv.system.storage.root_fstype = fs.into(); }
        inv.mounts.push(hi::Mount { target, source: src.into(), fstype: fs.into(), class: class.into(), total, used, root: String::new() });
    }
    for p in hi::parse_passwd(&g("passwd")) {
        if p.uid == 0 || ((1000..60000).contains(&p.uid) && hi::login_shell(&p.shell)) {
            inv.users.push(hi::UserInfo { name: p.name, uid: p.uid, gid: p.gid, home: p.home, login: hi::login_shell(&p.shell), shell: p.shell, ..Default::default() });
        }
    }
    for l in g("ps").lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5 { continue; }
        let Ok(pid) = f[0].parse::<u32>() else { continue };
        let ppid: u32 = f[1].parse().unwrap_or(0);
        if ppid == 2 || pid == 2 { continue; }
        let args = f[4..].join(" ");
        if args.starts_with('[') { continue; }
        inv.processes.push(hi::Proc { pid, ppid, user: f[2].into(), name: f[3].into(), exe: f[4].into(), cmdline: args.chars().take(400).collect(), packaged: true, ..Default::default() });
    }
    for l in g("cron").lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() >= 7 && !l.contains('=') {
            let cmd = w[6..].join(" ");
            inv.schedules.push(hi::Sched { kind: "cron".into(), source: "/etc/crontab, /etc/cron.d".into(), user: w[5].into(), schedule: w[..5].join(" "), scripts: cmd.split_whitespace().filter(|t| t.starts_with('/')).map(|t| t.to_string()).collect(), command: cmd, packaged: false });
        }
    }
    for u in g("units").lines().filter(|x| !x.trim().is_empty()) {
        inv.services.push(hi::Svc { unit: u.rsplit('/').next().unwrap_or(u).into(), file: u.into(), custom: true, ..Default::default() });
    }
    let ipo = g("ip");
    let mut ifaces: Vec<hs::Iface> = Vec::new();
    for l in ipo.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() > 3 && (w[2] == "inet" || w[2] == "inet6") {
            let name = w[1].trim_end_matches(':').to_string();
            if name == "lo" || w[3].starts_with("fe80") { continue; }
            match ifaces.iter_mut().find(|i| i.name == name) { Some(i) => i.addrs.push(w[3].into()), None => ifaces.push(hs::Iface { name, addrs: vec![w[3].into()], ..Default::default() }) }
        }
    }
    inv.system.network.interfaces = ifaces;
    for l in g("dns").lines() {
        if let Some(v) = l.strip_prefix("nameserver") { inv.system.network.dns.push(v.trim().into()); }
        if let Some(v) = l.strip_prefix("search") { inv.system.network.search.push(v.trim().into()); }
    }
    let se = g("selinux");
    inv.system.security.selinux = match se.trim() { "1" => "enforcing".into(), "0" => "permissive".into(), _ => String::new() };
    inv.system.time.timezone = { let t = g("tz").trim().to_string(); t.split("zoneinfo/").nth(1).map(|x| x.to_string()).unwrap_or(t) };
    inv.as_root = g("id").trim() == "0";
    inv.os = inv.system.os.pretty.clone();
    inv.os_id = inv.system.os.id.clone();
    inv.kernel = inv.system.kernel.release.clone();
    // Узнанные продукты — по именам процессов.
    for (name, id, title, plugin, avail) in [("postgres", "postgresql", "PostgreSQL", "postgresql", true), ("mysqld", "mysql", "MySQL / MariaDB", "mysql", false), ("mariadbd", "mysql", "MySQL / MariaDB", "mysql", false), ("mongod", "mongodb", "MongoDB", "mongodb", false), ("redis-server", "redis", "Redis", "redis", false), ("nginx", "nginx", "nginx", "files", true), ("haproxy", "haproxy", "HAProxy", "files", true), ("vault", "vault", "Vault / OpenBao", "vault", true), ("dockerd", "docker", "Docker", "", false)] {
        if inv.processes.iter().any(|p| p.name == name) && !inv.products.iter().any(|p| p.id == id) {
            inv.products.push(hi::Product { id: id.into(), title: title.into(), plugin: plugin.into(), available: avail, note: String::new() });
        }
    }
    inv
}

// ---------------------------------------------------------------- установка агента

#[derive(Deserialize)]
pub struct InstallReq {
    /// Адрес сервера управления для агента (https://сервер:4443).
    server_url: String,
}

pub async fn install_agent(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<InstallReq>) -> R<Json<Value>> {
    let m = machine(&s, &id)?;
    let url = q.server_url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("https://") { return Err(bad("адрес сервера для агента: https://сервер:4443")); }
    let mid = m.id.clone();
    let jid = s.spawn_task("machine_agent", format!("Установка агента на «{}» ({})", m.name, m.host), &w.user, move |s, log| async move { run_install(&s, &mid, &url, log).await });
    s.audit(&w.user, "machine_install_agent", m.name);
    Ok(Json(json!({ "job_id": jid })))
}

fn systemd_unit() -> &'static str {
    "[Unit]\nDescription=HopeBackup — агент\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart=/opt/hopebackup/hopebackup agent run\nEnvironment=HOPEBACKUP_DATA=/var/lib/hopebackup\nEnvironment=HOPEBACKUP_LOG=hopebackup=info,warn\nUser=root\nExecStopPost=-/opt/hopebackup/hopebackup vpn-cleanup\nRestart=always\nRestartSec=3\nLimitNOFILE=1048576\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n"
}

fn openrc_script() -> &'static str {
    "#!/sbin/openrc-run\ndescription=\"HopeBackup — агент\"\ncommand=/opt/hopebackup/hopebackup\ncommand_args=\"agent run\"\ncommand_background=true\npidfile=/run/hopebackup-agent.pid\nexport HOPEBACKUP_DATA=/var/lib/hopebackup\nsupervisor=supervise-daemon\noutput_log=/var/log/hopebackup-agent.log\nerror_log=/var/log/hopebackup-agent.log\ndepend() { need net; }\n"
}

async fn run_install(s: &Arc<Server>, id: &str, url: &str, log: crate::plugins::LogFn) -> Result<Value> {
    let m = machine(s, id)?;
    let c = connect(s, &m).await?;
    if c.sudo_mode() == Sudo::None { bail!("для установки агента нужен root или sudo{}", if c.sudo_note().is_empty() { String::new() } else { format!(" ({})", c.sudo_note()) }); }
    // HOPEBACKUP_TEST_INIT — только для стендовых проверок (tests/machines.sh) и только вместе с HOPEBACKUP_TESTING=1.
    let test_init = if std::env::var("HOPEBACKUP_TESTING").as_deref() == Ok("1") { std::env::var("HOPEBACKUP_TEST_INIT").unwrap_or_default() } else { String::new() };
    let init = if !test_init.is_empty() { test_init } else { c.exec("cat /proc/1/comm 2>/dev/null; [ -x /sbin/openrc-run ] && echo openrc", None, Duration::from_secs(20)).await?.text() };
    let systemd = init.lines().next().map(|l| l.trim() == "systemd").unwrap_or(false);
    let openrc = init.contains("openrc");
    if !systemd && !openrc { bail!("система инициализации {} — установите агента вручную (linux/install.sh)", init.lines().next().unwrap_or("?")); }
    // Одноразовый токен подключения, привязанный к этой машине: подключившийся с ним агент сервер сам
    // привяжет к машине. Токен идёт файлом 0600 в каталоге root (не в командной строке) и отзывается в конце.
    let token = crate::util::token();
    let th = crate::util::sha256_hex(&token);
    s.store.update(|st| {
        st.enroll_tokens.retain(|e| e.expires > Utc::now());
        st.enroll_tokens.push(super::state::EnrollToken { token_hash: th.clone(), expires: Utc::now() + chrono::Duration::hours(1), note: format!("агент для {}", m.name), machine_id: m.id.clone() });
        Ok(())
    })?;
    let r = install_steps(s, &c, &m, url, &token, systemd, &log).await;
    c.close().await;
    // Токен одноразовый: неиспользованный (ошибка, агент уже был подключён) — отозвать.
    let used = s.store.update(|st| { let n = st.enroll_tokens.len(); st.enroll_tokens.retain(|e| e.token_hash != th); Ok(n == st.enroll_tokens.len()) }).unwrap_or(false);
    let remote_id = r?;
    let mid = id.to_string();
    let agent_id = if used {
        // Агент подключился этим токеном — сервер уже привязал его к машине.
        machine(s, id)?.agent_id
    } else {
        // Агент был подключён раньше: принимаем его, только если он известен этому серверу и не принадлежит другой машине.
        let aid = remote_id.unwrap_or_default();
        s.store.update(|st| {
            if aid.is_empty() || !st.agents.iter().any(|a| a.id == aid) { bail!("на машине уже есть агент, но этому серверу он не известен (подключён к другому серверу?) — удалите /var/lib/hopebackup/agent.json на машине и повторите"); }
            if st.machines.iter().any(|x| x.id != mid && x.agent_id == aid) { bail!("агент {aid} уже привязан к другой машине"); }
            if let Some(x) = st.machines.iter_mut().find(|x| x.id == mid) { x.agent_id = aid.clone(); }
            Ok(())
        })?;
        aid
    };
    if agent_id.is_empty() { bail!("агент установлен, но не подключился к серверу"); }
    log(&format!("агент установлен и подключён (id {agent_id}), служба {}", if systemd { "hopebackup-agent (systemd)" } else { "hopebackup-agent (OpenRC)" }));
    Ok(json!({ "agent_id": agent_id }))
}

/// Шаги установки на машине (от root). Возвращает идентификатор из agent.json, если агент уже был подключён.
async fn install_steps(s: &Arc<Server>, c: &sshc::Conn, m: &Machine, url: &str, token: &str, systemd: bool, log: &crate::plugins::LogFn) -> Result<Option<String>> {
    cleanup_remote(c, log).await;
    let (_, bin) = program_for(c, log).await?;
    let tmp = stage_bin(c, &bin, log).await?;
    let q = crate::util::shq(&tmp);
    let put = |name: &str| format!("umask 077; cat > {q}/{name}");
    let o = c.exec_root(&put("unit"), Some(if systemd { systemd_unit() } else { openrc_script() }.as_bytes()), Duration::from_secs(30)).await?;
    if o.code != 0 { let _ = rm_dir(c, &tmp, true).await; bail!("запись файла службы: {}", o.err()); }
    let o = c.exec_root(&put("token"), Some(token.as_bytes()), Duration::from_secs(30)).await?;
    if o.code != 0 { let _ = rm_dir(c, &tmp, true).await; bail!("запись токена: {}", o.err()); }
    let unit_path = if systemd { "/etc/systemd/system/hopebackup-agent.service" } else { "/etc/init.d/hopebackup-agent" };
    let name = m.name.replace(['\'', '"', '\\', '\n'], " ");
    let script = format!(
        "set -e\n\
         trap 'rm -rf {tmp}' EXIT\n\
         mkdir -p /opt/hopebackup /var/lib/hopebackup\n\
         chmod 711 /var/lib/hopebackup\n\
         [ -f /opt/hopebackup/hopebackup ] && cp -a /opt/hopebackup/hopebackup /opt/hopebackup/hopebackup.bak.$(date +%s) || true\n\
         cp {tmp}/hopebackup /opt/hopebackup/hopebackup.new && chmod 0755 /opt/hopebackup/hopebackup.new && mv -f /opt/hopebackup/hopebackup.new /opt/hopebackup/hopebackup\n\
         ln -sf /opt/hopebackup/hopebackup /usr/local/bin/hopebackup\n\
         /opt/hopebackup/hopebackup --version\n\
         if [ ! -f /var/lib/hopebackup/agent.json ]; then HOPEBACKUP_DATA=/var/lib/hopebackup /opt/hopebackup/hopebackup agent enroll --server {url} --token-file {tmp}/token --fingerprint {fp} --name {name}; else echo 'агент уже подключён'; grep -o '\"agent_id\": *\"[^\"]*\"' /var/lib/hopebackup/agent.json; fi\n\
         mkdir -p $(dirname {unit}) && cp {tmp}/unit {unit} && chmod {mode} {unit}\n\
         {start}\n",
        tmp = q, url = crate::util::shq(url), fp = crate::util::shq(&s.fingerprint), name = crate::util::shq(&name),
        mode = if systemd { "0644" } else { "0755" }, unit = unit_path,
        start = if systemd { "systemctl daemon-reload && systemctl enable hopebackup-agent >/dev/null 2>&1 && systemctl restart hopebackup-agent" } else { "rc-update add hopebackup-agent default >/dev/null 2>&1; rc-service hopebackup-agent restart" },
    );
    let o = c.exec_root(&script, None, Duration::from_secs(300)).await?;
    for l in o.text().lines().chain(o.err().lines()) { if !l.trim().is_empty() { log(&format!("  {l}")); } }
    if o.code != 0 { let _ = rm_dir(c, &tmp, true).await; bail!("установка не удалась (код {})", o.code); }
    let existing = o.text().lines().find(|l| l.contains("\"agent_id\"")).and_then(|l| l.split('"').nth(3).map(|x| x.to_string())).filter(|x| x.chars().all(|c| c.is_ascii_alphanumeric()) && !x.is_empty());
    Ok(existing)
}

// ---------------------------------------------------------------- экземпляры по рекомендациям

#[derive(Deserialize)]
pub struct ApplyReq {
    recommendations: Vec<String>,
    site_id: String,
}

pub async fn apply(State(s): S, Extension(w): Extension<Who>, Path(id): Path<String>, Json(q): Json<ApplyReq>) -> R<Json<Value>> {
    let m = machine(&s, &id)?;
    if !has_agent(&s, &m) { return Err(bad("сначала установите агента на эту машину")); }
    let doc = read_doc(&s, &m.id).map_err(|_| bad("сначала выполните опись"))?;
    let inv: Inventory = serde_json::from_value(doc["inventory"].clone()).map_err(|e| bad(&format!("{e}")))?;
    let a = analysis::analyze(&inv, true);
    if let Some(x) = q.recommendations.iter().find(|x| !a.recommendations.iter().any(|r| &r.id == *x)) { return Err(bad(format!("рекомендации «{x}» нет в текущем анализе — обновите страницу"))); }
    let chosen: Vec<&analysis::Rec> = a.recommendations.iter().filter(|r| q.recommendations.contains(&r.id)).collect();
    // Поправки к «серверу целиком»: только добавляемое (диски, каталоги, все домашние каталоги).
    let (mut add_mounts, mut add_paths, mut home_all) = (Vec::<Value>::new(), Vec::<Value>::new(), false);
    for r in chosen.iter().filter(|r| r.action == "config") {
        for x in r.config.get("mounts_add").and_then(|x| x.as_array()).into_iter().flatten() { if !add_mounts.contains(x) { add_mounts.push(x.clone()); } }
        for x in r.config.get("extra_paths_add").and_then(|x| x.as_array()).into_iter().flatten() { if !add_paths.contains(x) { add_paths.push(x.clone()); } }
        if r.config.get("home") == Some(&json!("all")) { home_all = true; }
    }
    let has_cfg = !add_mounts.is_empty() || !add_paths.is_empty() || home_all;
    let existing: Vec<super::state::Instance> = s.store.read(|st| st.instances.iter().filter(|i| i.agent_id == m.agent_id).cloned().collect());
    let host_inst = existing.iter().find(|i| i.plugin == "host").cloned();
    let want_host = chosen.iter().any(|r| r.id == "host");
    if has_cfg && !want_host && host_inst.is_none() { return Err(bad("поправки (диски, каталоги, домашние каталоги) относятся к копии «сервер целиком» — отметьте и её")); }
    let merge = |cfg: &mut Value| {
        for (k, add) in [("mounts", &add_mounts), ("extra_paths", &add_paths)] {
            let mut cur = cfg[k].as_array().cloned().unwrap_or_default();
            for x in add.iter() { if !cur.contains(x) { cur.push(x.clone()); } }
            cfg[k] = json!(cur);
        }
        if home_all { cfg["home"] = json!("all"); }
    };
    let (mut created, mut skipped, mut updated) = (Vec::new(), Vec::new(), Vec::new());
    for r in chosen.iter().filter(|r| r.action == "instance") {
        if r.id != "host" && !r.available { skipped.push(json!({ "id": r.id, "reason": "плагина пока нет — см. рекомендацию" })); continue; }
        if r.id == "host" && host_inst.is_some() { continue; }
        // Тот же продукт на том же порту уже копируется этим агентом — второй экземпляр не создаём.
        let port = r.config.get("port").cloned();
        if let Some(e) = existing.iter().find(|i| i.catalog_id == r.catalog_id && (port.is_none() || i.config.get("port") == port.as_ref())) {
            skipped.push(json!({ "id": r.id, "reason": format!("уже есть экземпляр «{}»", e.name) }));
            continue;
        }
        let mut cfg = r.config.clone();
        if r.id == "host" { merge(&mut cfg); }
        let req = super::api::InstanceReq {
            name: format!("{} · {}", r.title.split(" (").next().unwrap_or(&r.title), m.name),
            catalog_id: r.catalog_id.clone(),
            agent_id: m.agent_id.clone(),
            site_id: q.site_id.clone(),
            config: cfg,
            schedule: Some(super::state::Schedule { enabled: true, full: Some(if r.schedule.is_empty() { "0 1 * * *".into() } else { r.schedule.clone() }), dump: None, incr: None }),
            retention: Some(super::state::Retention { keep_last: r.keep_last.max(1) as usize }),
            rpo_target_min: Some(if r.rpo_min > 0 { r.rpo_min as u64 } else { 1440 }),
            retry_mode: None,
            retry: None,
        };
        let inst = super::api::create_instance(&s, &w.user, req)?;
        created.push(json!({ "id": inst.id, "name": inst.name }));
    }
    if let (Some(h), true) = (&host_inst, has_cfg) {
        // Экземпляр «сервер целиком» уже есть — добавить в него отмеченное.
        let hid = h.id.clone();
        s.store.update(|st| { if let Some(i) = st.instances.iter_mut().find(|i| i.id == hid) { merge(&mut i.config); } Ok(()) })?;
        updated.push(json!({ "id": h.id, "name": h.name }));
    } else if let (Some(h), true) = (&host_inst, want_host) {
        skipped.push(json!({ "id": "host", "reason": format!("уже есть экземпляр «{}»", h.name) }));
    }
    s.audit(&w.user, "machine_apply", format!("{}: {}", m.name, q.recommendations.join(", ")));
    Ok(Json(json!({ "created": created, "updated": updated, "skipped": skipped })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts_secrets() {
        let r = super::redact_str;
        assert_eq!(r("username=bk,password=S3cr3t,vers=3.0"), "username=bk,password=********,vers=3.0");
        assert_eq!(r("mysqldump -uroot -pS3cr3t db"), "mysqldump -uroot -p******** db");
        assert_eq!(r("app --api-token abc123 --port 80"), "app --api-token ******** --port 80");
        assert_eq!(r("PGPASSWORD=x psql"), "PGPASSWORD=******** psql");
        assert_eq!(r("curl https://u:pw@h/x"), "curl https://u:********@h/x");
        assert_eq!(r("postgres -D /var/lib/pg --config-file=/etc/pg.conf"), "postgres -D /var/lib/pg --config-file=/etc/pg.conf");
        assert_eq!(r("ro,relatime"), "ro,relatime");
    }

    #[test]
    fn shell_inventory_parses_local_output() {
        // Сокращённая опись этой машины: тот же сценарий sh, что уходит на машины без программы.
        let o = std::process::Command::new("sh").arg("-s").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(super::SHELL_INV.as_bytes())?;
            c.wait_with_output()
        }).unwrap();
        let inv = super::parse_shell(&String::from_utf8_lossy(&o.stdout));
        assert_eq!(inv.method, "shell");
        assert!(!inv.system.os.pretty.is_empty() && !inv.system.kernel.release.is_empty() && !inv.system.kernel.arch.is_empty());
        assert!(inv.mounts.iter().any(|m| m.target == "/") && !inv.processes.is_empty() && inv.users.iter().any(|u| u.name == "root"));
        assert!(inv.system.hardware.cpus > 0 && inv.system.hardware.mem_total > 0);
        let a = crate::plugins::host::analysis::analyze(&inv, false);
        assert!(a.recommendations.iter().any(|r| r.id == "host"));
    }
}
