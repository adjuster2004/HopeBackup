//! TLS: самоподписанные сертификаты узлов и клиенты с закреплённым отпечатком.
//!
//! Узел площадки и сервер управления создают себе сертификат при первом запуске. Клиенты не доверяют
//! цепочкам сертификации, а сверяют SHA-256 сертификата с отпечатком, который оператор подтвердил
//! при подключении (как в репликации HopeDFS). Имя хоста не проверяется: к узлу ходят и через туннели.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Сертификат и ключ в каталоге `dir` (создаются при первом вызове).
pub fn ensure_cert(dir: &Path, extra_names: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
    let cp = dir.join("tls.crt");
    let kp = dir.join("tls.key");
    if let (Ok(c), Ok(k)) = (std::fs::read(&cp), std::fs::read(&kp)) {
        return Ok((c, k));
    }
    let mut names = vec!["localhost".to_string(), crate::util::hostname()];
    names.extend(extra_names.iter().cloned());
    names.extend(crate::util::local_ips());
    names.sort();
    names.dedup();
    let ck = rcgen::generate_simple_self_signed(names).map_err(|e| anyhow!("создание сертификата: {e}"))?;
    let cert = ck.cert.pem().into_bytes();
    let key = ck.key_pair.serialize_pem().into_bytes();
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cp, &cert)?;
    crate::util::write_secret(&kp, &key)?;
    tracing::info!("создан самоподписанный сертификат {}", cp.display());
    Ok((cert, key))
}

/// DER первого сертификата из PEM.
pub fn pem_to_der(pem: &[u8]) -> Result<Vec<u8>> {
    let s = std::str::from_utf8(pem)?;
    let start = s.find("-----BEGIN CERTIFICATE-----").context("в файле нет сертификата")? + "-----BEGIN CERTIFICATE-----".len();
    let end = s[start..].find("-----END CERTIFICATE-----").context("сертификат обрезан")? + start;
    let b64: String = s[start..end].chars().filter(|c| !c.is_whitespace()).collect();
    Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
}

/// Отпечаток SHA-256 сертификата в виде `AB:CD:…`.
pub fn fingerprint_der(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

pub fn fingerprint_pem(pem: &[u8]) -> Result<String> { Ok(fingerprint_der(&pem_to_der(pem)?)) }

/// Нормализовать отпечаток: только шестнадцатеричные цифры, нижний регистр.
pub fn normalize_fp(fp: &str) -> String { fp.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase() }

#[derive(Debug)]
struct Pinned {
    fp: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _i: &[CertificateDer<'_>], _n: &ServerName<'_>, _o: &[u8], _t: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let d = Sha256::digest(end_entity.as_ref());
        if subtle::ConstantTimeEq::ct_eq(d.as_slice(), self.fp.as_slice()).into() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!("отпечаток сертификата сервера {} не совпадает с закреплённым", fingerprint_der(end_entity.as_ref()))))
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> { self.provider.signature_verification_algorithms.supported_schemes() }
}

/// Принять любой сертификат, но запомнить его отпечаток (для первого подключения: показать оператору).
#[derive(Debug)]
struct Capture {
    seen: Arc<parking_lot::Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Capture {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _i: &[CertificateDer<'_>], _n: &ServerName<'_>, _o: &[u8], _t: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock() = Some(fingerprint_der(end_entity.as_ref()));
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> { self.provider.signature_verification_algorithms.supported_schemes() }
}

fn provider() -> Arc<CryptoProvider> { Arc::new(rustls::crypto::ring::default_provider()) }

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .user_agent(concat!("hopebackup/", env!("CARGO_PKG_VERSION")))
}

/// HTTP-клиент: для `https://` обязателен отпечаток сертификата.
pub fn client(fingerprint: Option<&str>) -> Result<reqwest::Client> {
    match fingerprint.map(normalize_fp).filter(|s| !s.is_empty()) {
        Some(fp) => {
            let fp = hex::decode(&fp).map_err(|_| anyhow!("неверный отпечаток сертификата"))?;
            if fp.len() != 32 { return Err(anyhow!("отпечаток сертификата должен быть SHA-256 (32 байта)")); }
            let p = provider();
            let cfg = rustls::ClientConfig::builder_with_provider(p.clone())
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned { fp, provider: p }))
                .with_no_client_auth();
            Ok(builder().use_preconfigured_tls(cfg).build()?)
        }
        None => Ok(builder().build()?),
    }
}

/// Узнать отпечаток сертификата сервера (для подтверждения оператором при первом подключении).
pub async fn probe_fingerprint(url: &str) -> Result<String> {
    let seen = Arc::new(parking_lot::Mutex::new(None));
    let p = provider();
    let cfg = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Capture { seen: seen.clone(), provider: p }))
        .with_no_client_auth();
    let c = builder().use_preconfigured_tls(cfg).build()?;
    let _ = c.get(url).send().await;
    let fp = seen.lock().clone();
    fp.ok_or_else(|| anyhow!("не удалось подключиться к {url}"))
}
