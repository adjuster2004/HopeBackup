//! OpenVPN: клиентский профиль .ovpn (сертификаты — внутри профиля), логин/пароль по желанию.
//!
//! Процесс `openvpn` работает дочерним процессом агента (или сервера), на своём интерфейсе `hb…`.
//! Присланные сервером маршруты, шлюз по умолчанию и DNS игнорируются; маршрут ставится только до
//! узла площадки (или до сетей из `routes`), заново после каждого переподключения. Внешние скрипты
//! запрещены (`--script-security 1`), а опасные директивы профиля вырезаются: профиль задаёт оператор
//! в панели, а исполняется он от root на всех агентах, которые пользуются площадкой.

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::net;
use crate::proto::ConnectorSpec;

pub struct OpenVpn {
    pub child: tokio::process::Child,
    pub iface: String,
    pub state: Arc<Mutex<VpnState>>,
}

#[derive(Default, Clone)]
pub struct VpnState {
    pub up: bool,
    pub tail: Vec<String>,
    /// Последние строки с причиной сбоя (проверка сертификата, аутентификация, ошибки параметров).
    pub important: Vec<String>,
    pub reconnects: u64,
}

fn is_important(m: &str) -> bool {
    ["VERIFY", "AUTH_FAILED", "Options error", "Cannot resolve", "Connection refused", "certificate", "Cannot load", "Exiting", "TLS key negotiation failed"].iter().any(|k| m.contains(k)) && !m.contains("VERIFY OK")
}

/// Разрешённые директивы клиентского профиля: ни одна не запускает программ, не читает и не пишет
/// файлов хоста. Профиль задаёт оператор, а исполняется он от root на всех агентах площадки, поэтому
/// проверка — по списку разрешённых, а не запрещённых директив.
const ALLOW: &[&str] = &[
    "client", "tls-client", "pull", "remote", "remote-random", "remote-random-hostname", "proto", "port", "rport", "lport", "nobind", "bind", "local", "float",
    "resolv-retry", "connect-retry", "connect-retry-max", "connect-timeout", "server-poll-timeout", "persist-key", "remote-cert-tls", "remote-cert-ku",
    "remote-cert-eku", "verify-x509-name", "ns-cert-type", "tls-version-min", "tls-version-max", "tls-cipher", "tls-ciphersuites", "tls-groups", "tls-timeout",
    "tls-cert-profile", "key-direction", "cipher", "data-ciphers", "data-ciphers-fallback", "ncp-ciphers", "ncp-disable", "auth", "auth-nocache", "auth-retry",
    "comp-lzo", "compress", "allow-compression", "keepalive", "ping", "ping-restart", "ping-exit", "ping-timer-rem", "inactive", "tun-mtu", "tun-mtu-extra",
    "link-mtu", "mtu-disc", "mssfix", "fragment", "sndbuf", "rcvbuf", "txqueuelen", "explicit-exit-notify", "reneg-sec", "reneg-bytes", "reneg-pkts",
    "hand-window", "tran-window", "verb", "mute", "topology", "push-peer-info", "ifconfig-nowarn", "replay-window", "http-proxy", "http-proxy-option",
    "socks-proxy", "tun-ipv6", "block-ipv6", "mark", "machine-readable-output", "allow-recursive-routing", "key-method", "tls-exit", "single-session",
];

/// Директивы, которые задаёт сам HopeBackup или которые меняют хост (скрипты, маршруты, DNS, журналы,
/// смена пользователя): из профиля вырезаются с пометкой — так типовые выгрузки профилей подходят как есть.
const DROP: &[&str] = &[
    "up", "down", "route-up", "route-pre-down", "ipchange", "tls-verify", "client-connect", "client-disconnect", "learn-address", "auth-user-pass-verify",
    "script-security", "setenv", "setenv-safe", "daemon", "log", "log-append", "writepid", "management", "management-query-passwords", "management-hold",
    "status", "cd", "chroot", "user", "group", "dev", "dev-node", "dev-type", "route", "route-ipv6", "route-gateway", "route-metric", "route-delay", "route-method",
    "redirect-gateway", "redirect-private", "dhcp-option", "block-outside-dns", "register-dns", "route-nopull", "pull-filter", "auth-user-pass", "askpass",
    "iproute", "ifconfig-noexec", "route-noexec", "persist-tun", "mute-replay-warnings", "syslog", "static-challenge", "auth-token-user", "ignore-unknown-option",
];

/// Встроенные блоки (`<ca>…</ca>`): только данные, файлы хоста не читаются.
const INLINE: &[&str] = &["ca", "cert", "key", "tls-auth", "tls-crypt", "tls-crypt-v2", "extra-certs", "pkcs12", "crl-verify", "peer-fingerprint", "secret", "dh", "http-proxy-user-pass", "connection"];

/// Директивы, у которых аргумент — файл: допустимы только со встроенным блоком.
const FILE_REFS: &[&str] = &["ca", "cert", "key", "tls-auth", "tls-crypt", "tls-crypt-v2", "pkcs12", "crl-verify", "secret", "extra-certs", "dh", "http-proxy-user-pass"];

/// Проверить одну строку-директиву. Ok(Some(строка)) — оставить, Ok(None) — вырезать, Err — профиль отклонён.
fn check_line(raw: &str, needs_auth: &mut bool) -> Result<Option<String>> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') { return Ok(Some(raw.to_string())); }
    let mut it = line.split_whitespace();
    let first = it.next().unwrap_or("");
    if first.contains(['"', '\'', '\\']) { bail!("строка профиля «{line}»: директива в кавычках не допускается"); }
    let word = first.trim_start_matches("--").to_string();
    let args: Vec<&str> = it.collect();
    if word == "auth-user-pass" { *needs_auth = true; }
    if DROP.contains(&word.as_str()) { return Ok(None); }
    if FILE_REFS.contains(&word.as_str()) {
        match args.first() {
            Some(&"[inline]") => {}
            Some(n) if word == "tls-auth" && n.parse::<u8>().is_ok() => {}
            Some(a) => bail!("профиль ссылается на файл «{word} {a}» — вставьте содержимое в профиль блоком <{word}>…</{word}>"),
            None => {}
        }
        return Ok(Some(raw.to_string()));
    }
    if !ALLOW.contains(&word.as_str()) {
        bail!("директива «{word}» не поддерживается в профиле коннектора (допустимы клиентские параметры соединения; скрипты, плагины и файлы хоста — нет)");
    }
    // Третий аргумент прокси — файл с паролем или ввод с консоли: только встроенный блок.
    if (word == "http-proxy" || word == "socks-proxy") && args.len() > 2 && !matches!(args[2], "auto" | "auto-nct" | "[inline]") {
        bail!("«{word}»: файл учётных данных прокси не допускается — используйте блок <http-proxy-user-pass>");
    }
    Ok(Some(raw.to_string()))
}

/// Очистить профиль; вернуть текст и признак «профиль требует логин и пароль».
pub fn sanitize_profile(text: &str) -> Result<(String, bool)> {
    let mut out = Vec::new();
    let mut needs_auth = false;
    let mut in_block: Option<String> = None;
    for raw in text.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let line = raw.trim();
        if line.contains('\0') { bail!("профиль содержит нулевой байт"); }
        if let Some(tag) = &in_block {
            if line == format!("</{tag}>") { in_block = None; out.push(raw.to_string()); continue; }
            if tag == "connection" {
                // Внутри <connection> — те же директивы и те же правила.
                if line.starts_with('<') { bail!("вложенные блоки внутри <connection> не поддерживаются"); }
                if let Some(l) = check_line(raw, &mut needs_auth)? { out.push(l); } else { out.push(format!("# hopebackup: убрано «{}»", line.split_whitespace().next().unwrap_or(""))); }
            } else {
                out.push(raw.to_string());
            }
            continue;
        }
        if line.starts_with('<') && line.ends_with('>') && !line.starts_with("</") {
            let tag = line.trim_matches(|c| c == '<' || c == '>').to_string();
            if !INLINE.contains(&tag.as_str()) { bail!("блок <{tag}> не поддерживается в профиле коннектора"); }
            in_block = Some(tag);
            out.push(raw.to_string());
            continue;
        }
        match check_line(raw, &mut needs_auth)? {
            Some(l) => out.push(l),
            None => out.push(format!("# hopebackup: убрано «{}»", line.split_whitespace().next().unwrap_or("").trim_start_matches("--"))),
        }
    }
    if let Some(t) = in_block { bail!("профиль обрезан: не закрыт блок <{t}>"); }
    Ok((out.join("\n") + "\n", needs_auth))
}

/// Адреса серверов из директив `remote` (чтобы не завернуть их в сам туннель).
pub async fn remote_hosts(profile: &str) -> Vec<Ipv4Addr> {
    let mut v = Vec::new();
    for l in profile.lines() {
        let mut it = l.split_whitespace();
        if it.next() == Some("remote") {
            if let Some(h) = it.next() { if let Ok(a) = net::resolve_host(h).await { v.extend(a); } }
        }
    }
    v
}

pub async fn up(c: &ConnectorSpec, routes: Vec<String>, status: Arc<dyn Fn(Value) + Send + Sync>) -> Result<OpenVpn> {
    net::require_root("OpenVPN")?;
    let bin = net::need("openvpn", "openvpn")?;
    let p = &c.params;
    let profile = net::p_raw(p, "profile");
    if profile.trim().is_empty() { bail!("коннектор «{}»: не задан профиль .ovpn", c.name); }
    let (clean, needs_auth) = sanitize_profile(&profile)?;
    let dir = net::work_dir(&c.id)?;
    let iface = net::iface_name(&c.id);
    net::del_iface(&iface).await;
    let conf = dir.join("profile.ovpn");
    crate::util::write_secret(&conf, clean.as_bytes())?;
    let user = net::p_str(p, "username");
    let pass = net::p_raw(p, "password");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--config").arg(&conf);
    // Наши параметры идут после профиля и имеют приоритет.
    cmd.args(["--dev", &iface, "--dev-type", "tun", "--route-nopull", "--pull-filter", "ignore", "redirect-gateway", "--pull-filter", "ignore", "dhcp-option",
        "--pull-filter", "ignore", "route-ipv6", "--pull-filter", "ignore", "block-outside-dns", "--script-security", "1", "--verb", "3", "--connect-retry", "5", "30",
        "--resolv-retry", "infinite", "--auth-nocache"]);
    if !user.is_empty() || needs_auth {
        if user.is_empty() { bail!("профиль требует логин и пароль (auth-user-pass) — заполните их в коннекторе"); }
        let f = dir.join("auth.txt");
        crate::util::write_secret(&f, format!("{user}\n{pass}\n").as_bytes())?;
        cmd.arg("--auth-user-pass").arg(&f);
    }
    let kp = net::p_raw(p, "key_password");
    if !kp.is_empty() {
        let f = dir.join("askpass.txt");
        crate::util::write_secret(&f, format!("{kp}\n").as_bytes())?;
        cmd.arg("--askpass").arg(&f);
    }
    cmd.current_dir(&dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| anyhow!("запуск openvpn: {e}"))?;
    let state = Arc::new(Mutex::new(VpnState::default()));
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel::<std::result::Result<(), String>>(4);
    let avoid = remote_hosts(&clean).await;
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)].into_iter().flatten() {
        let (state, iface, routes, avoid, status, ready_tx, name) = (state.clone(), iface.clone(), routes.clone(), avoid.clone(), status.clone(), ready_tx.clone(), c.name.clone());
        // Читаем вывод постоянно: и чтобы заметить подключение/ошибку, и чтобы openvpn не встал на полном канале.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let msg = strip_ts(&l);
                {
                    let mut s = state.lock();
                    s.tail.push(msg.clone());
                    if s.tail.len() > 30 { s.tail.remove(0); }
                    if is_important(&msg) && !s.important.contains(&msg) {
                        s.important.push(msg.clone());
                        if s.important.len() > 3 { s.important.remove(0); }
                    }
                }
                if l.contains("Initialization Sequence Completed") {
                    let r = net::add_routes(&iface, &routes, &avoid).await;
                    let ip = net::iface_ipv4(&iface).await;
                    {
                        let mut s = state.lock();
                        if s.up { s.reconnects += 1; }
                        s.up = r.is_ok();
                    }
                    match &r {
                        Ok(()) => { tracing::info!("OpenVPN «{name}»: подключён, {iface} {}, маршруты {}", ip.clone().unwrap_or_default(), routes.join(", ")); status(json!({ "up": true, "iface": iface, "address": ip, "routes": routes, "error": null })); }
                        Err(e) => status(json!({ "up": false, "iface": iface, "error": format!("маршруты: {e:#}") })),
                    }
                    let _ = ready_tx.try_send(r.map_err(|e| format!("{e:#}")));
                } else if l.contains("AUTH_FAILED") || l.contains("Exiting due to fatal error") {
                    // «Options error … [PUSH-OPTIONS]» — сервер прислал то, что мы не принимаем: не фатально.
                    // Ошибки собственных параметров завершают процесс — это ловит ожидание ниже.
                    state.lock().up = false;
                    status(json!({ "up": false, "iface": iface, "error": msg }));
                    let _ = ready_tx.try_send(Err(msg.clone()));
                } else if l.contains("SIGUSR1[soft") || l.contains("Restart pause") || l.contains("Inactivity timeout") {
                    state.lock().up = false;
                    status(json!({ "up": false, "iface": iface, "error": format!("переподключение: {msg}") }));
                }
            }
        });
    }
    let deadline = Duration::from_secs(net::p_u64(p, "timeout", 60));
    let r = tokio::time::timeout(deadline, async {
        loop {
            tokio::select! {
                m = ready_rx.recv() => return m.unwrap_or(Err("openvpn завершился".into())),
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if let Ok(Some(st)) = child.try_wait() { return Err(format!("openvpn завершился с кодом {}", st.code().unwrap_or(-1))); }
                }
            }
        }
    })
    .await;
    let tail = || {
        let s = state.lock();
        let mut v: Vec<String> = s.important.clone();
        for l in s.tail.iter().rev().take(3).rev() { if !v.contains(l) { v.push(l.clone()); } }
        v.join(" | ")
    };
    match r {
        Ok(Ok(())) => Ok(OpenVpn { child, iface, state }),
        Ok(Err(e)) => { let _ = child.kill().await; bail!("OpenVPN «{}»: {e} ({})", c.name, tail()) }
        Err(_) => { let _ = child.kill().await; bail!("OpenVPN «{}»: нет подключения за {} с ({})", c.name, deadline.as_secs(), tail()) }
    }
}

/// Убрать метку времени `2026-10-24 18:06:31 ` из строки журнала openvpn.
fn strip_ts(l: &str) -> String {
    let b = l.as_bytes();
    if b.len() > 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':' { l[20..].trim().to_string() } else { l.trim().to_string() }
}

impl OpenVpn {
    pub fn alive(&mut self) -> bool { matches!(self.child.try_wait(), Ok(None)) && net::iface_exists(&self.iface) }
    pub fn up_now(&self) -> bool { self.state.lock().up }
    pub fn tail(&self) -> String { self.state.lock().tail.iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join(" | ") }
    pub async fn down(&mut self) { let _ = self.child.kill().await; net::del_iface(&self.iface).await; }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize() {
        let p = "client\ndev tun0\nremote vpn.example 1194\nscript-security 2\nup /tmp/evil.sh\nredirect-gateway def1\nauth-user-pass\n<ca>\n-----BEGIN CERTIFICATE-----\nup inside-block-is-data\n-----END CERTIFICATE-----\n</ca>\n";
        let (s, auth) = sanitize_profile(p).unwrap();
        assert!(auth);
        assert!(!s.lines().any(|l| l.starts_with("up /tmp/evil.sh") || l.starts_with("script-security") || l.starts_with("redirect-gateway") || l.starts_with("dev ")));
        assert!(s.contains("up inside-block-is-data"), "содержимое блоков не трогаем");
        assert!(s.contains("remote vpn.example 1194"));
        assert!(sanitize_profile("client\nca /etc/openvpn/ca.crt\n").is_err());
        assert!(sanitize_profile("client\ntls-auth [inline] 1\n<tls-auth>\nx\n</tls-auth>\n").is_ok());
        assert!(sanitize_profile("client\n<ca>\nx\n").is_err());
        // Неизвестные и опасные директивы — отказ, а не пропуск.
        for bad in ["config /etc/other.conf", "plugin /lib/x.so", "providers legacy", "tls-export-cert /tmp", "tmp-dir /tmp", "\"up\" /bin/x", "engine dynamic", "http-proxy h 8080 /etc/passwd"] {
            assert!(sanitize_profile(&format!("client\n{bad}\n")).is_err(), "{bad}");
        }
        assert!(sanitize_profile("client\n<script>\nx\n</script>\n").is_err());
        assert!(sanitize_profile("client\n<connection>\nremote a 1194\nup /bin/x\n</connection>\n").unwrap().0.contains("убрано «up»"));
        assert!(sanitize_profile("client\n<connection>\nremote a 1194\nplugin x.so\n</connection>\n").is_err());
        assert!(sanitize_profile("--client\n\tremote vpn 1194 udp\nverb 3\n").is_ok());
    }
}
