//! Хранилище репозитория: локальный каталог (на узле площадки) или узел площадки по HTTPS.
//! Агенты видят одинаковый интерфейс; удаление доступно только через политику хранения (`prune`),
//! которую узел выполняет по токену администратора.

use anyhow::Result;
use async_trait::async_trait;

use super::types::*;

#[async_trait]
pub trait Backend: Send + Sync {
    /// Конфигурация репозитория; `None`, если репозиторий ещё не создан.
    async fn read_config(&self) -> Result<Option<RepoConfig>>;
    /// Создать репозиторий. Ошибка `Conflict`, если он уже есть.
    async fn write_config(&self, cfg: &RepoConfig) -> Result<()>;
    /// Какие из блоков отсутствуют на площадке (один запрос на пакет идентификаторов, как have/want в git).
    async fn missing(&self, ids: Vec<ChunkId>) -> Result<Vec<ChunkId>>;
    /// Сохранить pack-файл и его индекс.
    async fn put_pack(&self, pack_id: &str, data: bytes::Bytes, entries: Vec<IndexEntry>) -> Result<()>;
    /// Зашифрованный блок по идентификатору.
    async fn get_blob(&self, id: &ChunkId) -> Result<Vec<u8>>;
    async fn put_snapshot(&self, rec: &SnapshotRecord) -> Result<()>;
    async fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord>;
    async fn list_snapshots(&self, instance: Option<&str>) -> Result<Vec<SnapshotHeader>>;
    /// Объект вне снапшотов (сегмент WAL и т. п.). Повторная запись того же содержимого допустима,
    /// другого — ошибка `Conflict`.
    async fn put_object(&self, ns: &str, name: &str, data: Vec<u8>) -> Result<()>;
    async fn get_object(&self, ns: &str, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list_objects(&self, ns: &str, prefix: &str) -> Result<Vec<ObjInfo>>;
    async fn stats(&self) -> Result<RepoStats>;
    async fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport>;
    // ---- репликация на принимающие площадки: данные копируются как есть (уже зашифрованы)
    /// Все pack-файлы.
    async fn list_packs(&self) -> Result<Vec<PackInfo>>;
    /// pack-файл целиком с записями индекса.
    async fn get_pack(&self, id: &str) -> Result<(Vec<IndexEntry>, bytes::Bytes)>;
    /// Все объекты вне снапшотов (WAL и т. п.) по всем пространствам.
    async fn list_all_objects(&self) -> Result<Vec<NsObj>>;
    /// Несколько блоков сразу, по порядку `ids`. Может вернуть меньше, чем запрошено (ограничение размера
    /// ответа) — остальные запрашиваются снова. По умолчанию — по одному, до 8 параллельно.
    async fn get_blobs(&self, ids: &[ChunkId]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(ids.len());
        for part in ids.chunks(8) { out.extend(futures::future::try_join_all(part.iter().map(|id| self.get_blob(id))).await?); }
        Ok(out)
    }
    /// Какие блоки лежат в pack-файле (без данных, если хранилище это умеет; иначе — из pack-файла целиком).
    async fn pack_index(&self, id: &str) -> Result<Vec<IndexEntry>> { Ok(self.get_pack(id).await?.0) }
    /// Запись закончена (снять блокировку записи, если она есть у хранилища).
    async fn release(&self) {}
}

/// Допустимое имя пространства объектов: латиница, цифры, `._-/`, без `..`.
pub fn valid_ns(ns: &str) -> bool {
    !ns.is_empty()
        && ns.len() <= 200
        && !ns.contains("..")
        && !ns.starts_with('/')
        && ns.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && n != "." && n != ".." && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}
