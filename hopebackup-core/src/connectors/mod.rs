//! Коннекторы к площадке. Коннектор поднимает канал и отдаёт адрес узла хранения; протокол обмена
//! с узлом от коннектора не зависит, поэтому коннекторы можно менять без пересоздания копий.
//!
//! Виды: `direct` (HTTPS напрямую), `ssh_tunnel` (`ssh -L`), `openvpn`, `wireguard`, `l2tp_ipsec`,
//! `ipsec` (IKEv2). VPN-коннекторы направляют в туннель только трафик до площадки.

pub mod ipsec;
pub mod net;
pub mod openvpn;
pub mod ssh;
pub mod wireguard;

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::proto::{ConnectorSpec, SiteAccess};
use crate::store::Remote;

/// Поля, которые хранятся как секреты (маскируются в панели).
pub const SECRET_KEYS: &[&str] = &["password", "private_key", "profile", "psk", "preshared_key", "key_password", "secret_key", "token_secret", "admin_secret_key", "sa_key", "oauth_token", "iam_token", "guest_password"];

pub const AVAILABLE: &[&str] = &["direct", "ssh_tunnel", "openvpn", "wireguard", "l2tp_ipsec", "ipsec"];

fn routes_prop() -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "title": "Сети через туннель", "description": "CIDR, по одной на строку. Пусто — только адрес узла площадки (/32). Маршрут по умолчанию запрещён." })
}

fn node_url_prop() -> Value {
    json!({ "type": "string", "title": "Адрес узла через этот коннектор", "description": "Если внутри VPN узел виден по другому адресу: https://10.8.0.1:8443. Пусто — адрес площадки." })
}

pub fn kinds() -> Vec<Value> {
    let k = |id: &str, title: &str, desc: &str| json!({ "id": id, "title": title, "description": desc, "available": false });
    vec![
        json!({ "id": "ssh_tunnel", "title": "SSH-туннель (-L)", "description": "Локальный порт пробрасывается на узел хранения через SSH; переподключение, jump-хост", "available": true,
            "schema": { "type": "object", "properties": {
                "host": { "type": "string", "title": "SSH-сервер площадки" },
                "port": { "type": "integer", "title": "Порт SSH", "default": 22 },
                "user": { "type": "string", "title": "Пользователь", "default": "hopebackup" },
                "private_key": { "type": "string", "format": "secret-textarea", "title": "Закрытый ключ (OpenSSH)" },
                "remote_host": { "type": "string", "title": "Узел хранения за SSH-сервером", "default": "127.0.0.1" },
                "remote_port": { "type": "integer", "title": "Порт узла хранения", "default": 8443 },
                "jump": { "type": "string", "title": "Jump-хост (необязательно)", "description": "user@host:port" } } } }),
        json!({ "id": "direct", "title": "Напрямую (TLS)", "description": "Узел хранения доступен по сети: HTTPS с закреплённым сертификатом", "available": true,
            "schema": { "type": "object", "properties": { "node_url": node_url_prop() } } }),
        json!({ "id": "openvpn", "title": "OpenVPN", "description": "Профиль .ovpn со встроенными сертификатами, логин/пароль; маршрут только до площадки", "available": true,
            "schema": { "type": "object", "required": ["profile"], "properties": {
                "profile": { "type": "string", "format": "secret-textarea", "title": "Профиль .ovpn", "description": "Целиком, сертификаты и ключи — внутри (<ca>…</ca>, <cert>, <key>, <tls-crypt>). Скрипты и маршруты из профиля игнорируются." },
                "username": { "type": "string", "title": "Логин (auth-user-pass)" },
                "password": { "type": "string", "format": "secret", "title": "Пароль" },
                "key_password": { "type": "string", "format": "secret", "title": "Пароль закрытого ключа", "description": "Если ключ в профиле зашифрован" },
                "routes": routes_prop(),
                "node_url": node_url_prop(),
                "timeout": { "type": "integer", "title": "Ожидание подключения, с", "default": 60 } } } }),
        json!({ "id": "wireguard", "title": "WireGuard", "description": "Малые накладные расходы, устойчив к нестабильным каналам; ядро или userspace", "available": true,
            "schema": { "type": "object", "required": ["private_key", "address", "peer_public_key", "endpoint"], "properties": {
                "private_key": { "type": "string", "format": "secret", "title": "Закрытый ключ этого хоста", "description": "base64; кнопка «Создать ключи» сгенерирует пару и покажет открытый ключ для сервера площадки" },
                "address": { "type": "string", "title": "Адрес интерфейса", "description": "Например, 10.66.0.2/32" },
                "peer_public_key": { "type": "string", "title": "Открытый ключ сервера площадки" },
                "preshared_key": { "type": "string", "format": "secret", "title": "Общий ключ (PresharedKey)", "description": "Необязательно" },
                "endpoint": { "type": "string", "title": "Сервер площадки (Endpoint)", "description": "хост:порт, например vpn.dc2.example:51820" },
                "allowed_ips": { "type": "array", "items": { "type": "string" }, "title": "AllowedIPs", "description": "Сети площадки. Пусто — адрес узла (/32)" },
                "keepalive": { "type": "integer", "title": "PersistentKeepalive, с", "default": 25 },
                "mtu": { "type": "integer", "title": "MTU", "default": 1420 },
                "implementation": { "type": "string", "title": "Реализация", "enum": ["auto", "kernel", "wireguard-go", "boringtun"], "default": "auto", "description": "auto — модуль ядра, а если его нет — wireguard-go или boringtun" },
                "node_url": node_url_prop() } } }),
        json!({ "id": "l2tp_ipsec", "title": "L2TP / IPsec", "description": "strongSwan (IKEv1, PSK) + xl2tpd/pppd; MikroTik, Windows RRAS, Cisco, UserGate. В Windows — «L2TP/IPsec с общим ключом»", "available": true,
            "schema": { "type": "object", "required": ["server", "psk", "username", "password"], "properties": {
                "server": { "type": "string", "title": "Сервер L2TP" },
                "psk": { "type": "string", "format": "secret", "title": "Общий ключ IPsec (PSK)" },
                "username": { "type": "string", "title": "Логин" },
                "password": { "type": "string", "format": "secret", "title": "Пароль" },
                "routes": routes_prop(),
                "mtu": { "type": "integer", "title": "MTU PPP", "default": 1400 },
                "ike_proposals": { "type": "string", "title": "Алгоритмы IKE", "description": "По умолчанию — совместимые с MikroTik и Windows" },
                "esp_proposals": { "type": "string", "title": "Алгоритмы ESP" },
                "reconnect_delay": { "type": "integer", "title": "Пауза перед повторным подключением, с", "default": 60, "description": "Сколько ждать после разрыва или неудачи, чтобы шлюз успел закрыть прежний сеанс (иначе при ограничении «один сеанс на пользователя» он отклонит новый)" },
                "node_url": node_url_prop() } } }),
        json!({ "id": "ipsec", "title": "IPsec IKEv2", "description": "strongSwan: site-to-site (PSK) или удалённый доступ (EAP); Yandex Cloud, AWS Site-to-Site VPN. В Windows — тип «IKEv2»", "available": true,
            "schema": { "type": "object", "required": ["remote"], "properties": {
                "remote": { "type": "string", "title": "Шлюз IPsec площадки" },
                "auth": { "type": "string", "title": "Аутентификация", "enum": ["psk", "eap"], "default": "psk", "description": "psk — общий ключ; eap — логин/пароль (EAP-MSCHAPv2), шлюз — по сертификату" },
                "psk": { "type": "string", "format": "secret", "title": "Общий ключ (PSK)" },
                "username": { "type": "string", "title": "Логин (EAP)" },
                "password": { "type": "string", "format": "secret", "title": "Пароль (EAP)" },
                "ca_cert": { "type": "string", "format": "textarea", "title": "Сертификат CA шлюза (PEM, для EAP)" },
                "local_id": { "type": "string", "title": "Свой идентификатор", "description": "Обычно внешний IP этого хоста" },
                "remote_id": { "type": "string", "title": "Идентификатор шлюза" },
                "local_ts": { "type": "array", "items": { "type": "string" }, "title": "Свои сети (local_ts)", "description": "Пусто — адрес этого хоста" },
                "remote_ts": { "type": "array", "items": { "type": "string" }, "title": "Сети площадки (remote_ts)", "description": "Пусто — адрес узла (/32)" },
                "request_vip": { "type": "boolean", "title": "Запросить виртуальный адрес", "description": "Для удалённого доступа (EAP) — включено" },
                "ike_proposals": { "type": "string", "title": "Алгоритмы IKE" },
                "esp_proposals": { "type": "string", "title": "Алгоритмы ESP" },
                "reconnect_delay": { "type": "integer", "title": "Пауза перед повторным подключением, с", "default": 60, "description": "Сколько ждать после разрыва или неудачи, чтобы шлюз успел закрыть прежний сеанс (иначе при ограничении «один сеанс на пользователя» он отклонит новый)" },
                "node_url": node_url_prop() } } }),
        k("ssh_reverse", "SSH обратный (-R)", "Источник за NAT: туннель открывает площадка"),
        k("sftp", "SFTP", "На площадке только сервер без нашего ПО"),
        k("offline", "Офлайн-носитель", "Первая полная копия: выгрузка на диск"),
    ]
}

enum Live {
    Ssh(ssh::Ssh),
    OpenVpn(openvpn::OpenVpn),
    WireGuard(wireguard::WireGuard),
    Ikev2,
    L2tp,
    /// Канал поднят другим процессом HopeBackup на этом хосте (агент и сервер на одной машине).
    Borrowed { iface: String, owner: u32 },
}

struct Tunnel {
    sig: String,
    spec: ConnectorSpec,
    live: Live,
    /// Канал создан этим процессом (иначе — общий: IPsec SA уже была, интерфейс чужой).
    owned: bool,
    /// Интерфейс и маршруты, направленные в него при подключении: если канал переподключился сам
    /// (повторный дозвон PPP, перезапуск OpenVPN) и маршруты пропали, они возвращаются при следующем использовании.
    iface: Option<String>,
    routes: Vec<String>,
    /// Когда канал последний раз проверен живым: параллельные задания (репликация площадки, продукта, отправка
    /// образов) берут его без повторной проверки в течение `FRESH`.
    checked: std::time::Instant,
}

/// Сколько считать проверку канала свежей.
const FRESH: Duration = Duration::from_secs(15);

/// Сколько ждать, пока канал переподключается сам (xl2tpd дозванивается заново, OpenVPN переподключается),
/// прежде чем рвать его и подключаться заново.
const GRACE: Duration = Duration::from_secs(40);

/// Состояние подключений коннектора: когда канал опущен, сколько неудач подряд, когда можно пробовать снова.
#[derive(Default, Clone)]
struct Health {
    down_at: Option<std::time::Instant>,
    fails: u32,
    next_try: Option<std::time::Instant>,
    last_err: String,
    dead_since: Option<std::time::Instant>,
}

/// Пауза после неудачи: 30 с, 1, 2, 4, 8 мин, дальше 10 мин.
fn backoff(fails: u32) -> Duration { Duration::from_secs((30u64 << fails.saturating_sub(1).min(5)).min(600)) }

/// Пауза после разрыва перед новым подключением: шлюз должен успеть закрыть прежний сеанс.
fn min_gap(c: &ConnectorSpec) -> Duration {
    match c.kind.as_str() {
        "l2tp_ipsec" | "ipsec" => Duration::from_secs(net::p_u64(&c.params, "reconnect_delay", 60).min(900)),
        "openvpn" => Duration::from_secs(10),
        _ => Duration::ZERO,
    }
}

impl Health {
    fn not_before(&self, gap: Duration) -> Option<std::time::Instant> {
        let a = self.down_at.map(|t| t + gap);
        match (a, self.next_try) { (Some(x), Some(y)) => Some(x.max(y)), (x, y) => x.or(y) }
    }
}

type WaitLog<'a> = Option<&'a (dyn Fn(&str) + Send + Sync)>;

type Slot = Arc<tokio::sync::Mutex<Option<Tunnel>>>;

/// Живые каналы этого процесса (агента, сервера или исполнителя восстановления), по одному на коннектор.
#[derive(Clone, Default)]
pub struct TunnelManager {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    status: Arc<Mutex<HashMap<String, Value>>>,
    xl2tpd: ipsec::XlHandle,
    health: Arc<Mutex<HashMap<String, Health>>>,
    /// Каналы, которые нужно держать поднятыми (были подняты и не опущены явно) — их обходит сторож.
    wanted: Arc<Mutex<HashMap<String, (ConnectorSpec, String)>>>,
}

/// Адрес узла для коннектора: свой `node_url` или адрес площадки.
fn node_url(c: &ConnectorSpec, site_url: &str) -> String {
    let u = net::p_str(&c.params, "node_url");
    if u.is_empty() { site_url.to_string() } else { u.trim_end_matches('/').to_string() }
}

impl TunnelManager {
    pub fn status(&self) -> HashMap<String, Value> { self.status.lock().clone() }

    fn set_status(&self, id: &str, v: Value) {
        let mut v = v;
        if let Some(o) = v.as_object_mut() { o.insert("time".into(), json!(chrono::Utc::now())); }
        self.status.lock().insert(id.to_string(), v);
    }

    /// Канал переподключался сам (повторный дозвон, перезапуск) и маршруты в него пропали — вернуть.
    async fn heal(&self, c: &ConnectorSpec, t: &Tunnel) {
        if let (Some(iface), true) = (t.iface.clone(), t.owned && !t.routes.is_empty()) {
            let fixed = net::heal_routes(&iface, &t.routes).await;
            if !fixed.is_empty() {
                tracing::warn!("коннектор «{}»: канал переподключался, маршруты пропали — восстановлены: {}", c.name, fixed.join(", "));
                self.set_status(&c.id, json!({ "up": true, "iface": iface, "routes": t.routes, "healed": fixed, "error": null }));
            }
        }
    }

    fn slot(&self, id: &str) -> Slot { self.slots.lock().entry(id.to_string()).or_default().clone() }

    async fn alive(&self, t: &mut Tunnel) -> bool {
        match &mut t.live {
            Live::Ssh(s) => s.alive().await,
            Live::OpenVpn(o) => o.alive() && o.up_now(),
            Live::WireGuard(w) => w.alive(),
            Live::Ikev2 => ipsec::sa_established(&t.spec).await,
            Live::L2tp => net::iface_exists(&net::iface_name(&t.spec.id)) && ipsec::sa_established(&t.spec).await,
            Live::Borrowed { iface, owner } => net::owner_alive(*owner) && net::iface_exists(iface),
        }
    }

    async fn down_tunnel(&self, t: &mut Tunnel) {
        if !t.owned { return; }
        {
            let mut hs = self.health.lock();
            let h = hs.entry(t.spec.id.clone()).or_default();
            h.down_at = Some(std::time::Instant::now());
            h.dead_since = None;
        }
        net::release(&net::iface_name(&t.spec.id));
        match &mut t.live {
            Live::Ssh(s) => s.down().await,
            Live::OpenVpn(o) => o.down().await,
            Live::WireGuard(w) => w.down().await,
            Live::Ikev2 => ipsec::ipsec_down(&t.spec).await,
            Live::L2tp => ipsec::l2tp_down(&t.spec, &self.xl2tpd).await,
            Live::Borrowed { .. } => {}
        }
    }

    /// Поднять (или переиспользовать) канал коннектора. Для SSH возвращает локальный порт.
    /// Без ожидания: если канал переподключается или действует пауза перед повторным подключением — сразу ошибка.
    pub async fn ensure(&self, c: &ConnectorSpec, site_url: &str) -> Result<Option<u16>> { self.ensure_with(c, site_url, None).await }

    /// `wait` — подождать (канал переподключается сам; пауза, чтобы шлюз закрыл прежний сеанс) и написать об этом.
    pub async fn ensure_with(&self, c: &ConnectorSpec, site_url: &str, wait: WaitLog<'_>) -> Result<Option<u16>> {
        let nurl = node_url(c, site_url);
        let sig = crate::util::sha256_hex(&format!("{}|{}|{}", c.kind, nurl, serde_json::to_string(&c.params).unwrap_or_default()));
        let slot = self.slot(&c.id);
        let mut g = slot.lock().await;
        if let Some(t) = g.as_mut() {
            // Канал только что проверен другим заданием — взять как есть (слот заперт: проверки идут по очереди).
            // Маршруты (локальная проверка, без сети) — всё равно сверить: канал мог переподключиться сам.
            if t.sig == sig && t.checked.elapsed() < FRESH {
                self.heal(c, t).await;
                return Ok(match &t.live { Live::Ssh(s) => Some(s.port), _ => None });
            }
            if t.sig == sig {
                let mut ok = self.alive(t).await;
                // Одна неудачная проверка — ещё не повод рвать канал, которым, возможно, прямо сейчас передают
                // данные другие задания: перепроверить через 3 с.
                if !ok {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    ok = self.alive(t).await;
                    if ok { tracing::info!("коннектор «{}»: канал ответил со второй проверки — используется дальше", c.name); }
                }
                // Канал переподключается сам — не рвать его (новый сеанс шлюз может отклонить, пока жив прежний).
                if !ok && t.owned && matches!(t.live, Live::L2tp | Live::OpenVpn(_)) {
                    let since = *self.health.lock().entry(c.id.clone()).or_default().dead_since.get_or_insert_with(std::time::Instant::now);
                    if let Some(l) = wait {
                        let left = GRACE.saturating_sub(since.elapsed());
                        if !left.is_zero() {
                            l(&format!("коннектор «{}»: канал переподключается сам — жду до {} с", c.name, left.as_secs()));
                            let until = std::time::Instant::now() + left;
                            while std::time::Instant::now() < until {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                if self.alive(t).await { ok = true; break; }
                            }
                        }
                    } else if since.elapsed() < GRACE {
                        bail!("коннектор «{}»: канал переподключается — повторите через минуту", c.name);
                    }
                }
                if ok {
                    t.checked = std::time::Instant::now();
                    if let Some(h) = self.health.lock().get_mut(&c.id) { h.dead_since = None; }
                    self.heal(c, t).await;
                    return Ok(match &t.live { Live::Ssh(s) => Some(s.port), _ => None });
                }
            }
            self.down_tunnel(t).await;
            *g = None;
        }
        // Пауза перед новым подключением: после разрыва (шлюз закрывает прежний сеанс) и после неудач (растёт).
        let h = self.health.lock().get(&c.id).cloned().unwrap_or_default();
        if let Some(until) = h.not_before(min_gap(c)) {
            let now = std::time::Instant::now();
            if until > now {
                let secs = (until - now).as_secs() + 1;
                let why = if h.fails > 0 { format!("неудачных попыток подряд: {}; последняя ошибка: {}", h.fails, h.last_err) } else { "шлюз закрывает прежний сеанс".to_string() };
                match wait {
                    Some(l) if secs <= 180 => {
                        l(&format!("коннектор «{}»: пауза {secs} с перед подключением ({why})", c.name));
                        tokio::time::sleep(until - now).await;
                    }
                    _ => {
                        let msg = format!("следующая попытка подключения — не раньше чем через {secs} с ({why})");
                        if !self.status.lock().get(&c.id).map(|v| v["up"] == true).unwrap_or(false) { self.set_status(&c.id, json!({ "up": false, "error": msg.clone() })); }
                        bail!("коннектор «{}»: {msg}", c.name);
                    }
                }
            }
        }
        // Тот же канал уже поднят другим процессом HopeBackup на этом хосте — пользуемся им.
        let iface = net::iface_name(&c.id);
        let shared: Option<Live> = match c.kind.as_str() {
            "openvpn" | "wireguard" | "l2tp_ipsec" => net::foreign_owner(&iface).filter(|_| net::iface_exists(&iface)).map(|owner| Live::Borrowed { iface: iface.clone(), owner }),
            "ipsec" if ipsec::sa_established(c).await && net::foreign_owner(&iface).is_some() => Some(Live::Ikev2),
            _ => None,
        };
        if let Some(live) = shared {
            tracing::info!("коннектор «{}»: канал уже поднят другим процессом HopeBackup на этом хосте", c.name);
            self.set_status(&c.id, json!({ "up": true, "iface": iface, "shared": true, "error": null }));
            *g = Some(Tunnel { sig, spec: c.clone(), live, owned: false, iface: None, routes: vec![], checked: std::time::Instant::now() });
            return Ok(None);
        }
        let r: Result<(Live, Value)> = async {
            match c.kind.as_str() {
                "ssh_tunnel" => ssh::up(c).await.map(|(s, v)| (Live::Ssh(s), v)),
                "openvpn" => {
                    let routes = net::tunnel_routes(&c.params, &nurl).await?;
                    let (me, id) = (self.clone(), c.id.clone());
                    let st: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |v| me.set_status(&id, v));
                    let o = openvpn::up(c, routes.clone(), st).await?;
                    let ip = net::iface_ipv4(&o.iface).await;
                    let v = json!({ "up": true, "iface": o.iface, "address": ip, "routes": routes, "error": null });
                    Ok((Live::OpenVpn(o), v))
                }
                "wireguard" => {
                    let routes = net::routes_from(&c.params, &["allowed_ips"], &nurl).await?;
                    wireguard::up(c, routes).await.map(|(w, v)| (Live::WireGuard(w), v))
                }
                "ipsec" => {
                    let routes = net::routes_from(&c.params, &["remote_ts"], &nurl).await?;
                    ipsec::ikev2_up(c, routes).await.map(|v| (Live::Ikev2, v))
                }
                "l2tp_ipsec" => {
                    let routes = net::tunnel_routes(&c.params, &nurl).await?;
                    ipsec::l2tp_up(c, routes, &self.xl2tpd).await.map(|v| (Live::L2tp, v))
                }
                k => Err(anyhow!("тип коннектора «{k}» пока не поддерживается")),
            }
        }
        .await;
        match r {
            Ok((live, v)) => {
                let port = match &live { Live::Ssh(s) => Some(s.port), _ => None };
                if c.kind != "ssh_tunnel" { net::claim(&iface); }
                // Что фактически направлено в интерфейс — то и поддерживается (без маршрута до самого сервера VPN).
                let (tif, troutes) = match (&live, v["iface"].as_str()) {
                    (Live::L2tp | Live::OpenVpn(_) | Live::WireGuard(_), Some(i)) => (Some(i.to_string()), net::iface_routes(i).await),
                    _ => (None, vec![]),
                };
                self.set_status(&c.id, v);
                *g = Some(Tunnel { sig, spec: c.clone(), live, owned: true, iface: tif, routes: troutes, checked: std::time::Instant::now() });
                self.health.lock().remove(&c.id);
                if c.kind != "ssh_tunnel" { self.wanted.lock().insert(c.id.clone(), (c.clone(), site_url.to_string())); }
                Ok(port)
            }
            Err(e) => {
                let fails = {
                    let mut hs = self.health.lock();
                    let h = hs.entry(c.id.clone()).or_default();
                    h.fails += 1;
                    h.next_try = Some(std::time::Instant::now() + backoff(h.fails));
                    // Неудачная попытка тоже оставляет на шлюзе полуоткрытый сеанс.
                    h.down_at = Some(std::time::Instant::now());
                    h.last_err = format!("{e:#}").chars().take(300).collect();
                    h.fails
                };
                let e = e.context(format!("попытка {fails}; следующая — не раньше чем через {} с", backoff(fails).as_secs().max(min_gap(c).as_secs())));
                self.set_status(&c.id, json!({ "up": false, "error": format!("{e:#}") }));
                Err(e)
            }
        }
    }

    /// Совместимость: поднять SSH-туннель и вернуть локальный порт.
    pub async fn ensure_ssh(&self, c: &ConnectorSpec) -> Result<u16> {
        self.ensure_with(c, "", Some(&|_: &str| {})).await?.ok_or_else(|| anyhow!("не SSH-коннектор"))
    }

    /// Сторож (сервер управления): раз в 30 с проверяет поднятые каналы — возвращает пропавшие маршруты, а упавший
    /// канал поднимает заново сам, с паузой, чтобы шлюз успел закрыть прежний сеанс. Канал, занятый заданием, пропускается.
    pub fn spawn_watchdog(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                me.watch_once().await;
            }
        });
    }

    async fn watch_once(&self) {
        let wanted: Vec<(String, (ConnectorSpec, String))> = self.wanted.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (id, (spec, url)) in wanted {
            let slot = self.slot(&id);
            {
                let Ok(mut g) = slot.try_lock() else { continue };
                if let Some(t) = g.as_mut() {
                    if !t.owned { continue; }
                    if self.alive(t).await {
                        if let Some(h) = self.health.lock().get_mut(&id) { h.dead_since = None; }
                        if let Some(iface) = t.iface.clone() {
                            let fixed = net::heal_routes(&iface, &t.routes).await;
                            if !fixed.is_empty() {
                                tracing::warn!("коннектор «{}»: канал переподключался, маршруты пропали — восстановлены: {}", spec.name, fixed.join(", "));
                                self.set_status(&id, json!({ "up": true, "iface": iface, "routes": t.routes, "healed": fixed, "error": null }));
                            }
                        }
                        continue;
                    }
                }
            }
            // Канала нет или он упал: поднять (ensure сам подождёт, пока канал переподключается, и выдержит паузу).
            match self.ensure(&spec, &url).await {
                Ok(_) => tracing::info!("коннектор «{}»: канал поддерживается сторожем", spec.name),
                Err(e) => tracing::debug!("сторож каналов: {e:#}"),
            }
        }
    }

    /// Опустить канал коннектора (например, после изменения настроек или удаления).
    pub async fn down(&self, id: &str) {
        self.wanted.lock().remove(id);
        let slot = self.slot(id);
        let mut g = slot.lock().await;
        if let Some(t) = g.as_mut() { self.down_tunnel(t).await; }
        *g = None;
        self.status.lock().remove(id);
    }

    /// Опустить каналы коннекторов, которых больше нет на сервере.
    pub async fn retain(&self, keep: &[String]) {
        let ids: Vec<String> = self.slots.lock().keys().filter(|k| !keep.contains(k) && k.as_str() != "direct").cloned().collect();
        for id in ids {
            tracing::info!("коннектор {id} удалён на сервере — канал опущен");
            self.down(&id).await;
            self.slots.lock().remove(&id);
        }
    }

    /// Опустить все каналы (завершение процесса).
    pub async fn down_all(&self) {
        let ids: Vec<String> = self.slots.lock().keys().cloned().collect();
        for id in ids { self.down(&id).await; }
    }

    /// Адрес узла площадки через первый работающий коннектор (по приоритету). Без коннекторов — напрямую.
    pub async fn connect(&self, site: &SiteAccess, log: &(dyn Fn(&str) + Send + Sync)) -> Result<Remote> {
        let mut errors = Vec::new();
        let list: Vec<ConnectorSpec> = if site.connectors.is_empty() {
            vec![ConnectorSpec { id: "direct".into(), name: "напрямую".into(), kind: "direct".into(), params: Value::Null }]
        } else {
            site.connectors.clone()
        };
        for c in &list {
            let mut remote = site.remote.clone();
            let r: Result<()> = async {
                if remote.is_s3() && c.kind == "ssh_tunnel" { bail!("для площадки S3 и облачного диска используйте прямой доступ или VPN-коннектор"); }
                match c.kind.as_str() {
                    "direct" => {}
                    _ => {
                        if c.kind != "ssh_tunnel" { log(&format!("коннектор «{}»: подключение…", c.name)); }
                        if let Some(port) = self.ensure_with(c, &site.remote.url, Some(log)).await? {
                            let scheme = if remote.url.starts_with("http://") { "http" } else { "https" };
                            remote.url = format!("{scheme}://127.0.0.1:{port}");
                            return Ok(());
                        }
                    }
                }
                if !remote.is_s3() { remote.url = node_url(c, &remote.url); }
                Ok(())
            }
            .await;
            let r = match r { Ok(()) => crate::store::health(&remote).await, Err(e) => Err(e) };
            match r {
                Ok(()) => {
                    if list.len() > 1 || c.kind != "direct" { log(&format!("площадка «{}»: через коннектор «{}»", site.name, c.name)); }
                    return Ok(remote);
                }
                Err(e) => {
                    let mut msg = format!("{e:#}");
                    if c.kind == "wireguard" { if let Some(h) = self.wg_handshake(&c.id).await { msg += &format!(" (последнее рукопожатие WireGuard {h} с назад)"); } else if self.status.lock().get(&c.id).map(|v| v["up"] == true).unwrap_or(false) { msg += " (рукопожатия WireGuard не было: проверьте ключи и Endpoint)"; } }
                    log(&format!("коннектор «{}»: {msg}", c.name));
                    errors.push(format!("{}: {msg}", c.name));
                }
            }
        }
        bail!("площадка «{}» недоступна: {}", site.name, errors.join("; "))
    }

    async fn wg_handshake(&self, id: &str) -> Option<i64> {
        let slot = self.slot(id);
        let g = slot.lock().await;
        match g.as_ref().map(|t| &t.live) { Some(Live::WireGuard(w)) => w.last_handshake().await, _ => None }
    }

    /// Проверка коннектора из панели: поднять канал и описать его состояние.
    pub async fn test(&self, c: &ConnectorSpec, site_url: Option<&str>) -> Result<Value> {
        let port = self.ensure_with(c, site_url.unwrap_or(""), Some(&|_: &str| {})).await?;
        let mut v = self.status.lock().get(&c.id).cloned().unwrap_or(json!({}));
        if let Some(p) = port { v["local_port"] = json!(p); }
        if c.kind == "wireguard" {
            tokio::time::sleep(Duration::from_secs(1)).await;
            v["last_handshake_seconds"] = json!(self.wg_handshake(&c.id).await);
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_pauses() {
        assert_eq!(backoff(1).as_secs(), 30);
        assert_eq!(backoff(2).as_secs(), 60);
        assert_eq!(backoff(4).as_secs(), 240);
        assert_eq!(backoff(9).as_secs(), 600);
        let l2tp = ConnectorSpec { id: "a".into(), name: "a".into(), kind: "l2tp_ipsec".into(), params: json!({}) };
        assert_eq!(min_gap(&l2tp).as_secs(), 60);
        let l2tp90 = ConnectorSpec { params: json!({ "reconnect_delay": 90 }), ..l2tp.clone() };
        assert_eq!(min_gap(&l2tp90).as_secs(), 90);
        let now = std::time::Instant::now();
        let h = Health { down_at: Some(now), next_try: Some(now + Duration::from_secs(120)), ..Default::default() };
        assert_eq!(h.not_before(Duration::from_secs(60)), Some(now + Duration::from_secs(120)));
        let h = Health { down_at: Some(now), ..Default::default() };
        assert_eq!(h.not_before(Duration::from_secs(60)), Some(now + Duration::from_secs(60)));
        assert_eq!(Health::default().not_before(Duration::from_secs(60)), None);
    }
}
