//! WireGuard: интерфейс `hb…` в ядре Linux (5.6+) или в пространстве пользователя (wireguard-go,
//! boringtun), если модуля ядра нет. Конфигурацию собираем сами (`wg setconf`), без wg-quick:
//! wg-quick меняет DNS и маршрутизацию хоста, а нам нужен только путь до площадки.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

use super::net;
use crate::proto::ConnectorSpec;

pub struct WireGuard {
    pub iface: String,
    /// Процесс userspace-реализации (если ядро без WireGuard).
    pub child: Option<tokio::process::Child>,
    pub mode: String,
}

/// Новая пара ключей (закрытый, открытый) в base64, как у `wg genkey | wg pubkey`.
pub fn genkey() -> (String, String) {
    let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public = x25519_dalek::PublicKey::from(&secret);
    let e = base64::engine::general_purpose::STANDARD;
    (e.encode(secret.to_bytes()), e.encode(public.as_bytes()))
}

fn key32(s: &str, what: &str) -> Result<[u8; 32]> {
    let b = base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|_| anyhow!("{what}: неверный base64"))?;
    b.try_into().map_err(|_| anyhow!("{what}: ключ WireGuard должен быть 32 байта"))
}

/// Открытый ключ по закрытому (показываем в панели, чтобы добавить пира на стороне площадки).
pub fn pubkey(private: &str) -> Result<String> {
    let s = x25519_dalek::StaticSecret::from(key32(private, "закрытый ключ")?);
    Ok(base64::engine::general_purpose::STANDARD.encode(x25519_dalek::PublicKey::from(&s).as_bytes()))
}

/// Текст для `wg setconf`.
/// `хост:порт` или `[IPv6]:порт`; только символы имени хоста и адреса.
fn check_endpoint(e: &str) -> Result<()> {
    let bad = || anyhow!("адрес сервера WireGuard «{e}»: ожидается хост:порт");
    let (h, port) = e.rsplit_once(':').ok_or_else(bad)?;
    let port: u32 = port.parse().map_err(|_| bad())?;
    if port == 0 || port > 65535 { return Err(bad()); }
    let h = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h);
    if h.is_empty() || !h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:_".contains(c)) { return Err(bad()); }
    Ok(())
}

pub fn wg_conf(p: &Value, allowed: &[String]) -> Result<String> {
    let private = net::p_str(p, "private_key");
    let peer = net::p_str(p, "peer_public_key");
    let endpoint = net::p_str(p, "endpoint");
    if private.is_empty() { bail!("не задан закрытый ключ (кнопка «Создать ключи»)"); }
    if peer.is_empty() { bail!("не задан открытый ключ сервера площадки"); }
    if endpoint.is_empty() { bail!("не задан адрес сервера площадки (endpoint, хост:порт)"); }
    check_endpoint(&endpoint)?;
    key32(&private, "закрытый ключ")?;
    key32(&peer, "открытый ключ сервера")?;
    let mut s = format!("[Interface]\nPrivateKey = {private}\n");
    let lp = net::p_u64(p, "listen_port", 0);
    if lp > 0 { s += &format!("ListenPort = {lp}\n"); }
    s += &format!("\n[Peer]\nPublicKey = {peer}\n");
    let psk = net::p_str(p, "preshared_key");
    if !psk.is_empty() { key32(&psk, "общий ключ")?; s += &format!("PresharedKey = {psk}\n"); }
    s += &format!("Endpoint = {endpoint}\nAllowedIPs = {}\nPersistentKeepalive = {}\n", allowed.join(", "), net::p_u64(p, "keepalive", 25));
    Ok(s)
}

async fn create_iface(iface: &str, mode: &str) -> Result<(Option<tokio::process::Child>, String)> {
    if mode == "auto" || mode == "kernel" {
        match net::ip(&["link", "add", "dev", iface, "type", "wireguard"]).await {
            Ok(_) => return Ok((None, "kernel".into())),
            Err(e) if mode == "kernel" => bail!("модуль WireGuard в ядре недоступен: {e:#}"),
            Err(_) => {}
        }
    }
    // Пространство пользователя: сначала wireguard-go, затем boringtun.
    let cands: Vec<(&str, Vec<&str>)> = match mode {
        "wireguard-go" => vec![("wireguard-go", vec!["-f"])],
        "boringtun" => vec![("boringtun-cli", vec!["-f", "--disable-drop-privileges"]), ("boringtun", vec!["-f", "--disable-drop-privileges"])],
        _ => vec![("wireguard-go", vec!["-f"]), ("boringtun-cli", vec!["-f", "--disable-drop-privileges"]), ("boringtun", vec!["-f", "--disable-drop-privileges"])],
    };
    for (prog, args) in cands {
        let Some(bin) = net::which(prog) else { continue };
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args).arg(iface).env("WG_SUDO", "1").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("запуск {prog}"))?;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if net::iface_exists(iface) { return Ok((Some(child), prog.to_string())); }
            if let Ok(Some(st)) = child.try_wait() { bail!("{prog} завершился с кодом {}", st.code().unwrap_or(-1)); }
        }
        let _ = child.kill().await;
        bail!("{prog} не создал интерфейс {iface}");
    }
    bail!("в ядре нет WireGuard (нужно ядро 5.6+ или модуль wireguard), а userspace-реализация не установлена (wireguard-go или boringtun)")
}

pub async fn up(c: &ConnectorSpec, routes: Vec<String>) -> Result<(WireGuard, Value)> {
    net::require_root("WireGuard")?;
    let wg = net::need("wg", "wireguard-tools")?;
    let p = &c.params;
    let address = net::p_str(p, "address");
    if address.is_empty() { bail!("коннектор «{}»: не задан адрес интерфейса (например, 10.66.0.2/32)", c.name); }
    let (aip, apfx) = net::parse_cidr(&address)?;
    let allowed = routes;
    for a in &allowed {
        let (_, pfx) = net::parse_cidr(a)?;
        if pfx == 0 { bail!("AllowedIPs {a} перехватит весь трафик хоста — укажите сеть площадки"); }
    }
    let conf = wg_conf(p, &allowed)?;
    let dir = net::work_dir(&c.id)?;
    let cf = dir.join("wg.conf");
    crate::util::write_secret(&cf, conf.as_bytes())?;
    let iface = net::iface_name(&c.id);
    net::del_iface(&iface).await;
    let mode = { let m = net::p_str(p, "implementation"); if m.is_empty() { "auto".to_string() } else { m } };
    let (child, impl_name) = create_iface(&iface, &mode).await?;
    let mut w = WireGuard { iface: iface.clone(), child, mode: impl_name.clone() };
    let r: Result<()> = async {
        let mut cmd = tokio::process::Command::new(&wg);
        cmd.arg("setconf").arg(&iface).arg(&cf);
        crate::util::run(&mut cmd).await.context("wg setconf")?;
        // Адрес интерфейса — с хостовой частью (10.66.0.2/24), не адрес сети.
        net::ip(&["-4", "address", "add", &format!("{aip}/{apfx}"), "dev", &iface]).await?;
        let mtu = net::p_u64(p, "mtu", 1420).to_string();
        net::ip(&["link", "set", "mtu", &mtu, "up", "dev", &iface]).await?;
        let endpoint_host = net::p_str(p, "endpoint").rsplit_once(':').map(|x| x.0.trim_matches(|c| c == '[' || c == ']').to_string()).unwrap_or_default();
        let avoid = net::resolve_host(&endpoint_host).await.unwrap_or_default();
        net::add_routes(&iface, &allowed, &avoid).await?;
        Ok(())
    }
    .await;
    if let Err(e) = r { w.down().await; return Err(e); }
    tracing::info!("WireGuard «{}»: {iface} ({impl_name}) {address}, через туннель: {}", c.name, allowed.join(", "));
    Ok((w, json!({ "up": true, "iface": iface, "address": address, "routes": allowed, "implementation": impl_name, "error": null })))
}

impl WireGuard {
    pub fn alive(&mut self) -> bool {
        if let Some(ch) = &mut self.child { if !matches!(ch.try_wait(), Ok(None)) { return false; } }
        net::iface_exists(&self.iface)
    }

    /// Секунд с последнего рукопожатия (None — не было).
    pub async fn last_handshake(&self) -> Option<i64> {
        let wg = net::which("wg")?;
        let mut cmd = tokio::process::Command::new(wg);
        cmd.args(["show", &self.iface, "latest-handshakes"]);
        let out = crate::util::run(&mut cmd).await.ok()?;
        let t: i64 = out.split_whitespace().nth(1)?.parse().ok()?;
        if t == 0 { None } else { Some(chrono::Utc::now().timestamp() - t) }
    }

    pub async fn down(&mut self) {
        net::del_iface(&self.iface).await;
        if let Some(ch) = &mut self.child { let _ = ch.kill().await; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_conf() {
        let (sk, pk) = genkey();
        assert_eq!(pubkey(&sk).unwrap(), pk);
        let (_, peer) = genkey();
        let p = json!({ "private_key": sk, "peer_public_key": peer, "endpoint": "vpn.example:51820", "keepalive": 15 });
        let c = wg_conf(&p, &["10.66.0.1/32".into()]).unwrap();
        assert!(c.contains("AllowedIPs = 10.66.0.1/32") && c.contains("PersistentKeepalive = 15") && c.contains("Endpoint = vpn.example:51820"));
        assert!(wg_conf(&json!({ "private_key": "bad", "peer_public_key": peer, "endpoint": "x:1" }), &[]).is_err());
        assert!(check_endpoint("vpn.example:51820").is_ok() && check_endpoint("[2001:db8::1]:51820").is_ok());
        assert!(check_endpoint("vpn.example").is_err() && check_endpoint("x:1\nPostUp = id").is_err() && check_endpoint("x:70000").is_err());
    }
}
