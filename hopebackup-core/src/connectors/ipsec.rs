//! IPsec через strongSwan (swanctl/vici): L2TP/IPsec (IKEv1, транспортный режим + xl2tpd/pppd) и
//! IPsec IKEv2 (туннельный режим, PSK или EAP-MSCHAPv2, виртуальный адрес по запросу).
//!
//! Конфигурация каждого коннектора — отдельный файл `/etc/swanctl/conf.d/hopebackup-<iface>.conf`
//! (0600), подключаемый стандартным `swanctl --load-all`: чужие соединения хоста не затрагиваются.
//! xl2tpd запускается собственным экземпляром агента (свои файлы конфигурации и управления).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::net::{self, p_bool, p_list, p_raw, p_str, quoted};
use crate::proto::ConnectorSpec;

const IKE1_DEFAULT: &str = "aes256-sha256-modp2048,aes256-sha1-modp2048,aes128-sha1-modp2048,aes256-sha1-modp1024,3des-sha1-modp1024";
const ESP1_DEFAULT: &str = "aes256-sha256,aes256-sha1,aes128-sha1,3des-sha1";
const IKE2_DEFAULT: &str = "aes256-sha256-modp2048,aes256gcm16-prfsha256-ecp256,aes128-sha256-modp2048";
const ESP2_DEFAULT: &str = "aes256-sha256,aes256gcm16,aes128-sha256";

fn conf_dir() -> PathBuf {
    std::env::var("HOPEBACKUP_SWANCTL_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/swanctl/conf.d"))
}

fn conn_name(c: &ConnectorSpec) -> String { net::iface_name(&c.id) }

fn conf_path(c: &ConnectorSpec) -> PathBuf { conf_dir().join(format!("hopebackup-{}.conf", conn_name(c))) }

fn props(p: &Value, k: &str, d: &str) -> String {
    let v = p_str(p, k);
    let v = if v.is_empty() { d.to_string() } else { v };
    // Только символы, допустимые в списках предложений strongSwan.
    v.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ',' || *c == '_').collect()
}

/// Значение для swanctl.conf без кавычек: адрес, идентификатор, селектор трафика.
fn plain(s: &str, what: &str) -> Result<String> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || ".:-_@/[],%=".contains(c)) { bail!("{what}: недопустимое значение «{s}»"); }
    Ok(s.to_string())
}

/// swanctl.conf для L2TP/IPsec: IKEv1, PSK, транспортный режим для UDP/1701.
pub fn l2tp_conf(c: &ConnectorSpec, server_ip: &str) -> Result<String> {
    let p = &c.params;
    let n = conn_name(c);
    let server = plain(&p_str(p, "server"), "сервер L2TP")?;
    let psk = p_raw(p, "psk");
    if psk.is_empty() { bail!("не задан общий ключ IPsec (PSK)"); }
    net::no_ctl(&psk, "общий ключ IPsec")?;
    Ok(format!(
        "# HopeBackup: коннектор «{name}» (L2TP/IPsec). Файл создаётся автоматически.\nconnections {{\n  {n} {{\n    version = 1\n    remote_addrs = {server}\n    proposals = {ike}\n    dpd_delay = 30s\n    local {{\n      auth = psk\n    }}\n    remote {{\n      auth = psk\n    }}\n    children {{\n      {n} {{\n        mode = transport\n        local_ts = dynamic[udp/1701]\n        remote_ts = dynamic[udp/1701]\n        esp_proposals = {esp}\n        dpd_action = restart\n        start_action = none\n      }}\n    }}\n  }}\n}}\nsecrets {{\n  ike-{n} {{\n    id-1 = {server_ip}\n    secret = {secret}\n  }}\n}}\n",
        name = c.name.replace('\n', " "),
        ike = props(p, "ike_proposals", IKE1_DEFAULT),
        esp = props(p, "esp_proposals", ESP1_DEFAULT),
        secret = quoted(&psk),
    ))
}

/// swanctl.conf для IKEv2: туннельный режим до сетей площадки.
pub fn ikev2_conf(c: &ConnectorSpec, remote_ts: &[String]) -> Result<(String, Vec<(String, String)>)> {
    let p = &c.params;
    let n = conn_name(c);
    let gw = plain(&p_str(p, "remote"), "шлюз IPsec")?;
    let mode = { let m = p_str(p, "auth"); if m.is_empty() { "psk".to_string() } else { m } };
    let local_id = p_str(p, "local_id");
    let remote_id = p_str(p, "remote_id");
    let vip = p_bool(p, "request_vip", mode == "eap");
    let mut local_ts = p_list(p, "local_ts");
    if local_ts.is_empty() { local_ts.push("dynamic".into()); }
    let mut files = Vec::new();
    let mut local = String::new();
    let mut remote = String::new();
    let mut secrets = String::new();
    match mode.as_str() {
        "psk" => {
            let psk = p_raw(p, "psk");
            if psk.is_empty() { bail!("не задан общий ключ (PSK)"); }
            net::no_ctl(&psk, "общий ключ")?;
            local += "      auth = psk\n";
            remote += "      auth = psk\n";
            if !local_id.is_empty() { local += &format!("      id = {}\n", plain(&local_id, "локальный идентификатор")?); }
            let mut ids = String::new();
            if !local_id.is_empty() { ids += &format!("    id-1 = {}\n", plain(&local_id, "локальный идентификатор")?); }
            if !remote_id.is_empty() { ids += &format!("    id-2 = {}\n", plain(&remote_id, "идентификатор шлюза")?); }
            // Без идентификаторов секрет подошёл бы любому соединению хоста — привязываем к шлюзу.
            if ids.is_empty() && gw.parse::<std::net::IpAddr>().is_ok() { ids += &format!("    id-1 = {gw}\n"); }
            secrets += &format!("  ike-{n} {{\n{ids}    secret = {}\n  }}\n", quoted(&psk));
        }
        "eap" => {
            let user = p_str(p, "username");
            let pass = p_raw(p, "password");
            if user.is_empty() || pass.is_empty() { bail!("для EAP укажите логин и пароль"); }
            net::no_ctl(&user, "логин")?;
            net::no_ctl(&pass, "пароль")?;
            local += "      auth = eap-mschapv2\n";
            local += &format!("      eap_id = {}\n", quoted(&user));
            remote += "      auth = pubkey\n";
            let ca = p_raw(p, "ca_cert");
            if !ca.trim().is_empty() {
                crate::tlsutil::pem_to_der(ca.as_bytes()).map_err(|e| anyhow!("сертификат CA шлюза: {e}"))?;
                let f = format!("hopebackup-{n}-ca.pem");
                remote += &format!("      cacerts = {f}\n");
                files.push((f, ca));
            }
            secrets += &format!("  eap-{n} {{\n    id = {}\n    secret = {}\n  }}\n", quoted(&user), quoted(&pass));
        }
        m => bail!("неизвестный способ аутентификации «{m}» (psk | eap)"),
    }
    if !remote_id.is_empty() { remote += &format!("      id = {}\n", plain(&remote_id, "идентификатор шлюза")?); } else { remote += &format!("      id = {}\n", if mode == "eap" { gw.clone() } else { "%any".into() }); }
    let lts: Result<Vec<String>> = local_ts.iter().map(|t| plain(t, "локальные сети")).collect();
    let rts: Result<Vec<String>> = remote_ts.iter().map(|t| plain(t, "сети площадки")).collect();
    let conf = format!(
        "# HopeBackup: коннектор «{name}» (IPsec IKEv2). Файл создаётся автоматически.\nconnections {{\n  {n} {{\n    version = 2\n    remote_addrs = {gw}\n{vips}    proposals = {ike}\n    dpd_delay = 30s\n    local {{\n{local}    }}\n    remote {{\n{remote}    }}\n    children {{\n      {n} {{\n        local_ts = {lts}\n        remote_ts = {rts}\n        esp_proposals = {esp}\n        dpd_action = restart\n        close_action = start\n        start_action = none\n      }}\n    }}\n  }}\n}}\nsecrets {{\n{secrets}}}\n",
        name = c.name.replace('\n', " "),
        vips = if vip { "    vips = 0.0.0.0\n" } else { "" },
        ike = props(p, "ike_proposals", IKE2_DEFAULT),
        esp = props(p, "esp_proposals", ESP2_DEFAULT),
        lts = lts?.join(","),
        rts = rts?.join(","),
    );
    Ok((conf, files))
}

async fn swanctl(args: &[&str]) -> Result<String> {
    let bin = net::need("swanctl", "strongswan-swanctl (и charon-systemd)")?;
    let mut c = tokio::process::Command::new(bin);
    c.args(args);
    // Сообщения о незагруженных необязательных модулях strongSwan — не ошибка, убираем из текста.
    crate::util::run(&mut c).await.map_err(|e| {
        let t: Vec<String> = format!("{e:#}")
            .lines()
            .map(|l| match l.find("plugin '") { Some(p) if l.contains("failed to load") => l[..p].trim_end().to_string(), _ => l.trim().to_string() })
            .filter(|l| !l.is_empty())
            .collect();
        anyhow!("{}", t.join(" "))
    })
}

/// Демон charon доступен по vici; если нет — запустить службу strongSwan, а если strongSwan не установлен
/// (или нет charon-systemd) — поставить пакеты дистрибутива и запустить.
async fn ensure_charon() -> Result<()> {
    if net::which("swanctl").is_some() && swanctl(&["--stats"]).await.is_ok() { return Ok(()); }
    if net::which("swanctl").is_some() && start_charon().await { return Ok(()); }
    let (_, pkgs, cmd) = net::missing_packages("ipsec");
    if !pkgs.is_empty() {
        bail!("на {} нет strongSwan ({}) — установите: «Коннекторы» → «Установить пакеты» у коннектора (или вручную: {cmd}; systemctl enable --now strongswan)", crate::util::hostname(), pkgs.join(", "));
    }
    bail!("strongSwan установлен, но демон charon не отвечает — проверьте: systemctl status strongswan (служба ipsec из strongswan-starter занимает те же порты: systemctl disable --now strongswan-starter)")
}

async fn start_charon() -> bool {
    for svc in ["strongswan", "strongswan-starter", "strongswan-swanctl"] {
        let _ = tokio::process::Command::new("systemctl").args(["start", svc]).output().await;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if swanctl(&["--stats"]).await.is_ok() { return true; }
        }
    }
    false
}

async fn load(c: &ConnectorSpec, conf: &str, files: &[(String, String)]) -> Result<()> {
    let dir = conf_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
    for (name, body) in files {
        let x509 = dir.parent().unwrap_or(&dir).join("x509ca").join(name);
        crate::util::write_secret(&x509, body.as_bytes())?;
    }
    crate::util::write_secret(&conf_path(c), conf.as_bytes())?;
    if let Err(e) = swanctl(&["--load-all", "--noprompt"]).await {
        // Код ошибки общий для всех файлов conf.d: проверяем, загрузилось ли именно наше соединение.
        let loaded = swanctl(&["--list-conns"]).await.map(|o| o.lines().any(|l| l.starts_with(&format!("{}:", conn_name(c))))).unwrap_or(false);
        if loaded {
            tracing::warn!("swanctl --load-all: {e:#} (соединение «{}» загружено)", c.name);
        } else {
            // Неверный файл не должен мешать другим соединениям хоста.
            let _ = std::fs::remove_file(conf_path(c));
            let _ = swanctl(&["--load-all", "--noprompt"]).await;
            bail!("strongSwan не принял настройки коннектора «{}»: {e:#}", c.name);
        }
    }
    Ok(())
}

async fn initiate(c: &ConnectorSpec) -> Result<()> {
    let n = conn_name(c);
    swanctl(&["--initiate", "--child", &n, "--timeout", "30"]).await.map_err(|e| {
        let hint = if c.kind == "ipsec" { " Если в Windows это подключение называется «L2TP/IPsec с общим ключом», выберите тип коннектора «L2TP / IPsec» — шлюз L2TP на IKEv2 не отвечает." } else { "" };
        anyhow!("IPsec «{}»: согласование не удалось: {e:#}.{hint}", c.name)
    })?;
    Ok(())
}

pub async fn sa_established(c: &ConnectorSpec) -> bool {
    swanctl(&["--list-sas", "--ike", &conn_name(c)]).await.map(|o| o.contains("ESTABLISHED")).unwrap_or(false)
}

fn ca_path(n: &str) -> PathBuf {
    let dir = conf_dir();
    dir.parent().unwrap_or(&dir).join("x509ca").join(format!("hopebackup-{n}-ca.pem"))
}

pub async fn ipsec_down(c: &ConnectorSpec) {
    let n = conn_name(c);
    let _ = swanctl(&["--terminate", "--ike", &n, "--timeout", "10"]).await;
    let _ = std::fs::remove_file(conf_path(c));
    let _ = std::fs::remove_file(ca_path(&n));
    let _ = swanctl(&["--load-all", "--noprompt"]).await;
}

/// IKEv2: поднять SA. Возвращает описание для панели.
pub async fn ikev2_up(c: &ConnectorSpec, routes: Vec<String>) -> Result<Value> {
    net::require_root("IPsec IKEv2")?;
    ensure_charon().await?;
    // «10.77.2.10/24» → «10.77.2.0/24»: адрес хоста с маской сети приводится к адресу сети.
    let mut remote_ts: Vec<String> = p_list(&c.params, "remote_ts").into_iter()
        .map(|t| net::parse_cidr(&t).map(|(ip, p)| net::cidr_str(ip, p)).unwrap_or(t)).collect();
    if remote_ts.is_empty() { remote_ts = routes; }
    let (conf, files) = ikev2_conf(c, &remote_ts)?;
    load(c, &conf, &files).await?;
    initiate(c).await?;
    tracing::info!("IPsec IKEv2 «{}»: SA установлена, сети площадки {}", c.name, remote_ts.join(", "));
    Ok(json!({ "up": true, "routes": remote_ts, "error": null }))
}

// ---------------------------------------------------------------- L2TP

/// Свой экземпляр xl2tpd (по одному на процесс агента/сервера; LAC — по одному на коннектор).
pub struct Xl2tpd {
    child: tokio::process::Child,
    control: PathBuf,
    lacs: Vec<(String, String)>,
}

pub type XlHandle = std::sync::Arc<tokio::sync::Mutex<Option<Xl2tpd>>>;

fn ppp_options(c: &ConnectorSpec, logfile: &std::path::Path) -> Result<String> {
    let p = &c.params;
    let user = p_str(p, "username");
    let pass = p_raw(p, "password");
    if user.is_empty() { bail!("не задан логин L2TP"); }
    net::no_ctl(&user, "логин L2TP")?;
    net::no_ctl(&pass, "пароль L2TP")?;
    let mtu = net::p_u64(p, "mtu", 1400);
    Ok(format!(
        "# HopeBackup: PPP для коннектора «{name}»\nipcp-accept-local\nipcp-accept-remote\nrefuse-eap\nnoccp\nnoauth\nnoipdefault\nnodefaultroute\nnoproxyarp\nmtu {mtu}\nmru {mtu}\nlcp-echo-interval 20\nlcp-echo-failure 4\nifname {iface}\nlogfile {log}\nname {u}\npassword {pw}\n",
        name = c.name.replace('\n', " "),
        iface = conn_name(c),
        log = logfile.display(),
        u = quoted(&user),
        pw = quoted(&pass),
    ))
}

fn xl2tpd_conf(lacs: &[(String, String)]) -> String {
    // Комментарии в xl2tpd.conf — только «;»: строку с «#» xl2tpd считает данными вне секции и не запускается.
    let mut s = String::from("; HopeBackup: собственный экземпляр xl2tpd\n[global]\nport = 1701\naccess control = no\n");
    for (name, body) in lacs { s += &format!("\n[lac {name}]\n{body}"); }
    s
}

async fn control(ctl: &PathBuf, cmd: &str) -> Result<()> {
    let ctl = ctl.clone();
    let line = format!("{cmd}\n");
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&ctl).with_context(|| format!("канал управления xl2tpd {}", ctl.display()))?;
        f.write_all(line.as_bytes())?;
        Ok(())
    })
    .await??;
    Ok(())
}

/// Запустить (или перезапустить с новым набором LAC) собственный xl2tpd.
async fn xl2tpd_with(h: &XlHandle, lac: (String, String)) -> Result<PathBuf> {
    let mut g = h.lock().await;
    if let Some(x) = g.as_mut() {
        let alive = matches!(x.child.try_wait(), Ok(None));
        if alive && x.lacs.iter().any(|l| *l == lac) { return Ok(x.control.clone()); }
    }
    if net::which("xl2tpd").is_none() || net::which("pppd").is_none() {
        let (_, pkgs, cmd) = net::missing_packages("l2tp_ipsec");
        bail!("на {} нет {} — установите: «Коннекторы» → «Установить пакеты» у коннектора (или вручную: {cmd}; systemctl disable --now xl2tpd)", crate::util::hostname(), pkgs.join(", "));
    }
    let bin = net::need("xl2tpd", "xl2tpd")?;
    net::need("pppd", "ppp")?;
    let mut lacs = g.as_ref().map(|x| x.lacs.clone()).unwrap_or_default();
    lacs.retain(|l| l.0 != lac.0);
    lacs.push(lac);
    if let Some(mut old) = g.take() { let _ = old.child.kill().await; }
    let dir = crate::agent::data_dir().join("vpn").join("xl2tpd");
    std::fs::create_dir_all(&dir)?;
    let conf = dir.join("xl2tpd.conf");
    let ctl = dir.join("l2tp-control");
    let _ = std::fs::remove_file(&ctl);
    crate::util::write_secret(&conf, xl2tpd_conf(&lacs).as_bytes())?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("-D").arg("-c").arg(&conf).arg("-p").arg(dir.join("xl2tpd.pid")).arg("-C").arg(&ctl);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("запуск xl2tpd")?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if ctl.exists() { break; }
        if let Ok(Some(_)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() { let _ = tokio::io::AsyncReadExt::read_to_string(&mut e, &mut err).await; }
            bail!("xl2tpd не запустился: {} (если работает системная служба — `systemctl disable --now xl2tpd`)", err.trim());
        }
    }
    if !ctl.exists() { let _ = child.kill().await; bail!("xl2tpd не создал канал управления"); }
    *g = Some(Xl2tpd { child, control: ctl.clone(), lacs });
    Ok(ctl)
}

/// L2TP/IPsec: IPsec (транспорт) → L2TP-туннель → PPP-интерфейс `hb…` → маршруты до площадки.
pub async fn l2tp_up(c: &ConnectorSpec, routes: Vec<String>, xl: &XlHandle) -> Result<Value> {
    net::require_root("L2TP/IPsec")?;
    let server = p_str(&c.params, "server");
    if server.is_empty() { bail!("коннектор «{}»: не указан сервер L2TP", c.name); }
    let server_ip = net::resolve_host(&server).await?[0].to_string();
    ensure_charon().await?;
    let conf = l2tp_conf(c, &server_ip)?;
    load(c, &conf, &[]).await?;
    initiate(c).await?;
    let dir = net::work_dir(&c.id)?;
    let opts = dir.join("options.l2tpd.client");
    let ppplog = dir.join("pppd.log");
    let _ = std::fs::write(&ppplog, b"");
    crate::util::write_secret(&opts, ppp_options(c, &ppplog)?.as_bytes())?;
    let n = conn_name(c);
    net::del_iface(&n).await;
    let lac = (n.clone(), format!("lns = {server_ip}\npppoptfile = {}\nlength bit = yes\nredial = yes\nredial timeout = 30\nmax redials = 1000\nrequire authentication = no\n", opts.display()));
    let ctl = xl2tpd_with(xl, lac).await?;
    control(&ctl, &format!("c {n}")).await?;
    let mut ip = None;
    for _ in 0..90 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if net::iface_exists(&n) { ip = net::iface_ipv4(&n).await; if ip.is_some() { break; } }
    }
    let Some(ip) = ip else {
        let _ = control(&ctl, &format!("d {n}")).await;
        // Закрыть и IPsec, чтобы шлюз освободил сеанс сразу, а не по тайм-ауту.
        tokio::time::sleep(Duration::from_secs(1)).await;
        ipsec_down(c).await;
        let why = ppp_reason(&std::fs::read_to_string(&ppplog).unwrap_or_default());
        bail!("L2TP «{}»: PPP-интерфейс не поднялся за 45 с — {}", c.name, why);
    };
    let avoid = vec![server_ip.parse().map_err(|_| anyhow!("адрес сервера"))?];
    net::add_routes(&n, &routes, &avoid).await?;
    tracing::info!("L2TP/IPsec «{}»: {n} {ip}, маршруты {}", c.name, routes.join(", "));
    Ok(json!({ "up": true, "iface": n, "address": ip, "routes": routes, "error": null }))
}

/// Причина отказа PPP по журналу pppd: понятное объяснение и последние значимые строки.
fn ppp_reason(log: &str) -> String {
    let keys = ["authentication", "auth ", "chap", "pap", "lcp terminated", "terminated", "refused", "failed", "timeout", "hangup", "no response", "peer", "rejected"];
    let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty())
        .filter(|l| { let x = l.to_lowercase(); keys.iter().any(|k| x.contains(k)) }).collect();
    let all = log.to_lowercase();
    let hint = if all.contains("authentication failed") || all.contains("auth failed") || all.contains("chap authentication failed") || all.contains("pap authentication failed") {
        "шлюз отклонил логин или пароль (проверьте пользователя и его группу VPN на шлюзе)"
    } else if all.contains("lcp terminated by peer") || all.contains("terminated by peer") {
        "шлюз сам закрыл сеанс сразу после соединения — часто лимит сеансов пользователя (прежний сеанс ещё не истёк) или запрет политики; сбросьте сессию на шлюзе или подождите несколько минут"
    } else if all.contains("no response to") || all.contains("timeout") {
        "шлюз не отвечает на PPP внутри L2TP (проверьте, что L2TP-сервер на шлюзе включён и разрешён для этого пользователя)"
    } else if log.trim().is_empty() {
        "pppd ничего не записал: соединение L2TP не дошло до PPP (журнал: journalctl -t xl2tpd -t pppd)"
    } else {
        "см. журнал pppd ниже"
    };
    let tail: Vec<&str> = lines.iter().rev().take(4).rev().copied().collect();
    if tail.is_empty() { hint.to_string() } else { format!("{hint}. pppd: {}", tail.join(" | ")) }
}

pub async fn l2tp_down(c: &ConnectorSpec, xl: &XlHandle) {
    let n = conn_name(c);
    if let Some(x) = xl.lock().await.as_ref() { let _ = control(&x.control, &format!("d {n}")).await; }
    // Дать xl2tpd завершить сеанс (StopCCN) — тогда шлюз закроет его сразу, а не по тайм-ауту.
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !net::iface_exists(&n) { break; }
    }
    net::del_iface(&n).await;
    ipsec_down(c).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(params: Value) -> ConnectorSpec { ConnectorSpec { id: "abcdef123456".into(), name: "ЦОД-3".into(), kind: "x".into(), params } }

    #[test]
    fn l2tp_config() {
        let c = spec(json!({ "server": "vpn.dc3.example", "psk": "se\"cret", "username": "backup", "password": "p w" }));
        let s = l2tp_conf(&c, "203.0.113.7").unwrap();
        assert!(s.contains("version = 1") && s.contains("mode = transport") && s.contains("dynamic[udp/1701]"));
        assert!(s.contains("id-1 = 203.0.113.7") && s.contains("secret = \"se\\\"cret\""));
        let o = ppp_options(&c, std::path::Path::new("/var/lib/hopebackup/vpn/x/pppd.log")).unwrap();
        assert!(o.contains("nodefaultroute") && o.contains("ifname hbabcdef12") && o.contains("password \"p w\"") && o.contains("logfile /var/lib/hopebackup/vpn/x/pppd.log"));
        let r = ppp_reason("rcvd [LCP ConfReq id=0x1]\nCHAP authentication failed: Access denied\nConnection terminated.\n");
        assert!(r.contains("логин или пароль") && r.contains("CHAP authentication failed"), "{r}");
        assert!(ppp_reason("LCP terminated by peer\n").contains("лимит сеансов"));
        assert!(ppp_reason("").contains("не дошло до PPP"));
        assert!(l2tp_conf(&spec(json!({ "server": "x;rm -rf", "psk": "a" })), "1.2.3.4").is_err());
        // xl2tpd не понимает «#»-комментарии: каждая строка — «;», «[секция]» или «ключ = значение».
        let x = xl2tpd_conf(&[("hbabcdef12".into(), "lns = 203.0.113.7\n".into())]);
        assert!(x.lines().all(|l| l.is_empty() || l.starts_with(';') || l.starts_with('[') || l.contains(" = ")), "{x}");
    }

    #[test]
    fn ikev2_config() {
        let c = spec(json!({ "remote": "198.51.100.1", "auth": "psk", "psk": "k", "local_id": "203.0.113.5", "local_ts": ["10.1.0.0/24"] }));
        let (s, _) = ikev2_conf(&c, &["10.20.0.0/16".into()]).unwrap();
        assert!(s.contains("version = 2") && s.contains("remote_ts = 10.20.0.0/16") && s.contains("local_ts = 10.1.0.0/24") && !s.contains("vips"));
        let c = spec(json!({ "remote": "vpn.example", "auth": "eap", "username": "u", "password": "p", "ca_cert": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n" }));
        let (s, files) = ikev2_conf(&c, &["10.8.0.1/32".into()]).unwrap();
        assert!(s.contains("vips = 0.0.0.0") && s.contains("auth = eap-mschapv2") && s.contains("cacerts =") && files.len() == 1);
    }
}

/// Снять SA и конфигурации коннекторов, чьи владельцы завершились.
pub async fn cleanup_dead(ifaces: &[String]) {
    let mut changed = false;
    for n in ifaces {
        let f = conf_dir().join(format!("hopebackup-{n}.conf"));
        if f.exists() {
            let _ = swanctl(&["--terminate", "--ike", n, "--timeout", "10"]).await;
            let _ = std::fs::remove_file(f);
            let _ = std::fs::remove_file(ca_path(n));
            changed = true;
        }
    }
    if changed { let _ = swanctl(&["--load-all", "--noprompt"]).await; }
}
