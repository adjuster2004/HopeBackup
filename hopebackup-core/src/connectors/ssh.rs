//! SSH-туннель (`ssh -L`) системным OpenSSH: локальный порт → узел хранения за SSH-сервером площадки.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::net::p_str;
use crate::proto::ConnectorSpec;

pub struct Ssh {
    pub child: tokio::process::Child,
    pub port: u16,
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

pub async fn port_open(port: u16) -> bool {
    tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(("127.0.0.1", port))).await.map(|r| r.is_ok()).unwrap_or(false)
}

fn p_u16(v: &Value, k: &str, d: u16) -> u16 { v.get(k).and_then(|x| x.as_u64()).map(|x| x as u16).filter(|x| *x > 0).unwrap_or(d) }

pub async fn up(c: &ConnectorSpec) -> Result<(Ssh, Value)> {
    let p = &c.params;
    let host = p_str(p, "host");
    if host.is_empty() { bail!("коннектор «{}»: не указан SSH-сервер", c.name); }
    let user = { let u = p_str(p, "user"); if u.is_empty() { "hopebackup".to_string() } else { u } };
    let rhost = { let h = p_str(p, "remote_host"); if h.is_empty() { "127.0.0.1".to_string() } else { h } };
    let rport = p_u16(p, "remote_port", 8443);
    let sport = p_u16(p, "port", 22);
    let key = p_str(p, "private_key");
    let jump = p_str(p, "jump");
    let dir = crate::agent::data_dir().join("ssh");
    std::fs::create_dir_all(&dir)?;
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-N", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15"]);
    cmd.arg("-o").arg(format!("UserKnownHostsFile={}", dir.join("known_hosts").display()));
    if !key.is_empty() {
        let kf: PathBuf = dir.join(format!("{}.key", c.id));
        let mut k = key.replace("\r\n", "\n");
        if !k.ends_with('\n') { k.push('\n'); }
        crate::util::write_secret(&kf, k.as_bytes())?;
        cmd.arg("-i").arg(&kf).args(["-o", "IdentitiesOnly=yes"]);
    }
    if !jump.is_empty() { cmd.arg("-J").arg(&jump); }
    let lport = free_port()?;
    cmd.arg("-p").arg(sport.to_string()).arg("-L").arg(format!("127.0.0.1:{lport}:{rhost}:{rport}")).arg(format!("{user}@{host}"));
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("запуск ssh (нужен клиент OpenSSH)")?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(Some(st)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() { let _ = tokio::io::AsyncReadExt::read_to_string(&mut e, &mut err).await; }
            bail!("SSH-туннель «{}» не поднялся (код {}): {}", c.name, st.code().unwrap_or(-1), err.trim());
        }
        if port_open(lport).await {
            tracing::info!("SSH-туннель «{}»: 127.0.0.1:{lport} → {rhost}:{rport} через {user}@{host}", c.name);
            return Ok((Ssh { child, port: lport }, json!({ "up": true, "local_port": lport, "error": null })));
        }
    }
    let _ = child.kill().await;
    bail!("SSH-туннель «{}»: таймаут подключения", c.name)
}

impl Ssh {
    pub async fn alive(&mut self) -> bool { matches!(self.child.try_wait(), Ok(None)) && port_open(self.port).await }
    pub async fn down(&mut self) { let _ = self.child.kill().await; }
}
