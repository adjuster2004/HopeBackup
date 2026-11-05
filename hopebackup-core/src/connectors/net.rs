//! Сетевые операции VPN-коннекторов: интерфейсы, адреса, маршруты (через `ip` из iproute2).
//!
//! Правило всех VPN-коннекторов: через туннель идёт только трафик до узла площадки (или явно
//! перечисленных сетей). Маршрут по умолчанию, DNS и прочие настройки, которые присылает сервер VPN,
//! не принимаются — хост с продуктивной СУБД не должен менять маршрутизацию из-за резервного копирования.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

/// Имя интерфейса коннектора: `hb` + 8 знаков идентификатора (ограничение Linux — 15 символов).
pub fn iface_name(connector_id: &str) -> String {
    let id: String = connector_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("hb{}", id.to_lowercase())
}

/// Каталог рабочих файлов коннектора (конфигурации, ключи — 0600).
pub fn work_dir(connector_id: &str) -> Result<PathBuf> {
    let d = crate::agent::data_dir().join("vpn").join(iface_name(connector_id));
    std::fs::create_dir_all(&d)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

/// Общий для хоста каталог владельцев каналов: агент и сервер на одной машине не поднимают один и тот
/// же VPN дважды (сервер VPN выбил бы дублирующийся сертификат, у WireGuard «прыгал» бы endpoint).
fn run_dir() -> PathBuf {
    let d = std::env::var("HOPEBACKUP_RUN_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/run/hopebackup/vpn"));
    if std::fs::create_dir_all(&d).is_ok() { d } else { let d = crate::agent::data_dir().join("run"); let _ = std::fs::create_dir_all(&d); d }
}

fn pid_alive(pid: u32) -> bool { std::path::Path::new(&format!("/proc/{pid}")).exists() }

/// Канал уже поднят другим процессом HopeBackup на этом хосте: его pid.
pub fn foreign_owner(iface: &str) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(run_dir().join(format!("{iface}.pid"))).ok()?.trim().parse().ok()?;
    (pid != std::process::id() && pid_alive(pid)).then_some(pid)
}

pub fn owner_alive(pid: u32) -> bool { pid_alive(pid) }

pub fn claim(iface: &str) { let _ = std::fs::write(run_dir().join(format!("{iface}.pid")), std::process::id().to_string()); }

pub fn release(iface: &str) {
    let p = run_dir().join(format!("{iface}.pid"));
    if std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(std::process::id()) { let _ = std::fs::remove_file(p); }
}

/// Убрать каналы, владелец которых завершился (ExecStopPost служб, `hopebackup vpn-cleanup`).
pub async fn cleanup_dead() -> Vec<String> {
    let mut done = Vec::new();
    let Ok(rd) = std::fs::read_dir(run_dir()) else { return done };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(iface) = name.strip_suffix(".pid") else { continue };
        let pid: u32 = std::fs::read_to_string(e.path()).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        if pid != 0 && pid_alive(pid) { continue; }
        del_iface(iface).await;
        let _ = std::fs::remove_file(e.path());
        done.push(iface.to_string());
    }
    done
}

pub fn require_root(kind: &str) -> Result<()> {
    if !crate::util::is_root() { bail!("коннектор «{kind}» настраивает сетевой интерфейс — агент или сервер должен работать от root"); }
    Ok(())
}

pub fn iface_exists(name: &str) -> bool { std::path::Path::new("/sys/class/net").join(name).exists() }

/// Найти программу в PATH (и в типичных sbin, которых может не быть в PATH службы).
pub fn which(prog: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"] { dirs.push(PathBuf::from(d)); }
    dirs.into_iter().map(|d| d.join(prog)).find(|p| p.is_file())
}

/// Программа, нужная коннектору, и пакеты, которые её дают.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Req {
    pub what: &'static str,
    pub deb: Vec<&'static str>,
    pub rpm: Vec<&'static str>,
    pub present: bool,
}

/// Демон charon (charon-systemd в Debian/Ubuntu и RHEL; charon из strongswan-starter).
fn charon_present() -> bool {
    which("charon-systemd").is_some() || ["/usr/libexec/strongswan/charon-systemd", "/usr/lib/ipsec/charon", "/usr/libexec/ipsec/charon", "/usr/libexec/strongswan/charon"].iter().any(|p| std::path::Path::new(p).exists())
}

/// Что нужно на этой машине для коннектора вида `kind`.
pub fn requirements(kind: &str) -> Vec<Req> {
    let r = |what, deb: &[&'static str], rpm: &[&'static str], present| Req { what, deb: deb.to_vec(), rpm: rpm.to_vec(), present };
    match kind {
        "ipsec" | "l2tp_ipsec" => {
            let mut v = vec![
                r("swanctl (strongSwan)", &["strongswan-swanctl"], &["strongswan"], which("swanctl").is_some()),
                r("демон charon (strongSwan)", &["charon-systemd", "libcharon-extra-plugins", "libstrongswan-extra-plugins"], &["strongswan"], charon_present()),
            ];
            if kind == "l2tp_ipsec" {
                v.push(r("xl2tpd", &["xl2tpd"], &["xl2tpd"], which("xl2tpd").is_some()));
                v.push(r("pppd", &["ppp"], &["ppp"], which("pppd").is_some()));
            }
            v
        }
        "openvpn" => vec![r("openvpn", &["openvpn"], &["openvpn"], which("openvpn").is_some())],
        "wireguard" => vec![r("wg (wireguard-tools)", &["wireguard-tools"], &["wireguard-tools"], which("wg").is_some())],
        _ => vec![],
    }
}

/// Пакеты для недостающего и менеджер пакетов этой машины: (менеджер, пакеты, команда для ручной установки).
pub fn missing_packages(kind: &str) -> (Option<&'static str>, Vec<&'static str>, String) {
    let pm = ["apt-get", "dnf", "yum"].into_iter().find(|p| which(p).is_some());
    let mut pkgs: Vec<&'static str> = Vec::new();
    for q in requirements(kind).into_iter().filter(|q| !q.present) {
        for p in if pm == Some("apt-get") || pm.is_none() { q.deb } else { q.rpm } { if !pkgs.contains(&p) { pkgs.push(p); } }
    }
    let cmd = if pkgs.is_empty() { String::new() } else { format!("{} install -y {}", pm.unwrap_or("apt-get"), pkgs.join(" ")) };
    (pm, pkgs, cmd)
}

/// Состояние пакетов коннектора на этой машине — для панели.
pub fn requirements_report(kind: &str) -> serde_json::Value {
    let (pm, pkgs, cmd) = missing_packages(kind);
    serde_json::json!({ "host": crate::util::hostname(), "requirements": requirements(kind), "missing": pkgs, "command": cmd,
        "manager": pm, "root": crate::util::is_root(), "can_install": pm.is_some() && crate::util::is_root() })
}

/// Поставить недостающее для коннектора `kind` (по подтверждению администратора) и подготовить службы:
/// strongSwan — включить и запустить; системный xl2tpd — выключить (агент запускает свой экземпляр).
pub async fn install_for(kind: &str, log: &(dyn Fn(&str) + Send + Sync)) -> Result<serde_json::Value> {
    let (pm, pkgs, cmd) = missing_packages(kind);
    if pkgs.is_empty() {
        log("все нужные пакеты уже установлены");
    } else {
        log(&format!("установка: {cmd}"));
        let what = format!("пакеты для коннектора ({})", pkgs.join(", "));
        let deb: Vec<&str> = pkgs.clone();
        install_packages(&what, &deb, &deb).await?;
        log("пакеты установлены");
    }
    if kind == "ipsec" || kind == "l2tp_ipsec" {
        for args in [["enable", "--now", "strongswan"]] {
            let o = tokio::process::Command::new("systemctl").args(args).output().await;
            if matches!(&o, Ok(o) if o.status.success()) { log("служба strongswan включена и запущена"); }
        }
    }
    if kind == "l2tp_ipsec" {
        let _ = tokio::process::Command::new("systemctl").args(["disable", "--now", "xl2tpd"]).output().await;
        log("системная служба xl2tpd выключена — агент запускает свой экземпляр");
    }
    let mut still: Vec<String> = requirements(kind).into_iter().filter(|q| !q.present).map(|q| q.what.to_string()).collect();
    if !still.is_empty() && !pkgs.is_empty() {
        // Пакет числится установленным, а программы нет (удалили файл) — переустановить.
        if let Some(apt) = which("apt-get") {
            log("программы всё ещё нет — переустановка пакетов");
            let mut c = tokio::process::Command::new(apt);
            c.args(["install", "--reinstall", "-y"]).args(&pkgs).env("DEBIAN_FRONTEND", "noninteractive").stdin(std::process::Stdio::null());
            let _ = crate::util::run(&mut c).await;
            still = requirements(kind).into_iter().filter(|q| !q.present).map(|q| q.what.to_string()).collect();
        }
    }
    if !still.is_empty() { bail!("после установки всё ещё нет: {}", still.join(", ")); }
    Ok(serde_json::json!({ "installed": pkgs, "manager": pm, "host": crate::util::hostname() }))
}

/// Поставить пакеты дистрибутива (apt-get, dnf или yum) — только по явному подтверждению администратора.
pub async fn install_packages(what: &str, deb: &[&str], rpm: &[&str]) -> Result<()> {
    if !crate::util::is_root() { bail!("установить {what} может только root"); }
    let run = |prog: PathBuf, args: Vec<String>| async move {
        let mut c = tokio::process::Command::new(prog);
        c.args(&args).env("DEBIAN_FRONTEND", "noninteractive").stdin(std::process::Stdio::null());
        match tokio::time::timeout(std::time::Duration::from_secs(600), crate::util::run(&mut c)).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(anyhow!("установка не завершилась за 10 минут")),
        }
    };
    tracing::warn!("ставлю {what}: пакеты дистрибутива");
    if let Some(apt) = which("apt-get") {
        let mut args: Vec<String> = ["install", "-y", "--no-install-recommends", "-o", "Dpkg::Options::=--force-confold"].iter().map(|s| s.to_string()).collect();
        args.extend(deb.iter().map(|s| s.to_string()));
        if run(apt.clone(), args.clone()).await.is_ok() { return Ok(()); }
        // Списки пакетов могли устареть — обновить и повторить.
        run(apt.clone(), vec!["update".into()]).await.ok();
        return run(apt, args).await.with_context(|| format!("apt-get install {}", deb.join(" ")));
    }
    for pm in ["dnf", "yum"] {
        if let Some(b) = which(pm) {
            let mut args: Vec<String> = vec!["install".into(), "-y".into()];
            args.extend(rpm.iter().map(|s| s.to_string()));
            return run(b, args).await.with_context(|| format!("{pm} install {}", rpm.join(" ")));
        }
    }
    bail!("не найден менеджер пакетов (apt-get, dnf, yum) — поставьте {what} вручную")
}

pub fn need(prog: &str, package: &str) -> Result<PathBuf> {
    which(prog).ok_or_else(|| anyhow!("не найдена программа {prog} — установите пакет {package}"))
}

pub async fn ip(args: &[&str]) -> Result<String> {
    let bin = need("ip", "iproute2")?;
    let mut c = tokio::process::Command::new(bin);
    c.args(args);
    crate::util::run(&mut c).await.with_context(|| format!("ip {}", args.join(" ")))
}

/// IPv4-адрес интерфейса (первый), если есть.
pub async fn iface_ipv4(name: &str) -> Option<String> {
    let out = ip(&["-4", "-o", "addr", "show", "dev", name]).await.ok()?;
    out.split_whitespace().skip_while(|w| *w != "inet").nth(1).map(|s| s.split('/').next().unwrap_or(s).to_string())
}

/// Удалить интерфейс, если он остался от прошлого запуска.
pub async fn del_iface(name: &str) {
    if iface_exists(name) { let _ = ip(&["link", "del", "dev", name]).await; }
}

/// Сеть в нотации CIDR: проверка и нормализация (`10.0.0.5` → `10.0.0.5/32`).
pub fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8)> {
    let s = s.trim();
    let (a, p) = match s.split_once('/') { Some((a, p)) => (a, p.parse::<u8>().map_err(|_| anyhow!("неверная маска в «{s}»"))?), None => (s, 32) };
    let ip: Ipv4Addr = a.parse().map_err(|_| anyhow!("«{s}» — не IPv4-сеть"))?;
    if p > 32 { bail!("неверная маска в «{s}»"); }
    Ok((ip, p))
}

pub fn cidr_str(ip: Ipv4Addr, p: u8) -> String {
    let mask = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
    format!("{}/{p}", Ipv4Addr::from(u32::from(ip) & mask))
}

/// Маршруты через туннель: явно заданные `routes` или адрес узла площадки (/32).
/// Маршрут по умолчанию запрещён: он перехватил бы весь трафик хоста.
pub async fn tunnel_routes(params: &Value, node_url: &str) -> Result<Vec<String>> { routes_from(params, &["routes"], node_url).await }

/// Сети из первого непустого списка `keys`, иначе адрес узла (/32).
pub async fn routes_from(params: &Value, keys: &[&str], node_url: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let explicit: Vec<String> = keys.iter().map(|k| p_list(params, k)).find(|v| !v.is_empty()).unwrap_or_default();
    if explicit.is_empty() {
        if node_url.trim().is_empty() { bail!("укажите сети через туннель или выберите этот коннектор у площадки (маршрут строится до её узла)"); }
        for ip in resolve_url_host(node_url).await? { out.push(format!("{ip}/32")); }
    } else {
        for r in explicit {
            let (ip, p) = parse_cidr(&r)?;
            if p < 8 { bail!("маршрут {r} через туннель запрещён: слишком широкая сеть перехватит трафик хоста — укажите сеть площадки (/8 и уже)"); }
            for gw in default_gateways() {
                if in_cidr(gw, ip, p) { bail!("маршрут {r} накрывает шлюз по умолчанию этого хоста ({gw}) — хост потеряет связь; укажите сеть площадки точнее"); }
            }
            out.push(cidr_str(ip, p));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn in_cidr(a: Ipv4Addr, net: Ipv4Addr, p: u8) -> bool {
    let m = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
    (u32::from(a) & m) == (u32::from(net) & m)
}

/// Шлюзы маршрутов по умолчанию (`/proc/net/route`).
pub fn default_gateways() -> Vec<Ipv4Addr> {
    let Ok(s) = std::fs::read_to_string("/proc/net/route") else { return vec![] };
    s.lines().skip(1).filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" { return None; }
        let g = u32::from_str_radix(f[2], 16).ok()?;
        if g == 0 { return None; }
        Some(Ipv4Addr::from(g.swap_bytes()))
    }).collect()
}

/// Значение для файлов настроек (PPP, swanctl): без переводов строк и управляющих символов.
pub fn no_ctl(s: &str, what: &str) -> Result<()> {
    if s.chars().any(|c| c.is_control()) { bail!("{what}: недопустимые символы (перевод строки, управляющие)"); }
    Ok(())
}

/// Хост из URL узла (`https://10.8.0.1:8443` → 10.8.0.1); имя разрешается в адреса IPv4.
pub async fn resolve_url_host(url: &str) -> Result<Vec<Ipv4Addr>> {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let host = match hostport.rsplit_once(':') { Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h, _ => hostport };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() { bail!("в адресе узла «{url}» нет хоста"); }
    resolve_host(host).await
}

pub async fn resolve_host(host: &str) -> Result<Vec<Ipv4Addr>> {
    if let Ok(IpAddr::V4(v4)) = host.parse::<IpAddr>() { return Ok(vec![v4]); }
    let addrs = tokio::net::lookup_host((host, 0)).await.with_context(|| format!("не удалось разрешить имя {host}"))?;
    let v: Vec<Ipv4Addr> = addrs.filter_map(|a| match a.ip() { IpAddr::V4(v) => Some(v), _ => None }).collect();
    if v.is_empty() { bail!("у {host} нет адреса IPv4"); }
    Ok(v)
}

/// Направить сети в интерфейс туннеля (маршрут устройства — шлюз не нужен для L3-туннелей).
pub async fn add_routes(iface: &str, routes: &[String], avoid: &[Ipv4Addr]) -> Result<()> {
    for r in routes {
        let (net, p) = parse_cidr(r)?;
        // Сервер VPN должен остаться доступен по обычному маршруту, иначе туннель замкнётся сам на себя.
        if p == 32 && avoid.contains(&net) { continue; }
        ip(&["route", "replace", r, "dev", iface]).await?;
    }
    Ok(())
}

/// Маршруты, направленные в интерфейс туннеля этим процессом (без добавленных ядром), в виде CIDR.
pub async fn iface_routes(iface: &str) -> Vec<String> {
    let Ok(out) = tokio::process::Command::new("ip").args(["-4", "route", "show", "dev", iface]).output().await else { return vec![] };
    parse_iface_routes(&String::from_utf8_lossy(&out.stdout))
}

fn parse_iface_routes(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| !l.contains("proto kernel"))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|t| *t != "default")
        .filter_map(|t| { let c = if t.contains('/') { t.to_string() } else { format!("{t}/32") }; parse_cidr(&c).ok().map(|(ip, p)| cidr_str(ip, p)) })
        .collect()
}

/// Вернуть маршруты, которые пропали из интерфейса (PPP/OpenVPN переподключились сами и создали интерфейс
/// заново — ядро удаляет маршруты прежнего). Возвращает восстановленные.
pub async fn heal_routes(iface: &str, want: &[String]) -> Vec<String> {
    let have: std::collections::HashSet<String> = iface_routes(iface).await.into_iter().collect();
    let mut fixed = Vec::new();
    for r in want {
        if have.contains(r) { continue; }
        if ip(&["route", "replace", r, "dev", iface]).await.is_ok() { fixed.push(r.clone()); }
    }
    fixed
}

/// Строка для конфигурационных файлов в двойных кавычках (strongSwan, pppd).
pub fn quoted(s: &str) -> String { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }

pub fn p_str(v: &Value, k: &str) -> String { v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string() }
pub fn p_raw(v: &Value, k: &str) -> String { v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string() }
pub fn p_u64(v: &Value, k: &str, d: u64) -> u64 { v.get(k).and_then(|x| x.as_u64()).filter(|x| *x > 0).unwrap_or(d) }
pub fn p_bool(v: &Value, k: &str, d: bool) -> bool { v.get(k).and_then(|x| x.as_bool()).unwrap_or(d) }
pub fn p_list(v: &Value, k: &str) -> Vec<String> {
    match v.get(k) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) => s.split([',', '\n']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iface_routes_parsing() {
        let out = "10.77.2.0/24 scope link \n10.77.2.1 proto kernel scope link src 10.77.2.11 \n10.77.3.0/24 scope link \n10.9.9.9 scope link \n";
        assert_eq!(parse_iface_routes(out), vec!["10.77.2.0/24", "10.77.3.0/24", "10.9.9.9/32"]);
    }

    #[test]
    fn cidr() {
        assert_eq!(cidr_str(parse_cidr("10.1.2.3/24").unwrap().0, 24), "10.1.2.0/24");
        assert_eq!(parse_cidr("10.1.2.3").unwrap(), ("10.1.2.3".parse().unwrap(), 32));
        assert!(parse_cidr("10.1.2.3/40").is_err());
        assert!(parse_cidr("host").is_err());
        assert_eq!(iface_name("3f2a9c1d77aa"), "hb3f2a9c1d");
    }

    #[tokio::test]
    async fn routes_default_to_node() {
        let r = tunnel_routes(&serde_json::json!({}), "https://10.8.0.1:8443").await.unwrap();
        assert_eq!(r, vec!["10.8.0.1/32"]);
        let r = tunnel_routes(&serde_json::json!({ "routes": ["10.20.0.0/16", "10.20.1.5"] }), "https://x:1").await.unwrap();
        assert_eq!(r, vec!["10.20.0.0/16", "10.20.1.5/32"]);
        assert!(tunnel_routes(&serde_json::json!({ "routes": ["0.0.0.0/0"] }), "https://x:1").await.is_err());
        assert!(tunnel_routes(&serde_json::json!({ "routes": ["10.0.0.0/7"] }), "https://x:1").await.is_err());
        assert!(in_cidr("10.1.2.3".parse().unwrap(), "10.1.0.0".parse().unwrap(), 16) && !in_cidr("10.2.0.1".parse().unwrap(), "10.1.0.0".parse().unwrap(), 16));
        assert!(no_ctl("a b", "x").is_ok() && no_ctl("a\nb", "x").is_err() && no_ctl("a\rb", "x").is_err());
    }
}
