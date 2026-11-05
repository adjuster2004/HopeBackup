//! Репозиторий на узле площадки, доступный по HTTPS (с закреплённым сертификатом и токеном).

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;

use super::backend::Backend;
use super::types::*;

#[derive(Clone)]
pub struct HttpBackend {
    client: reqwest::Client,
    base: String,
    token: String,
    /// Адрес узла (для /v1/health).
    root: String,
    /// Узел умеет пакетное чтение блоков и индекс pack-файла без данных (0.9.5+); выясняется один раз.
    batch: std::sync::Arc<tokio::sync::OnceCell<bool>>,
}

/// Адрес узла, имя репозитория, токен и отпечаток сертификата.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Remote {
    pub url: String,
    pub repo: String,
    pub token: String,
    #[serde(default)]
    pub fingerprint: String,
    /// Площадка в объектном хранилище S3 (Yandex Object Storage, AWS S3, MinIO…): `url` = `s3://бакет/префикс`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<super::s3::S3Access>,
    /// Облачный диск (Google, Яндекс, Nextcloud/WebDAV): `url` = `cloud://…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<crate::cloud::drive::CloudAccess>,
}

impl Remote {
    pub fn node(url: &str, repo: &str, token: &str, fingerprint: &str) -> Remote {
        Remote { url: url.to_string(), repo: repo.to_string(), token: token.to_string(), fingerprint: fingerprint.to_string(), s3: None, cloud: None }
    }
    /// Хранилище объектов (S3 или облачный диск), а не узел HopeBackup.
    pub fn is_s3(&self) -> bool { self.url.starts_with("s3://") || self.s3.is_some() || self.cloud.is_some() }
}

impl HttpBackend {
    pub fn new(r: &Remote) -> Result<HttpBackend> {
        let fp = if r.url.starts_with("https://") { Some(r.fingerprint.as_str()) } else { None };
        if r.url.starts_with("https://") && crate::tlsutil::normalize_fp(&r.fingerprint).is_empty() {
            bail!("для узла {} не задан отпечаток сертификата", r.url);
        }
        Ok(HttpBackend {
            client: crate::tlsutil::client(fp)?,
            base: format!("{}/v1/r/{}", r.url.trim_end_matches('/'), r.repo),
            token: r.token.clone(),
            root: r.url.trim_end_matches('/').to_string(),
            batch: Default::default(),
        })
    }

    /// Как со старым узлом: без пакетного чтения (для замеров).
    #[cfg(test)]
    pub fn without_batch(self) -> Self { let _ = self.batch.set(false); self }

    /// Версия узла не ниже 0.9.5: есть `/blobs` и `/pack/:id/index`. Старый узел — поблочно, как раньше.
    async fn has_batch(&self) -> bool {
        *self.batch.get_or_init(|| async {
            let v = async { self.client.get(format!("{}/v1/health", self.root)).timeout(std::time::Duration::from_secs(30)).send().await.ok()?.json::<serde_json::Value>().await.ok() }.await;
            let ver = v.as_ref().and_then(|v| v["version"].as_str()).unwrap_or("");
            let n: Vec<u64> = ver.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).take(3).filter_map(|x| x.parse().ok()).collect();
            let ok = n.len() == 3 && (n[0], n[1], n[2]) >= (0, 9, 5);
            if !ok { tracing::info!("узел {}: версия «{ver}» — пакетного чтения блоков нет, блоки читаются по одному", self.root); }
            ok
        }).await
    }

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(m, format!("{}{}", self.base, path)).bearer_auth(&self.token)
    }

    async fn check(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        let st = resp.status();
        if st.is_success() { return Ok(resp); }
        let body = resp.text().await.unwrap_or_default();
        let msg = if body.is_empty() { st.to_string() } else { body };
        match st {
            StatusCode::NOT_FOUND => Err(NotFound(format!("{what}: {msg}")).into()),
            StatusCode::CONFLICT => Err(Conflict(format!("{what}: {msg}")).into()),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(anyhow!("узел отказал в доступе ({what}): {msg}")),
            _ => Err(anyhow!("узел площадки: {what}: {msg}")),
        }
    }
}

/// Повтор при обрыве связи: медленные и нестабильные каналы — обычное дело.
async fn retry<T, F, Fut>(what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    // Обрыв связи с площадкой: ждём её возвращения до 10 минут (паузы 1, 2, 4… 30 с), затем — ошибка.
    let mut delay = 1u64;
    let started = std::time::Instant::now();
    let patience = crate::util::net_patience();
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let transient = e.downcast_ref::<reqwest::Error>().map(|r| r.is_connect() || r.is_timeout() || r.is_request() || r.is_body()).unwrap_or(false);
                if !transient { return Err(e); }
                if started.elapsed() + std::time::Duration::from_secs(delay) > patience {
                    return Err(e.context(format!("площадка недоступна дольше {} с ({what})", patience.as_secs())));
                }
                tracing::warn!("{what}: {e}; повтор через {delay} с (жду площадку до {} с)", patience.as_secs());
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                delay = (delay * 2).min(30);
            }
        }
    }
}

#[derive(Deserialize)]
struct MissingResp {
    missing: Vec<ChunkId>,
}

#[async_trait]
impl Backend for HttpBackend {
    async fn read_config(&self) -> Result<Option<RepoConfig>> {
        let r = retry("конфигурация", || async { Ok(self.req(reqwest::Method::GET, "/config").send().await?) }).await?;
        if r.status() == StatusCode::NOT_FOUND { return Ok(None); }
        Ok(Some(Self::check(r, "конфигурация").await?.json().await?))
    }

    async fn write_config(&self, cfg: &RepoConfig) -> Result<()> {
        let r = self.req(reqwest::Method::PUT, "/config").json(cfg).send().await?;
        Self::check(r, "создание репозитория").await?;
        Ok(())
    }

    async fn missing(&self, ids: Vec<ChunkId>) -> Result<Vec<ChunkId>> {
        let body = serde_json::json!({ "ids": ids });
        let r = retry("проверка блоков", || async { Ok(self.req(reqwest::Method::POST, "/missing").json(&body).send().await?) }).await?;
        Ok(Self::check(r, "проверка блоков").await?.json::<MissingResp>().await?.missing)
    }

    async fn put_pack(&self, pack_id: &str, data: bytes::Bytes, entries: Vec<IndexEntry>) -> Result<()> {
        let ej = serde_json::to_vec(&entries)?;
        let mut head = Vec::with_capacity(4 + ej.len());
        head.extend_from_slice(&(ej.len() as u32).to_be_bytes());
        head.extend_from_slice(&ej);
        let head = bytes::Bytes::from(head);
        let len = (head.len() + data.len()) as u64;
        // Заголовок и данные — двумя частями, без копии pack-файла в общий буфер.
        let r = retry("отправка pack-файла", || async {
            let parts = futures::stream::iter([Ok::<_, std::io::Error>(head.clone()), Ok(data.clone())]);
            Ok(self.req(reqwest::Method::PUT, &format!("/pack/{pack_id}")).timeout(std::time::Duration::from_secs(3600)).header(reqwest::header::CONTENT_LENGTH, len).body(reqwest::Body::wrap_stream(parts)).send().await?)
        })
        .await?;
        Self::check(r, "отправка pack-файла").await?;
        Ok(())
    }

    async fn get_blob(&self, id: &ChunkId) -> Result<Vec<u8>> {
        let r = retry("чтение блока", || async { Ok(self.req(reqwest::Method::GET, &format!("/blob/{}", id.to_hex())).send().await?) }).await?;
        Ok(Self::check(r, "чтение блока").await?.bytes().await?.to_vec())
    }

    async fn put_snapshot(&self, rec: &SnapshotRecord) -> Result<()> {
        let r = retry("запись снапшота", || async { Ok(self.req(reqwest::Method::PUT, &format!("/snapshot/{}", rec.header.id)).json(rec).send().await?) }).await?;
        Self::check(r, "запись снапшота").await?;
        Ok(())
    }

    async fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord> {
        let r = retry("чтение снапшота", || async { Ok(self.req(reqwest::Method::GET, &format!("/snapshot/{id}")).send().await?) }).await?;
        Ok(Self::check(r, "чтение снапшота").await?.json().await?)
    }

    async fn list_snapshots(&self, instance: Option<&str>) -> Result<Vec<SnapshotHeader>> {
        let mut q = self.req(reqwest::Method::GET, "/snapshots");
        if let Some(i) = instance { q = q.query(&[("instance", i)]); }
        let r = q.send().await?;
        Ok(Self::check(r, "список снапшотов").await?.json().await?)
    }

    async fn put_object(&self, ns: &str, name: &str, data: Vec<u8>) -> Result<()> {
        let body = bytes::Bytes::from(data);
        let r = retry("запись объекта", || async { Ok(self.req(reqwest::Method::PUT, &format!("/obj/{ns}/{name}")).body(body.clone()).send().await?) }).await?;
        Self::check(r, "запись объекта").await?;
        Ok(())
    }

    async fn get_object(&self, ns: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let r = retry("чтение объекта", || async { Ok(self.req(reqwest::Method::GET, &format!("/obj/{ns}/{name}")).send().await?) }).await?;
        if r.status() == StatusCode::NOT_FOUND { return Ok(None); }
        Ok(Some(Self::check(r, "чтение объекта").await?.bytes().await?.to_vec()))
    }

    async fn list_objects(&self, ns: &str, prefix: &str) -> Result<Vec<ObjInfo>> {
        let r = self.req(reqwest::Method::GET, "/objs").query(&[("ns", ns), ("prefix", prefix)]).send().await?;
        Ok(Self::check(r, "список объектов").await?.json().await?)
    }

    async fn stats(&self) -> Result<RepoStats> {
        let r = self.req(reqwest::Method::GET, "/stats").send().await?;
        Ok(Self::check(r, "статистика").await?.json().await?)
    }

    async fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport> {
        let r = self.req(reqwest::Method::POST, "/prune").json(policy).send().await?;
        Ok(Self::check(r, "очистка").await?.json().await?)
    }

    async fn list_packs(&self) -> Result<Vec<PackInfo>> {
        let r = retry("список pack-файлов", || async { Ok(self.req(reqwest::Method::GET, "/packs").send().await?) }).await?;
        Ok(Self::check(r, "список pack-файлов").await?.json().await?)
    }

    async fn get_pack(&self, id: &str) -> Result<(Vec<IndexEntry>, bytes::Bytes)> {
        let r = retry("чтение pack-файла", || async { Ok(self.req(reqwest::Method::GET, &format!("/pack/{id}")).timeout(std::time::Duration::from_secs(3600)).send().await?) }).await?;
        let b = Self::check(r, "чтение pack-файла").await?.bytes().await?;
        if b.len() < 4 { bail!("pack-файл {id}: пустой ответ"); }
        let n = u32::from_be_bytes(b[..4].try_into().unwrap()) as usize;
        if b.len() < 4 + n { bail!("pack-файл {id}: обрезанный ответ"); }
        let entries: Vec<IndexEntry> = serde_json::from_slice(&b[4..4 + n])?;
        Ok((entries, b.slice(4 + n..)))
    }

    async fn list_all_objects(&self) -> Result<Vec<NsObj>> {
        let r = retry("список объектов", || async { Ok(self.req(reqwest::Method::GET, "/allobjs").send().await?) }).await?;
        Ok(Self::check(r, "список объектов").await?.json().await?)
    }

    async fn get_blobs(&self, ids: &[ChunkId]) -> Result<Vec<Vec<u8>>> {
        if !self.has_batch().await {
            let mut out = Vec::with_capacity(ids.len());
            for part in ids.chunks(8) { out.extend(futures::future::try_join_all(part.iter().map(|id| self.get_blob(id))).await?); }
            return Ok(out);
        }
        let ids = &ids[..ids.len().min(BLOBS_MAX_IDS)];
        let body = serde_json::json!({ "ids": ids.iter().map(|x| x.to_hex()).collect::<Vec<_>>() });
        let r = retry("чтение блоков", || async { Ok(self.req(reqwest::Method::POST, "/blobs").json(&body).timeout(std::time::Duration::from_secs(1800)).send().await?) }).await?;
        let b = Self::check(r, "чтение блоков").await?.bytes().await?;
        let mut out = Vec::new();
        let mut p = 0usize;
        while p < b.len() {
            if p + 4 > b.len() { bail!("чтение блоков: обрезанный ответ"); }
            let n = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            if p + n > b.len() { bail!("чтение блоков: обрезанный ответ"); }
            out.push(b[p..p + n].to_vec());
            p += n;
        }
        if out.len() > ids.len() || (out.is_empty() && !ids.is_empty()) { bail!("чтение блоков: узел вернул {} из {}", out.len(), ids.len()); }
        Ok(out)
    }

    async fn pack_index(&self, id: &str) -> Result<Vec<IndexEntry>> {
        if !self.has_batch().await { return Ok(self.get_pack(id).await?.0); }
        let r = retry("индекс pack-файла", || async { Ok(self.req(reqwest::Method::GET, &format!("/pack/{id}/index")).send().await?) }).await?;
        Ok(Self::check(r, "индекс pack-файла").await?.json().await?)
    }
}
