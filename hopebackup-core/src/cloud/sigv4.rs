//! Подпись запросов AWS Signature Version 4 — для S3 (AWS, Yandex Object Storage, MinIO, Ceph RGW)
//! и API EC2.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn sha256_hex(b: &[u8]) -> String { hex::encode(Sha256::digest(b)) }

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("ключ HMAC");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// URI-кодирование по правилам SigV4: незарезервированные символы как есть, остальное — %XX.
/// `keep_slash` — для пути объекта («/» не кодируется).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') { o.push(b as char) } else { o += &format!("%{b:02X}") }
    }
    o
}

pub struct Creds<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// Подписать запрос. `path` — уже закодированный путь, `query` — пары без кодирования,
/// `headers` — заголовки без host и x-amz-*: они добавляются здесь. Возвращает все заголовки,
/// которые надо отправить (включая Authorization).
pub fn sign(c: &Creds, method: &str, host: &str, path: &str, query: &[(String, String)], headers: &[(String, String)], payload_sha256: &str, now: chrono::DateTime<chrono::Utc>) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let mut hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
    hs.push(("host".into(), host.to_string()));
    hs.push(("x-amz-date".into(), amz_date.clone()));
    hs.push(("x-amz-content-sha256".into(), payload_sha256.to_string()));
    hs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let cq = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let ch: String = hs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let sh = hs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let creq = format!("{method}\n{path}\n{cq}\n{ch}\n{sh}\n{payload_sha256}");
    let scope = format!("{date}/{}/{}/aws4_request", c.region, c.service);
    let sts = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(creq.as_bytes()));
    let k = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
    let k = hmac(&k, c.region.as_bytes());
    let k = hmac(&k, c.service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let sig = hex::encode(hmac(&k, sts.as_bytes()));
    let mut out: Vec<(String, String)> = hs.into_iter().filter(|(k, _)| k != "host").collect();
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={sh}, Signature={sig}", c.access_key)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пример из документации AWS («GET Object» с Range, раздел Signature Calculations for S3).
    #[test]
    fn aws_s3_get_object_example() {
        let c = Creds { access_key: "AKIAIOSFODNN7EXAMPLE", secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", region: "us-east-1", service: "s3" };
        let now = chrono::DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let h = sign(&c, "GET", "examplebucket.s3.amazonaws.com", "/test.txt", &[], &[("Range".into(), "bytes=0-9".into())], EMPTY_SHA256, now);
        let auth = h.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert_eq!(auth, "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    /// Пример «GET Bucket Lifecycle» / «List objects» с параметрами запроса.
    #[test]
    fn aws_s3_list_example() {
        let c = Creds { access_key: "AKIAIOSFODNN7EXAMPLE", secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", region: "us-east-1", service: "s3" };
        let now = chrono::DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let q = vec![("max-keys".to_string(), "2".to_string()), ("prefix".to_string(), "J".to_string())];
        let h = sign(&c, "GET", "examplebucket.s3.amazonaws.com", "/", &q, &[], EMPTY_SHA256, now);
        let auth = h.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(auth.ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{auth}");
    }
}
