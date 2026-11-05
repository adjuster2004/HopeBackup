//! Шифрование и сжатие блоков.
//!
//! Блок на площадке: `nonce(24) || XChaCha20-Poly1305(flag || payload)`, где flag = 0 (как есть) или 1 (zstd),
//! а связанные данные (AAD) — идентификатор блока или имя объекта: блок нельзя подменить другим.
//! Мастер-ключ репозитория случайный; на площадке он хранится зашифрованным ключом из пароля (Argon2id).

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use rand::RngCore;

use super::types::{ChunkId, KdfParams};

const NONCE: usize = 24;

/// Ключи, выведенные из мастер-ключа.
#[derive(Clone)]
pub struct Keys {
    master: [u8; 32],
    /// Шифрование блоков и объектов.
    pub data: [u8; 32],
    /// Ключ идентификаторов блоков (keyed BLAKE3).
    pub id: [u8; 32],
    /// Шифрование тел снапшотов.
    pub meta: [u8; 32],
    /// Затравка gear-хэша FastCDC: границы блоков зависят от ключа репозитория.
    pub cdc_seed: u64,
}

impl Keys {
    pub fn from_master(master: [u8; 32]) -> Keys {
        let data = blake3::derive_key("hopebackup 2026 data key v1", &master);
        let id = blake3::derive_key("hopebackup 2026 chunk id key v1", &master);
        let meta = blake3::derive_key("hopebackup 2026 snapshot key v1", &master);
        let s = blake3::derive_key("hopebackup 2026 cdc seed v1", &master);
        let cdc_seed = u64::from_le_bytes(s[..8].try_into().unwrap());
        Keys { master, data, id, meta, cdc_seed }
    }

    pub fn generate() -> Keys {
        let mut m = [0u8; 32];
        OsRng.fill_bytes(&mut m);
        Keys::from_master(m)
    }

    pub fn master(&self) -> &[u8; 32] { &self.master }

    pub fn chunk_id(&self, data: &[u8]) -> ChunkId { ChunkId(*blake3::keyed_hash(&self.id, data).as_bytes()) }
}

/// Сжать (если выгодно) и зашифровать.
pub fn seal(key: &[u8; 32], aad: &[u8], data: &[u8], level: i32) -> Vec<u8> {
    let mut pt = Vec::with_capacity(data.len() + 1);
    let mut compressed = false;
    if level > 0 && data.len() > 64 {
        if let Ok(c) = zstd::bulk::compress(data, level) {
            // Несжимаемые данные (архивы, медиа, уже сжатые файлы) хранятся как есть.
            if c.len() + data.len() / 32 < data.len() {
                pt.push(1);
                pt.extend_from_slice(&c);
                compressed = true;
            }
        }
    }
    if !compressed {
        pt.push(0);
        pt.extend_from_slice(data);
    }
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, Payload { msg: &pt, aad }).expect("шифрование не может завершиться ошибкой");
    let mut out = Vec::with_capacity(NONCE + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Расшифровать и распаковать.
pub fn open(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < NONCE + 17 { bail!("повреждённый блок: слишком короткий"); }
    let cipher = XChaCha20Poly1305::new(key.into());
    let (nonce, ct) = blob.split_at(NONCE);
    let pt = cipher
        .decrypt(nonce.into(), Payload { msg: ct, aad })
        .map_err(|_| anyhow!("не удалось расшифровать блок: неверный ключ или данные повреждены"))?;
    match pt.first() {
        Some(0) => Ok(pt[1..].to_vec()),
        Some(1) => Ok(zstd::stream::decode_all(&pt[1..])?),
        _ => bail!("неизвестный формат блока"),
    }
}

pub fn new_kdf_params() -> KdfParams {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    KdfParams { m_kib: 64 * 1024, t: 3, p: 1, salt: base64::engine::general_purpose::STANDARD.encode(salt) }
}

fn kek(password: &str, kdf: &KdfParams) -> Result<[u8; 32]> {
    let salt = base64::engine::general_purpose::STANDARD.decode(&kdf.salt)?;
    let params = argon2::Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32)).map_err(|e| anyhow!("Argon2: {e}"))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    a.hash_password_into(password.as_bytes(), &salt, &mut out).map_err(|e| anyhow!("Argon2: {e}"))?;
    Ok(out)
}

/// Зашифровать мастер-ключ паролем.
pub fn wrap_master(keys: &Keys, password: &str, kdf: &KdfParams, repo_id: &str) -> Result<String> {
    let k = kek(password, kdf)?;
    let blob = seal(&k, repo_id.as_bytes(), keys.master(), 0);
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// Расшифровать мастер-ключ паролем.
pub fn unwrap_master(wrapped: &str, password: &str, kdf: &KdfParams, repo_id: &str) -> Result<Keys> {
    let k = kek(password, kdf)?;
    let blob = base64::engine::general_purpose::STANDARD.decode(wrapped)?;
    let m = open(&k, repo_id.as_bytes(), &blob).map_err(|_| anyhow!("неверный пароль репозитория"))?;
    let m: [u8; 32] = m.try_into().map_err(|_| anyhow!("повреждённый мастер-ключ"))?;
    Ok(Keys::from_master(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_tamper() {
        let k = Keys::generate();
        let data = vec![7u8; 100_000];
        let b = seal(&k.data, b"id", &data, 3);
        assert!(b.len() < 2000, "повторяющиеся данные должны сжаться");
        assert_eq!(open(&k.data, b"id", &b).unwrap(), data);
        assert!(open(&k.data, b"other", &b).is_err(), "чужой AAD должен отвергаться");
        let mut t = b.clone();
        let n = t.len() - 1;
        t[n] ^= 1;
        assert!(open(&k.data, b"id", &t).is_err());
    }

    #[test]
    fn incompressible_stored_raw() {
        let k = Keys::generate();
        let mut data = vec![0u8; 50_000];
        OsRng.fill_bytes(&mut data);
        let b = seal(&k.data, b"x", &data, 3);
        assert_eq!(b.len(), data.len() + 1 + 24 + 16);
        assert_eq!(open(&k.data, b"x", &b).unwrap(), data);
    }

    #[test]
    fn wrap_unwrap() {
        let k = Keys::generate();
        let kdf = KdfParams { m_kib: 1024, t: 1, p: 1, salt: "c2FsdHNhbHRzYWx0".into() };
        let w = wrap_master(&k, "секрет", &kdf, "repo1").unwrap();
        let k2 = unwrap_master(&w, "секрет", &kdf, "repo1").unwrap();
        assert_eq!(k.data, k2.data);
        assert!(unwrap_master(&w, "не тот", &kdf, "repo1").is_err());
    }
}
