//! Восстановление на новую виртуальную машину: сервер создаёт ВМ у гипервизора или в облаке
//! (Proxmox VE, VMware vSphere, Yandex Compute Cloud, AWS EC2), передаёт ей одноразовый ключ SSH
//! через cloud-init, ждёт загрузки и выполняет обычное восстановление по SSH. При сбое ВМ удаляется
//! (если не отключено), при успехе — остаётся и появляется среди целей как сервер по SSH.

pub mod aws;
pub mod proxmox;
pub mod vmware;
pub mod yandex;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::state::Target;
use super::Server;
use crate::plugins::LogFn;
use crate::proto::JobSpec;

/// Что нужно создать.
pub struct VmRequest {
    pub name: String,
    /// Пользователь, которого создаёт cloud-init (с sudo без пароля).
    pub user: String,
    pub ssh_pubkey: String,
    /// Параметры цели с поправками на это восстановление.
    pub p: Value,
}

/// Созданная ВМ.
pub struct VmInfo {
    pub id: String,
    /// Адрес для SSH.
    pub ip: String,
    pub details: Value,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn title(&self) -> &'static str;
    /// Проверка доступа к API и шаблона/образа.
    async fn test(&self) -> Result<String>;
    async fn create(&self, req: &VmRequest, log: &LogFn) -> Result<VmInfo>;
    async fn destroy(&self, id: &str, log: &LogFn) -> Result<()>;
}

pub fn is_vm_kind(k: &str) -> bool { matches!(k, "proxmox" | "vmware" | "yandex_cloud" | "aws") }

pub fn provider(t: &Target) -> Result<Box<dyn Provider>> {
    let p = t.params.clone();
    Ok(match t.kind.as_str() {
        "proxmox" => Box::new(proxmox::Proxmox::new(p)?),
        "vmware" => Box::new(vmware::Vmware::new(p)?),
        "yandex_cloud" => Box::new(yandex::Yandex::new(p)?),
        "aws" => Box::new(aws::Aws::new(p)?),
        k => bail!("цель «{k}» не создаёт ВМ"),
    })
}

/// Общие поля ВМ-целей (ресурсы, сеть, ожидание).
fn vm_common() -> serde_json::Map<String, Value> {
    let v = json!({
        "cores": { "type": "integer", "title": "vCPU", "default": 2 },
        "memory_mb": { "type": "integer", "title": "Память, МиБ", "default": 4096 },
        "disk_gb": { "type": "integer", "title": "Диск, ГиБ", "description": "Пусто — как у шаблона/образа" },
        "ssh_user": { "type": "string", "title": "Пользователь ВМ", "default": "hopebackup", "description": "Создаётся cloud-init с sudo без пароля и одноразовым ключом" },
        "ssh_port": { "type": "integer", "title": "Порт SSH", "default": 22 },
        "boot_timeout": { "type": "integer", "title": "Ожидание загрузки, с", "default": 900 },
        "destroy_on_failure": { "type": "boolean", "title": "Удалить ВМ, если восстановление не удалось", "default": true },
        "prepare_script": { "type": "string", "format": "textarea", "title": "Подготовка ВМ (shell, от root)", "description": "Выполняется перед восстановлением: пакеты, монтирование дисков, настройки" }
    });
    v.as_object().cloned().unwrap_or_default()
}

fn schema(props: Value, required: &[&str]) -> Value {
    let mut m = props.as_object().cloned().unwrap_or_default();
    for (k, v) in vm_common() { m.entry(k).or_insert(v); }
    json!({ "type": "object", "required": required, "properties": m })
}

/// Виды целей восстановления для панели (со схемами параметров).
pub fn kinds() -> Vec<Value> {
    vec![
        json!({ "id": "ssh", "title": "Сервер по SSH", "description": "Готовая машина по IP: агент ставится по SSH на время восстановления", "available": true }),
        json!({ "id": "agent", "title": "Сервер с агентом", "description": "Машина, где уже работает агент HopeBackup", "available": true }),
        json!({ "id": "proxmox", "title": "Proxmox VE", "description": "Новая ВМ — клон шаблона с cloud-init", "available": true, "schema": schema(json!({
            "url": { "type": "string", "title": "Адрес Proxmox", "description": "https://pve.example:8006" },
            "fingerprint": { "type": "string", "title": "Отпечаток сертификата", "description": "SHA-256; пусто — проверка по системным CA" },
            "token_id": { "type": "string", "title": "API-токен", "description": "user@realm!имя, например root@pam!hopebackup" },
            "token_secret": { "type": "string", "format": "secret", "title": "Секрет токена" },
            "node": { "type": "string", "title": "Узел Proxmox", "default": "pve" },
            "template": { "type": "integer", "title": "VMID шаблона с cloud-init" },
            "full_clone": { "type": "boolean", "title": "Полный клон", "default": true, "description": "Иначе — связанный клон (быстрее, зависит от шаблона)" },
            "storage": { "type": "string", "title": "Хранилище дисков", "description": "Для полного клона; пусто — как у шаблона" },
            "pool": { "type": "string", "title": "Пул ресурсов" },
            "bridge": { "type": "string", "title": "Сетевой мост", "description": "Например vmbr1; пусто — как у шаблона" },
            "ip": { "type": "string", "title": "Адрес ВМ", "default": "dhcp", "description": "dhcp или 10.0.0.50/24 (DHCP: адрес узнаётся через qemu-guest-agent)" },
            "gateway": { "type": "string", "title": "Шлюз" },
            "dns": { "type": "string", "title": "DNS-серверы", "description": "Через пробел" },
            "disk": { "type": "string", "title": "Системный диск", "default": "scsi0" }
        }), &["url", "token_id", "token_secret", "node", "template"]) }),
        json!({ "id": "vmware", "title": "VMware vSphere", "description": "Клон ВМ-шаблона или шаблон библиотеки; cloud-init (vSphere 7.0 U3+) или спецификация настройки", "available": true, "schema": schema(json!({
            "url": { "type": "string", "title": "Адрес vCenter", "description": "https://vcenter.example" },
            "fingerprint": { "type": "string", "title": "Отпечаток сертификата", "description": "SHA-256; пусто — проверка по системным CA" },
            "username": { "type": "string", "title": "Пользователь", "description": "hopebackup@vsphere.local" },
            "password": { "type": "string", "format": "secret", "title": "Пароль" },
            "source_type": { "type": "string", "title": "Источник", "enum": ["vm", "library"], "default": "vm", "description": "vm — выключенная ВМ-эталон; library — шаблон из библиотеки содержимого" },
            "source": { "type": "string", "title": "Имя эталона / шаблона" },
            "folder": { "type": "string", "title": "Папка ВМ" },
            "resource_pool": { "type": "string", "title": "Пул ресурсов" },
            "cluster": { "type": "string", "title": "Кластер" },
            "host": { "type": "string", "title": "Хост ESXi" },
            "datastore": { "type": "string", "title": "Хранилище" },
            "customization": { "type": "string", "title": "Настройка гостя", "enum": ["cloudinit", "spec", "none"], "default": "cloudinit", "description": "cloudinit — ключ, пользователь и сеть через cloud-init; spec — готовая спецификация vCenter; none — ключ уже в эталоне" },
            "spec_name": { "type": "string", "title": "Спецификация настройки (для spec)" },
            "private_key": { "type": "string", "format": "secret-textarea", "title": "Ключ SSH эталона (для spec/none)", "description": "Закрытый ключ пользователя, уже разрешённого в эталоне" },
            "ip": { "type": "string", "title": "Адрес ВМ", "default": "dhcp", "description": "dhcp или 10.0.0.50/24 (DHCP: адрес — от VMware Tools)" },
            "gateway": { "type": "string", "title": "Шлюз" },
            "dns": { "type": "string", "title": "DNS-серверы", "description": "Через пробел" }
        }), &["url", "username", "password", "source"]) }),
        json!({ "id": "yandex_cloud", "title": "Yandex Cloud", "description": "Новая ВМ в Compute Cloud из образа, cloud-init", "available": true, "schema": schema(json!({
            "auth": { "type": "string", "title": "Аутентификация", "enum": ["service_account_key", "oauth", "iam_token"], "default": "service_account_key" },
            "sa_key": { "type": "string", "format": "secret-textarea", "title": "Авторизованный ключ сервисного аккаунта (JSON)", "description": "yc iam key create --service-account-name hopebackup -o key.json" },
            "oauth_token": { "type": "string", "format": "secret", "title": "OAuth-токен" },
            "iam_token": { "type": "string", "format": "secret", "title": "IAM-токен (до 12 ч)" },
            "folder_id": { "type": "string", "title": "Каталог (folder_id)" },
            "zone": { "type": "string", "title": "Зона", "default": "ru-central1-a" },
            "subnet_id": { "type": "string", "title": "Подсеть (subnet_id)" },
            "security_group_ids": { "type": "array", "items": { "type": "string" }, "title": "Группы безопасности" },
            "public_ip": { "type": "boolean", "title": "Публичный адрес", "default": false, "description": "Иначе SSH — по внутреннему адресу (сервер управления в той же сети или через VPN)" },
            "platform": { "type": "string", "title": "Платформа", "default": "standard-v3" },
            "core_fraction": { "type": "integer", "title": "Гарантированная доля vCPU, %", "default": 100 },
            "image_id": { "type": "string", "title": "Образ (image_id)", "description": "Пусто — последний из семейства" },
            "image_family": { "type": "string", "title": "Семейство образов", "default": "ubuntu-2404-lts" },
            "disk_type": { "type": "string", "title": "Тип диска", "default": "network-ssd" },
            "endpoint": { "type": "string", "title": "API (для отладки)", "description": "Пусто — api.cloud.yandex.net" }
        }), &["folder_id", "subnet_id"]) }),
        json!({ "id": "aws", "title": "AWS EC2", "description": "Новый инстанс из AMI, cloud-init (user-data)", "available": true, "schema": schema(json!({
            "region": { "type": "string", "title": "Регион", "default": "eu-central-1" },
            "access_key": { "type": "string", "title": "Access key ID" },
            "secret_key": { "type": "string", "format": "secret", "title": "Secret access key" },
            "instance_type": { "type": "string", "title": "Тип инстанса", "default": "t3.large" },
            "ami": { "type": "string", "title": "AMI", "description": "Пусто — последний Ubuntu 24.04 от Canonical" },
            "subnet_id": { "type": "string", "title": "Подсеть (subnet-…)" },
            "security_group_ids": { "type": "array", "items": { "type": "string" }, "title": "Группы безопасности (sg-…)" },
            "public_ip": { "type": "boolean", "title": "Публичный адрес", "default": false },
            "volume_type": { "type": "string", "title": "Тип диска", "default": "gp3" },
            "endpoint": { "type": "string", "title": "Адрес API EC2", "description": "Пусто — https://ec2.<регион>.amazonaws.com (VPC endpoint, LocalStack)" }
        }), &["region", "access_key", "secret_key", "subnet_id"]) }),
        json!({ "id": "hyperv", "title": "Hyper-V", "description": "PowerShell через WinRM", "available": false }),
    ]
}

// ---------------------------------------------------------------- общие помощники провайдеров

pub fn p_str(v: &Value, k: &str) -> String { crate::connectors::net::p_str(v, k) }
pub fn p_raw(v: &Value, k: &str) -> String { crate::connectors::net::p_raw(v, k) }
pub fn p_u64(v: &Value, k: &str, d: u64) -> u64 { crate::connectors::net::p_u64(v, k, d) }
pub fn p_bool(v: &Value, k: &str, d: bool) -> bool { crate::connectors::net::p_bool(v, k, d) }
pub fn p_list(v: &Value, k: &str) -> Vec<String> { crate::connectors::net::p_list(v, k) }

/// Процентное кодирование (RFC 3986, кодируется всё, кроме незарезервированных символов).
pub fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { o.push(b as char) } else { o += &format!("%{b:02X}") }
    }
    o
}

/// HTTP-клиент к API гипервизора: закреплённый отпечаток или системные CA.
pub fn http(fingerprint: &str) -> Result<reqwest::Client> {
    let fp = crate::tlsutil::normalize_fp(fingerprint);
    crate::tlsutil::client(if fp.is_empty() { None } else { Some(fingerprint) })
}

/// Адрес ВМ из параметра `ip`: `dhcp` → None, `10.0.0.5/24` → (10.0.0.5, 24).
pub fn static_ip(p: &Value) -> Result<Option<(String, u8)>> {
    let ip = p_str(p, "ip");
    if ip.is_empty() || ip.eq_ignore_ascii_case("dhcp") { return Ok(None); }
    let (a, m) = crate::connectors::net::parse_cidr(&ip)?;
    if m == 32 && !ip.contains('/') { bail!("адрес ВМ укажите с маской: {ip}/24"); }
    Ok(Some((a.to_string(), m)))
}

/// Пользовательские данные cloud-init: пользователь с ключом и sudo без пароля.
pub fn cloud_config(user: &str, pubkey: &str, hostname: &str) -> String {
    format!(
        "#cloud-config\nhostname: {hostname}\npreserve_hostname: false\nssh_pwauth: false\nusers:\n  - default\n  - name: {user}\n    gecos: HopeBackup restore\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    shell: /bin/bash\n    lock_passwd: true\n    ssh_authorized_keys:\n      - {key}\n",
        key = pubkey.trim()
    )
}

/// Сетевая конфигурация cloud-init (network v2) для метаданных VMware.
pub fn netplan(p: &Value) -> Result<String> {
    let mut s = String::from("network:\n  version: 2\n  ethernets:\n    nic0:\n      match:\n        name: \"e*\"\n");
    match static_ip(p)? {
        None => s += "      dhcp4: true\n",
        Some((ip, m)) => {
            s += &format!("      dhcp4: false\n      addresses: [{ip}/{m}]\n");
            let gw = p_str(p, "gateway");
            if !gw.is_empty() { s += &format!("      routes:\n        - to: default\n          via: {gw}\n"); }
            let dns: Vec<String> = p_str(p, "dns").split_whitespace().map(|x| x.to_string()).collect();
            if !dns.is_empty() { s += &format!("      nameservers:\n        addresses: [{}]\n", dns.join(", ")); }
        }
    }
    Ok(s)
}

/// Одноразовая пара ключей SSH для новой ВМ (ssh-keygen из OpenSSH).
pub async fn keypair(tag: &str) -> Result<(String, String)> {
    let dir = crate::agent::data_dir().join("ssh").join("vm");
    std::fs::create_dir_all(&dir)?;
    let f = dir.join(format!("{tag}.key"));
    let _ = std::fs::remove_file(&f);
    let _ = std::fs::remove_file(f.with_extension("key.pub"));
    let mut c = tokio::process::Command::new("ssh-keygen");
    c.args(["-q", "-t", "ed25519", "-N", "", "-C", &format!("hopebackup-{tag}"), "-f"]).arg(&f);
    crate::util::run(&mut c).await.context("ssh-keygen (нужен OpenSSH)")?;
    let private = std::fs::read_to_string(&f)?;
    let public = std::fs::read_to_string(dir.join(format!("{tag}.key.pub")))?;
    let _ = std::fs::remove_file(&f);
    let _ = std::fs::remove_file(dir.join(format!("{tag}.key.pub")));
    Ok((private, public.trim().to_string()))
}

/// Объединить параметры цели и поправки этого восстановления (`params.vm`).
fn merged(t: &Target, over: &Value) -> Value {
    let mut p = t.params.clone();
    if let (Some(m), Some(o)) = (p.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            if !v.is_null() && v.as_str() != Some("") { m.insert(k.clone(), v.clone()); }
        }
    }
    p
}

fn vm_name(p: &Value, instance: &str) -> String {
    let n = p_str(p, "vm_name");
    if !n.is_empty() { return n; }
    let base: String = instance.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let base = base.trim_matches('-').chars().take(40).collect::<String>();
    format!("hb-{base}-{}", chrono::Utc::now().format("%m%d%H%M"))
}

/// Ждать, пока ВМ примет SSH (cloud-init создаёт пользователя и ключ при первой загрузке).
async fn wait_ssh(t: &Target, timeout: Duration, log: &LogFn) -> Result<String> {
    let started = std::time::Instant::now();
    let mut last = String::new();
    let mut said = false;
    while started.elapsed() < timeout {
        match super::ssh_restore::test(t).await {
            Ok(info) => return Ok(info),
            Err(e) => {
                last = format!("{e:#}");
                if !said { log(&format!("ожидание SSH на {}:{} (загрузка, cloud-init)…", t.host, t.port)); said = true; }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    bail!("ВМ не приняла SSH за {} с: {last}", timeout.as_secs())
}

/// Выполнить команду на цели (от root).
async fn ssh_sh(t: &Target, script: &str, log: &LogFn) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut c = super::ssh_restore::base_cmd(t)?;
    let sudo = if t.user == "root" { "" } else { "sudo -n " };
    c.arg("--").arg(super::ssh_restore::dest(t)?).arg(format!("{sudo}sh -s"));
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut ch = c.spawn()?;
    let mut si = ch.stdin.take().unwrap();
    si.write_all(script.as_bytes()).await?;
    drop(si);
    let out = ch.wait_with_output().await?;
    for l in String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out.stderr).lines()) {
        if !l.trim().is_empty() && !l.contains("Warning: Permanently added") { log(&format!("ВМ: {l}")); }
    }
    if !out.status.success() { bail!("подготовка ВМ завершилась с кодом {}", out.status.code().unwrap_or(-1)); }
    Ok(())
}

/// Восстановление на новую ВМ.
pub async fn restore(s: &Server, mut spec: JobSpec, t: &Target, via: &str, log: LogFn) -> Result<Value> {
    let over = spec.params.get("vm").cloned().unwrap_or(Value::Null);
    if let Some(o) = spec.params.as_object_mut() { o.remove("vm"); }
    let p = merged(t, &over);
    let prov = provider(&Target { params: p.clone(), ..t.clone() })?;
    let instance = spec.instance.as_ref().map(|i| i.id.clone()).unwrap_or_default();
    let name = vm_name(&p, &instance);
    let custom = p_str(&p, "customization");
    let uses_cloudinit = !(t.kind == "vmware" && (custom == "spec" || custom == "none"));
    let user = { let u = p_str(&p, "ssh_user"); if u.is_empty() { "hopebackup".to_string() } else { u } };
    let (private_key, pubkey) = if uses_cloudinit {
        keypair(&spec.id).await?
    } else {
        let k = p_raw(&p, "private_key");
        if k.trim().is_empty() { bail!("для настройки «{custom}» укажите закрытый ключ SSH эталона"); }
        (k, String::new())
    };
    log(&format!("{}: создание ВМ «{name}»", prov.title()));
    let req = VmRequest { name: name.clone(), user: user.clone(), ssh_pubkey: pubkey, p: p.clone() };
    let vm = prov.create(&req, &log).await?;
    log(&format!("ВМ «{name}» создана (id {}), адрес {}", vm.id, vm.ip));
    let ssh_t = Target {
        id: format!("vm-{}", spec.id),
        name: name.clone(),
        kind: "ssh".into(),
        host: vm.ip.clone(),
        port: p_u64(&p, "ssh_port", 22) as u16,
        user: user.clone(),
        password: String::new(),
        private_key: private_key.clone(),
        agent_id: String::new(),
        params: Value::Null,
        created: chrono::Utc::now(),
    };
    let r: Result<Value> = async {
        let info = wait_ssh(&ssh_t, Duration::from_secs(p_u64(&p, "boot_timeout", 900)), &log).await?;
        log(&format!("ВМ доступна по SSH: {info}"));
        if uses_cloudinit {
            // Дождаться окончания cloud-init: иначе apt/dnf внутри восстановления упрётся в блокировку.
            let _ = ssh_sh(&ssh_t, "command -v cloud-init >/dev/null && timeout 900 cloud-init status --wait >/dev/null 2>&1; true\n", &log).await;
        }
        let prep = p_raw(&p, "prepare_script");
        if !prep.trim().is_empty() {
            log("подготовка ВМ (prepare_script)");
            ssh_sh(&ssh_t, &format!("set -e\n{prep}\n"), &log).await?;
        }
        super::ssh_restore::run(s, spec.clone(), &ssh_t, via, log.clone()).await
    }
    .await;
    match r {
        Ok(mut v) => {
            // Новая машина — сразу среди целей (сервер по SSH) с ключом, которым она создана.
            let tid = crate::util::short_id();
            let saved = s.store.update(|st| {
                st.targets.push(Target { id: tid.clone(), name: format!("{name} ({})", prov.title()), created: chrono::Utc::now(), ..ssh_t.clone() });
                Ok(())
            });
            if saved.is_ok() { log(&format!("ВМ добавлена в цели восстановления как «{name} ({})»", prov.title())); }
            let vmv = json!({ "provider": t.kind, "id": vm.id, "name": name, "ip": vm.ip, "user": user, "target_id": tid, "details": vm.details });
            if let Some(o) = v.as_object_mut() { o.insert("vm".into(), vmv); } else { v = json!({ "result": v, "vm": vmv }); }
            Ok(v)
        }
        Err(e) => {
            if p_bool(&p, "destroy_on_failure", true) {
                log(&format!("удаление ВМ «{name}» (destroy_on_failure)"));
                if let Err(de) = prov.destroy(&vm.id, &log).await { log(&format!("ВМ удалить не удалось: {de:#} — удалите вручную (id {})", vm.id)); }
            } else {
                log(&format!("ВМ «{name}» оставлена для разбора (id {}, адрес {})", vm.id, vm.ip));
            }
            Err(e)
        }
    }
}

/// Проверка ВМ-цели из панели.
pub async fn test(t: &Target) -> Result<String> { provider(t)?.test().await }

/// Ответ API с ошибкой — текст для оператора.
pub async fn api_error(r: reqwest::Response, what: &str) -> anyhow::Error {
    let st = r.status();
    let body = r.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("errors").map(|e| e.to_string())
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .or_else(|| v.get("error_type").map(|e| format!("{e} {}", v.get("messages").map(|m| m.to_string()).unwrap_or_default())))
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    anyhow!("{what}: HTTP {st}: {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(pct("ssh-ed25519 AAAA/b+c= x@y"), "ssh-ed25519%20AAAA%2Fb%2Bc%3D%20x%40y");
        let n = netplan(&json!({ "ip": "10.0.0.50/24", "gateway": "10.0.0.1", "dns": "1.1.1.1 8.8.8.8" })).unwrap();
        assert!(n.contains("addresses: [10.0.0.50/24]") && n.contains("via: 10.0.0.1") && n.contains("[1.1.1.1, 8.8.8.8]"));
        assert!(netplan(&json!({ "ip": "dhcp" })).unwrap().contains("dhcp4: true"));
        assert!(static_ip(&json!({ "ip": "10.0.0.50" })).is_err());
        let c = cloud_config("hopebackup", "ssh-ed25519 AAAA test", "vm1");
        assert!(c.starts_with("#cloud-config") && c.contains("NOPASSWD:ALL") && c.contains("- ssh-ed25519 AAAA test"));
    }
}
