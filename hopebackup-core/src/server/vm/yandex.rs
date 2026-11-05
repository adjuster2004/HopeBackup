//! Yandex Cloud: новая ВМ в Compute Cloud из образа (последний в семействе или указанный), ключ и
//! пользователь — через cloud-init (`user-data`). Аутентификация: авторизованный ключ сервисного
//! аккаунта (JWT PS256 → IAM-токен), OAuth-токен или готовый IAM-токен.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

use super::{cloud_config, p_bool, p_list, p_raw, p_str, p_u64, Provider, VmInfo, VmRequest};
use crate::plugins::LogFn;

const IAM_URL: &str = "https://iam.api.cloud.yandex.net/iam/v1/tokens";

pub struct Yandex {
    c: reqwest::Client,
    p: Value,
    /// Один адрес для всех API (отладка, имитатор); пусто — публичные адреса Yandex Cloud.
    base: Option<String>,
}

fn b64url(b: &[u8]) -> String { base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b) }

/// JWT для обмена на IAM-токен: PS256, `kid` — идентификатор ключа, `iss` — сервисный аккаунт.
pub fn sa_jwt(key_json: &str, now: i64) -> Result<String> {
    let k: Value = serde_json::from_str(key_json).context("авторизованный ключ: ожидается JSON из `yc iam key create`")?;
    let kid = k["id"].as_str().ok_or_else(|| anyhow!("в ключе нет поля id"))?;
    let sa = k["service_account_id"].as_str().ok_or_else(|| anyhow!("в ключе нет поля service_account_id"))?;
    let pem = k["private_key"].as_str().ok_or_else(|| anyhow!("в ключе нет private_key"))?;
    // В ключе Yandex перед PEM бывает служебная строка «PLEASE DO NOT REMOVE THIS LINE!…».
    let start = pem.find("-----BEGIN PRIVATE KEY-----").ok_or_else(|| anyhow!("private_key: нет PKCS#8 PEM"))? + "-----BEGIN PRIVATE KEY-----".len();
    let end = pem[start..].find("-----END PRIVATE KEY-----").ok_or_else(|| anyhow!("private_key обрезан"))? + start;
    let der = base64::engine::general_purpose::STANDARD.decode(pem[start..end].chars().filter(|c| !c.is_whitespace()).collect::<String>())?;
    let kp = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|e| anyhow!("закрытый ключ сервисного аккаунта: {e}"))?;
    let header = json!({ "typ": "JWT", "alg": "PS256", "kid": kid });
    let payload = json!({ "aud": IAM_URL, "iss": sa, "iat": now, "exp": now + 3600 });
    let msg = format!("{}.{}", b64url(header.to_string().as_bytes()), b64url(payload.to_string().as_bytes()));
    let mut sig = vec![0u8; kp.public().modulus_len()];
    kp.sign(&ring::signature::RSA_PSS_SHA256, &ring::rand::SystemRandom::new(), msg.as_bytes(), &mut sig).map_err(|_| anyhow!("подпись JWT не удалась"))?;
    Ok(format!("{msg}.{}", b64url(&sig)))
}

impl Yandex {
    pub fn new(p: Value) -> Result<Yandex> {
        if p_str(&p, "folder_id").is_empty() { bail!("не задан каталог (folder_id)"); }
        if p_str(&p, "subnet_id").is_empty() { bail!("не задана подсеть (subnet_id)"); }
        let auth = p_str(&p, "auth");
        let ok = match auth.as_str() {
            "oauth" => !p_raw(&p, "oauth_token").is_empty(),
            "iam_token" => !p_raw(&p, "iam_token").is_empty(),
            _ => !p_raw(&p, "sa_key").is_empty(),
        };
        if !ok { bail!("не заданы данные аутентификации Yandex Cloud ({})", if auth.is_empty() { "service_account_key" } else { &auth }); }
        let base = Some(p_str(&p, "endpoint")).filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string());
        Ok(Yandex { c: crate::tlsutil::client(None)?, p, base })
    }

    fn url(&self, svc: &str, path: &str) -> String {
        match &self.base { Some(b) => format!("{b}{path}"), None => format!("https://{svc}.api.cloud.yandex.net{path}") }
    }

    async fn iam(&self) -> Result<String> {
        let body = match p_str(&self.p, "auth").as_str() {
            "iam_token" => return Ok(p_raw(&self.p, "iam_token").trim().to_string()),
            "oauth" => json!({ "yandexPassportOauthToken": p_raw(&self.p, "oauth_token").trim() }),
            _ => json!({ "jwt": sa_jwt(&p_raw(&self.p, "sa_key"), chrono::Utc::now().timestamp())? }),
        };
        let r = self.c.post(self.url("iam", "/iam/v1/tokens")).json(&body).timeout(Duration::from_secs(30)).send().await.map_err(|e| anyhow!("Yandex IAM: {e}"))?;
        if !r.status().is_success() { return Err(super::api_error(r, "Yandex IAM: получение токена").await); }
        let v: Value = r.json().await?;
        v["iamToken"].as_str().map(|s| s.to_string()).ok_or_else(|| anyhow!("Yandex IAM: нет iamToken в ответе"))
    }

    async fn call(&self, tok: &str, m: reqwest::Method, url: String, body: Option<Value>) -> Result<Value> {
        let mut r = self.c.request(m.clone(), &url).bearer_auth(tok).timeout(Duration::from_secs(60));
        if let Some(b) = body { r = r.json(&b); }
        let resp = r.send().await.map_err(|e| anyhow!("Yandex Cloud {url}: {e}"))?;
        if !resp.status().is_success() { return Err(super::api_error(resp, &format!("Yandex Cloud {m}")).await); }
        Ok(resp.json().await.unwrap_or(Value::Null))
    }

    async fn wait_op(&self, tok: &str, op: &Value, what: &str, timeout: Duration) -> Result<Value> {
        let id = op["id"].as_str().ok_or_else(|| anyhow!("Yandex Cloud: {what} — нет идентификатора операции"))?.to_string();
        let started = std::time::Instant::now();
        let mut cur = op.clone();
        loop {
            if cur["done"] == true {
                if let Some(e) = cur.get("error").filter(|e| !e.is_null()) { bail!("Yandex Cloud: {what} — {}", e["message"].as_str().unwrap_or(&e.to_string())); }
                return Ok(cur);
            }
            if started.elapsed() > timeout { bail!("Yandex Cloud: {what} — операция {id} не завершилась за {} с", timeout.as_secs()); }
            tokio::time::sleep(Duration::from_secs(3)).await;
            cur = self.call(tok, reqwest::Method::GET, self.url("operation", &format!("/operations/{id}")), None).await?;
        }
    }

    async fn image(&self, tok: &str) -> Result<(String, u64)> {
        let id = p_str(&self.p, "image_id");
        let v = if !id.is_empty() {
            self.call(tok, reqwest::Method::GET, self.url("compute", &format!("/compute/v1/images/{id}")), None).await?
        } else {
            let fam = { let f = p_str(&self.p, "image_family"); if f.is_empty() { "ubuntu-2404-lts".to_string() } else { f } };
            self.call(tok, reqwest::Method::GET, self.url("compute", &format!("/compute/v1/images:latestByFamily?folderId=standard-images&family={}", super::pct(&fam))), None).await?
        };
        let img = v["id"].as_str().ok_or_else(|| anyhow!("Yandex Cloud: образ не найден"))?.to_string();
        let min = v["minDiskSize"].as_str().and_then(|s| s.parse().ok()).or_else(|| v["minDiskSize"].as_u64()).unwrap_or(0);
        Ok((img, min))
    }

    fn ip_of(inst: &Value, public: bool) -> Option<String> {
        let ni = inst["networkInterfaces"].get(0)?;
        let a = &ni["primaryV4Address"];
        let ip = if public { a["oneToOneNat"]["address"].as_str() } else { a["address"].as_str() };
        ip.filter(|s| !s.is_empty()).map(|s| s.to_string())
    }
}

#[async_trait]
impl Provider for Yandex {
    fn title(&self) -> &'static str { "Yandex Cloud" }

    async fn test(&self) -> Result<String> {
        let tok = self.iam().await?;
        let folder = p_str(&self.p, "folder_id");
        self.call(&tok, reqwest::Method::GET, self.url("compute", &format!("/compute/v1/instances?folderId={}&pageSize=1", super::pct(&folder))), None).await?;
        let (img, _) = self.image(&tok).await?;
        let sub = self.call(&tok, reqwest::Method::GET, self.url("vpc", &format!("/vpc/v1/subnets/{}", p_str(&self.p, "subnet_id"))), None).await?;
        let zone = { let z = p_str(&self.p, "zone"); if z.is_empty() { "ru-central1-a".to_string() } else { z } };
        let sz = sub["zoneId"].as_str().unwrap_or("");
        if !sz.is_empty() && sz != zone { bail!("подсеть {} в зоне {sz}, а в цели указана {zone}", p_str(&self.p, "subnet_id")); }
        Ok(format!("Yandex Cloud: токен получен, каталог {folder}, образ {img}, подсеть {} ({sz})", p_str(&self.p, "subnet_id")))
    }

    async fn create(&self, req: &VmRequest, log: &LogFn) -> Result<VmInfo> {
        let p = &req.p;
        let tok = self.iam().await?;
        let (img, min) = self.image(&tok).await?;
        let disk = (p_u64(p, "disk_gb", 20) << 30).max(min);
        let public = p_bool(p, "public_ip", false);
        let mut nic = json!({ "subnetId": p_str(p, "subnet_id"), "primaryV4AddressSpec": {} });
        if public { nic["primaryV4AddressSpec"]["oneToOneNatSpec"] = json!({ "ipVersion": "IPV4" }); }
        let sgs = p_list(p, "security_group_ids");
        if !sgs.is_empty() { nic["securityGroupIds"] = json!(sgs); }
        let name: String = req.name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).take(63).collect();
        let or = |k: &str, d: &str| { let v = p_str(p, k); if v.is_empty() { d.to_string() } else { v } };
        let (zone, platform, dtype) = (or("zone", "ru-central1-a"), or("platform", "standard-v3"), or("disk_type", "network-ssd"));
        let body = json!({
            "folderId": p_str(p, "folder_id"),
            "name": name,
            "hostname": name,
            "description": "HopeBackup: восстановление",
            "labels": { "hopebackup": "restore" },
            "zoneId": zone,
            "platformId": platform,
            "resourcesSpec": { "memory": (p_u64(p, "memory_mb", 4096) << 20).to_string(), "cores": p_u64(p, "cores", 2).to_string(), "coreFraction": p_u64(p, "core_fraction", 100).to_string() },
            "metadata": { "user-data": cloud_config(&req.user, &req.ssh_pubkey, &name), "ssh-keys": format!("{}:{}", req.user, req.ssh_pubkey) },
            "bootDiskSpec": { "autoDelete": true, "diskSpec": { "typeId": dtype, "size": disk.to_string(), "imageId": img } },
            "networkInterfaceSpecs": [nic],
        });
        log(&format!("создание ВМ в каталоге {} (образ {img}, диск {} ГиБ)", p_str(p, "folder_id"), disk >> 30));
        let op = self.call(&tok, reqwest::Method::POST, self.url("compute", "/compute/v1/instances"), Some(body)).await?;
        let id = op["metadata"]["instanceId"].as_str().map(|s| s.to_string());
        let done = match self.wait_op(&tok, &op, "создание ВМ", Duration::from_secs(900)).await {
            Ok(d) => d,
            Err(e) => {
                if let Some(id) = &id { if p_bool(p, "destroy_on_failure", true) { let _ = self.destroy(id, log).await; } }
                return Err(e);
            }
        };
        let id = id.or_else(|| done["response"]["id"].as_str().map(|s| s.to_string())).ok_or_else(|| anyhow!("Yandex Cloud: не вернул идентификатор ВМ"))?;
        let inst = self.call(&tok, reqwest::Method::GET, self.url("compute", &format!("/compute/v1/instances/{id}")), None).await?;
        let ip = Self::ip_of(&inst, public).ok_or_else(|| anyhow!("у ВМ {id} нет {} адреса", if public { "публичного" } else { "внутреннего" }))?;
        Ok(VmInfo { id: id.clone(), ip, details: json!({ "instance_id": id, "folder_id": p_str(p, "folder_id"), "image_id": img }) })
    }

    async fn destroy(&self, id: &str, log: &LogFn) -> Result<()> {
        let tok = self.iam().await?;
        let op = self.call(&tok, reqwest::Method::DELETE, self.url("compute", &format!("/compute/v1/instances/{id}")), None).await?;
        self.wait_op(&tok, &op, "удаление ВМ", Duration::from_secs(600)).await?;
        log(&format!("ВМ {id} удалена"));
        Ok(())
    }
}
