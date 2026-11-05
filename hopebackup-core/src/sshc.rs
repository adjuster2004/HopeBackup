//! Клиент SSH (без внешнего ssh): вход по паролю (в том числе keyboard-interactive, как в PAM) или по
//! ключу, проверка ключа хоста по закреплённому отпечатку (первое подключение — запоминание), выполнение
//! команд, передача файлов, повышение прав через sudo (без пароля или с паролем через stdin).

use anyhow::{anyhow, bail, Context, Result};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Auth {
    pub user: String,
    pub password: String,
    pub private_key: String,
    pub key_passphrase: String,
    /// Пароль sudo, если отличается от пароля входа (для входа по ключу).
    pub sudo_password: String,
}

struct Handler {
    expected: String,
    seen: Arc<parking_lot::Mutex<String>>,
}

fn fp_of(k: &PublicKeyOrCertificate) -> String {
    match k {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
        PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256).to_string(),
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = fp_of(key);
        *self.seen.lock() = fp.clone();
        // Пустой ожидаемый отпечаток — первое подключение: принимаем и запоминаем.
        Ok(self.expected.is_empty() || self.expected == fp)
    }
}

pub struct Conn {
    h: client::Handle<Handler>,
    pub host_key: String,
    pub user: String,
    sudo: Sudo,
    sudo_password: String,
    note: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sudo {
    /// Вошли под root.
    Root,
    /// sudo без пароля.
    NoPass,
    /// sudo с паролем (через stdin).
    Pass,
    /// Прав root нет.
    None,
}

#[derive(Debug, Default)]
pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Out {
    pub fn text(&self) -> String { String::from_utf8_lossy(&self.stdout).to_string() }
    pub fn err(&self) -> String { String::from_utf8_lossy(&self.stderr).trim().to_string() }
}

/// Подключиться и войти. `expected_fp` — закреплённый отпечаток ключа хоста (пусто — первый раз).
/// Подключение, вход и определение sudo вместе ограничены 90 с.
pub async fn connect(host: &str, port: u16, auth: &Auth, expected_fp: &str) -> Result<Conn> {
    match tokio::time::timeout(Duration::from_secs(90), connect_inner(host, port, auth, expected_fp)).await {
        Ok(r) => r,
        Err(_) => bail!("{host}:{port}: вход не завершился за 90 с"),
    }
}

/// Предел вывода одной команды (опись крупного сервера — единицы МБ).
pub const MAX_OUT: usize = 64 << 20;
const MAX_ERR: usize = 1 << 20;
const SUDO_PW: &str = "sudo -k -S -p '' true";

async fn connect_inner(host: &str, port: u16, auth: &Auth, expected_fp: &str) -> Result<Conn> {
    let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(900)), keepalive_interval: Some(Duration::from_secs(30)), nodelay: true, ..Default::default() });
    let seen = Arc::new(parking_lot::Mutex::new(String::new()));
    let handler = Handler { expected: expected_fp.trim().to_string(), seen: seen.clone() };
    let addr = format!("{}:{port}", host.trim().trim_matches(|c| c == '[' || c == ']'));
    let fut = client::connect(cfg, addr.clone(), handler);
    let mut h = match tokio::time::timeout(Duration::from_secs(30), fut).await {
        Err(_) => bail!("{addr}: нет ответа за 30 с"),
        Ok(Err(e)) => {
            let s = seen.lock().clone();
            if !expected_fp.is_empty() && !s.is_empty() && s != expected_fp.trim() {
                bail!("ключ хоста {addr} изменился: был {expected_fp}, сейчас {s} — если машину переустановили, сбросьте закреплённый ключ (возможна подмена сервера)");
            }
            return Err(anyhow!("{addr}: {e}"));
        }
        Ok(Ok(h)) => h,
    };
    let host_key = seen.lock().clone();
    let user = auth.user.trim().to_string();
    if user.is_empty() { bail!("не задан пользователь SSH"); }
    let mut ok = false;
    let mut tried = Vec::new();
    if !auth.private_key.trim().is_empty() {
        let pass = if auth.key_passphrase.is_empty() { None } else { Some(auth.key_passphrase.as_str()) };
        let key = russh::keys::decode_secret_key(auth.private_key.trim(), pass).map_err(|e| anyhow!("закрытый ключ: {e} (нужен ключ OpenSSH или PEM; зашифрованный — с парольной фразой)"))?;
        let hash = h.best_supported_rsa_hash().await.ok().flatten().flatten();
        let r = h.authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
        ok = r.success();
        tried.push("ключ");
    }
    if !ok && !auth.password.is_empty() {
        let r = h.authenticate_password(&user, &auth.password).await?;
        ok = r.success();
        tried.push("пароль");
        if !ok {
            // Многие системы (PAM) принимают пароль только как keyboard-interactive.
            let mut resp = h.authenticate_keyboard_interactive_start(&user, None).await?;
            for _ in 0..5 {
                match resp {
                    client::KeyboardInteractiveAuthResponse::Success => { ok = true; break; }
                    client::KeyboardInteractiveAuthResponse::Failure { .. } => break,
                    client::KeyboardInteractiveAuthResponse::InfoRequest { ref prompts, .. } => {
                        let answers = prompts.iter().map(|_| auth.password.clone()).collect();
                        resp = h.authenticate_keyboard_interactive_respond(answers).await?;
                    }
                }
            }
            tried.push("keyboard-interactive");
        }
    }
    if !ok {
        if tried.is_empty() { bail!("укажите пароль или закрытый ключ"); }
        bail!("{user}@{addr}: вход не выполнен ({}) — проверьте логин, пароль или ключ; если пароли запрещены (PasswordAuthentication no), используйте ключ", tried.join(", "));
    }
    let mut c = Conn { h, host_key, user: user.clone(), sudo: Sudo::None, note: String::new(), sudo_password: if auth.sudo_password.is_empty() { auth.password.clone() } else { auth.sudo_password.clone() } };
    c.sudo = c.detect_sudo().await;
    Ok(c)
}

impl Conn {
    pub fn sudo_mode(&self) -> Sudo { self.sudo }

    /// Выполнить команду (без повышения прав); `stdin` — данные на вход. `timeout` — на всю команду,
    /// включая открытие канала и передачу stdin; вывод больше `MAX_OUT` — ошибка.
    pub async fn exec(&self, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<Out> {
        let res = tokio::time::timeout(timeout, async {
            let mut ch = self.h.channel_open_session().await.context("канал SSH")?;
            ch.exec(true, cmd).await?;
            if let Some(d) = stdin { ch.data(d).await?; }
            ch.eof().await?;
            let mut out = Out { code: -1, ..Default::default() };
            while let Some(m) = ch.wait().await {
                match m {
                    ChannelMsg::Data { data } => {
                        if out.stdout.len() + data.len() > MAX_OUT { let _ = ch.close().await; bail!("вывод команды больше {} МБ — прервано", MAX_OUT >> 20); }
                        out.stdout.extend_from_slice(&data)
                    }
                    ChannelMsg::ExtendedData { data, .. } => { if out.stderr.len() < MAX_ERR { out.stderr.extend_from_slice(&data[..data.len().min(MAX_ERR - out.stderr.len())]); } }
                    ChannelMsg::ExitStatus { exit_status } => out.code = exit_status as i32,
                    _ => {}
                }
            }
            Ok::<Out, anyhow::Error>(out)
        })
        .await;
        match res {
            Err(_) => bail!("команда не завершилась за {} с", timeout.as_secs()),
            Ok(r) => r,
        }
    }

    async fn detect_sudo(&mut self) -> Sudo {
        if let Ok(o) = self.exec("id -u", None, Duration::from_secs(20)).await { if o.text().trim() == "0" { return Sudo::Root; } }
        if let Ok(o) = self.exec("sudo -n true", None, Duration::from_secs(20)).await {
            if o.code == 0 { return Sudo::NoPass; }
            if o.err().contains("requiretty") || o.err().contains("must have a tty") { self.note = format!("sudo требует терминал (Defaults requiretty) — разрешите sudo без терминала для {}", self.user); return Sudo::None; }
        }
        if !self.sudo_password.is_empty() {
            let mut input = self.sudo_password.clone().into_bytes();
            input.push(b'\n');
            if let Ok(o) = self.exec(SUDO_PW, Some(&input), Duration::from_secs(20)).await.map(|o| (o.code, o.err())) {
                if o.0 == 0 { return Sudo::Pass; }
                self.note = if o.1.contains("requiretty") || o.1.contains("tty") { format!("sudo требует терминал (Defaults requiretty) — разрешите sudo без терминала для {}", self.user) } else if o.1.contains("not in the sudoers") || o.1.contains("not allowed") { format!("{} не может выполнять sudo", self.user) } else { "пароль sudo не подошёл".into() };
            }
        }
        Sudo::None
    }

    /// Почему нет прав root (если известно).
    pub fn sudo_note(&self) -> &str { &self.note }

    /// Выполнить от root (sudo при необходимости). Для sudo с паролем пароль идёт первой строкой stdin;
    /// `-k` — всегда спрашивать пароль, чтобы закэшированные права не оставили пароль в данных команды.
    pub async fn exec_root(&self, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<Out> {
        let sh = crate::util::shq(cmd);
        match self.sudo {
            Sudo::Root => self.exec(&format!("sh -c {sh}"), stdin, timeout).await,
            Sudo::NoPass => self.exec(&format!("sudo -n sh -c {sh}"), stdin, timeout).await,
            Sudo::Pass => {
                let mut input = self.sudo_password.clone().into_bytes();
                input.push(b'\n');
                if let Some(d) = stdin { input.extend_from_slice(d); }
                self.exec(&format!("{} sh -c {sh}", SUDO_PW.trim_end_matches(" true")), Some(&input), timeout).await
            }
            Sudo::None => bail!("у пользователя {} нет прав root: войдите под root или дайте sudo{}", self.user, if self.note.is_empty() { String::new() } else { format!(" ({})", self.note) }),
        }
    }

    /// Команда от root и то, что нужно передать в stdin до данных (пароль sudo).
    pub fn root_wrap(&self, cmd: &str) -> Result<(String, Vec<u8>)> {
        let sh = crate::util::shq(cmd);
        Ok(match self.sudo {
            Sudo::Root => (format!("sh -c {sh}"), Vec::new()),
            Sudo::NoPass => (format!("sudo -n sh -c {sh}"), Vec::new()),
            Sudo::Pass => { let mut p = self.sudo_password.clone().into_bytes(); p.push(b'\n'); (format!("{} sh -c {sh}", SUDO_PW.trim_end_matches(" true")), p) }
            Sudo::None => bail!("у пользователя {} нет прав root: войдите под root или дайте sudo", self.user),
        })
    }

    /// Выполнить команду и отдавать её stdout потоком (большие файлы: образы дисков, дампы).
    /// Возвращает читатель (блокирующий — для потока в репозиторий) и задачу с кодом выхода и stderr.
    pub async fn stream_out(&self, cmd: &str, prefix: Vec<u8>) -> Result<(ChanReader, tokio::task::JoinHandle<Result<(i32, String)>>)> {
        let mut ch = self.h.channel_open_session().await.context("канал SSH")?;
        ch.exec(true, cmd).await?;
        if !prefix.is_empty() { ch.data(&prefix[..]).await?; }
        ch.eof().await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let jh = tokio::spawn(async move {
            let (mut code, mut err) = (-1, Vec::new());
            let mut open = true;
            while let Some(m) = ch.wait().await {
                match m {
                    ChannelMsg::Data { data } => { if open && tx.send(data.to_vec()).await.is_err() { open = false; } }
                    ChannelMsg::ExtendedData { data, .. } => { if err.len() < MAX_ERR { err.extend_from_slice(&data[..data.len().min(MAX_ERR - err.len())]); } }
                    ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
                    _ => {}
                }
            }
            Ok((code, String::from_utf8_lossy(&err).trim().to_string()))
        });
        Ok((ChanReader { rx, buf: Vec::new(), pos: 0 }, jh))
    }

    /// Выполнить команду, передав ей в stdin поток `r` (восстановление образа диска).
    pub async fn stream_in<R: std::io::Read + Send + 'static>(&self, cmd: &str, prefix: Vec<u8>, r: R) -> Result<(i32, String)> {
        let mut ch = self.h.channel_open_session().await.context("канал SSH")?;
        ch.exec(true, cmd).await?;
        if !prefix.is_empty() { ch.data(&prefix[..]).await?; }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(16);
        let reader = tokio::task::spawn_blocking(move || {
            let mut r = r;
            loop {
                let mut b = vec![0u8; 1 << 20];
                match r.read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => { b.truncate(n); if tx.blocking_send(Ok(b)).is_err() { break; } }
                    Err(e) => { let _ = tx.blocking_send(Err(e)); break; }
                }
            }
        });
        while let Some(chunk) = rx.recv().await {
            let chunk = chunk.context("чтение данных для передачи")?;
            ch.data(&chunk[..]).await.context("передача по SSH")?;
        }
        let _ = reader.await;
        ch.eof().await?;
        let (mut code, mut err) = (-1, Vec::new());
        while let Some(m) = ch.wait().await {
            match m {
                ChannelMsg::ExtendedData { data, .. } => { if err.len() < MAX_ERR { err.extend_from_slice(&data[..data.len().min(MAX_ERR - err.len())]); } }
                ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
                _ => {}
            }
        }
        Ok((code, String::from_utf8_lossy(&err).trim().to_string()))
    }

    /// Записать файл (от текущего пользователя) с правами `mode`.
    pub async fn upload(&self, path: &str, data: &[u8], mode: &str) -> Result<()> {
        let p = crate::util::shq(path);
        let o = self.exec(&format!("umask 077; cat > {p} && chmod {mode} {p}"), Some(data), Duration::from_secs(600)).await?;
        if o.code != 0 { bail!("запись {path}: {}", o.err()); }
        Ok(())
    }

    pub async fn close(self) { let _ = self.h.disconnect(russh::Disconnect::ByApplication, "", "ru").await; }
}

/// Блокирующий читатель данных канала SSH (для потока в репозиторий из отдельного потока).
pub struct ChanReader { rx: tokio::sync::mpsc::Receiver<Vec<u8>>, buf: Vec<u8>, pos: usize }

impl std::io::Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.blocking_recv() { Some(b) => { self.buf = b; self.pos = 0; } None => return Ok(0) }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
