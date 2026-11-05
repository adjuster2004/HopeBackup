//! Типы репозитория: идентификаторы блоков, записи индекса, снапшоты, конфигурация.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Идентификатор блока: BLAKE3 с ключом репозитория от открытого содержимого блока.
/// Ключ не даёт площадке проверить догадку «есть ли у вас такой файл».
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(pub [u8; 32]);

impl ChunkId {
    pub fn to_hex(&self) -> String { hex::encode(self.0) }
    pub fn from_hex(s: &str) -> anyhow::Result<Self> {
        let v = hex::decode(s)?;
        let a: [u8; 32] = v.try_into().map_err(|_| anyhow::anyhow!("идентификатор блока должен быть 32 байта"))?;
        Ok(ChunkId(a))
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", &self.to_hex()[..16]) }
}

impl Serialize for ChunkId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(&self.to_hex()) }
}

impl<'de> Deserialize<'de> for ChunkId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Без промежуточной строки: в снапшоте их сотни тысяч.
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = ChunkId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("идентификатор блока (64 шестнадцатеричных символа)") }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<ChunkId, E> {
                let mut a = [0u8; 32];
                hex::decode_to_slice(s, &mut a).map_err(|_| E::custom("идентификатор блока должен быть 32 байта в hex"))?;
                Ok(ChunkId(a))
            }
        }
        d.deserialize_str(V)
    }
}

/// Положение зашифрованного блока внутри pack-файла.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: ChunkId,
    pub offset: u32,
    pub len: u32,
}

/// Параметры нарезки по содержимому (FastCDC).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkerParams {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkerParams {
    fn default() -> Self { ChunkerParams { min: 256 * 1024, avg: 1024 * 1024, max: 4 * 1024 * 1024 } }
}

/// Параметры Argon2id для ключа, которым зашифрован мастер-ключ репозитория.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
}

/// Открытая конфигурация репозитория (файл `config.json`). Мастер-ключ в ней — только в зашифрованном виде.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoConfig {
    pub version: u32,
    pub id: String,
    pub created: DateTime<Utc>,
    pub chunker: ChunkerParams,
    pub kdf: KdfParams,
    /// Мастер-ключ, зашифрованный ключом из пароля (base64: nonce + шифротекст).
    pub master_key: String,
}

/// Открытый заголовок снапшота: по нему площадка применяет политику хранения, не зная содержимого.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub id: String,
    pub time: DateTime<Utc>,
    /// Экземпляр продукта (например, `pg-db-prod-01`).
    pub instance: String,
    /// Плагин (`postgresql`, `files`, …).
    pub product: String,
    /// Вид копии: `full`, `dump`, `files`, …
    pub kind: String,
    pub host: String,
    /// Открытый объём данных источника, байт.
    pub size: u64,
    /// Сколько данных оказалось новыми (не найдено на площадке), байт до сжатия.
    pub new_bytes: u64,
    /// Сколько байт ушло по каналу (после сжатия и шифрования).
    pub uploaded: u64,
    pub chunks: u64,
    /// Открытые служебные поля: начало журнала WAL, версия СУБД — нужны площадке для очистки.
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Снапшот целиком, как он хранится на площадке.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRecord {
    #[serde(flatten)]
    pub header: SnapshotHeader,
    /// Уникальные блоки, на которые ссылается снапшот (для сборки мусора на площадке).
    pub refs: Vec<ChunkId>,
    /// Зашифрованное тело (`Manifest`), base64.
    pub body: String,
}

/// Поток данных внутри снапшота (tar-архив, дамп базы и т. п.).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamEntry {
    pub name: String,
    pub size: u64,
    pub chunks: Vec<ChunkId>,
}

/// Образ диска: последовательность блоков фиксированного размера. В отличие от `StreamEntry`
/// (нарезка по содержимому), здесь блок адресуется номером, а карта блоков (`map`) — это поток
/// с конкатенацией 32-байтных идентификаторов (нулевой идентификатор — нулевой блок, он не хранится).
/// Так образ восстанавливается чтением карты и блоков по порядку, а неизменные блоки каждой следующей
/// копии берутся из прошлой копии без повторной передачи (см. `repo::Block::Ref`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageEntry {
    /// Имя диска в образе (например, `sda`).
    pub name: String,
    /// Размер диска, байт.
    pub size: u64,
    /// Размер блока, байт.
    pub block: u32,
    /// Число блоков.
    pub blocks: u64,
    /// Поток с картой блоков: `blocks` записей по 32 байта (идентификатор блока или нули).
    pub map: StreamEntry,
}

/// Зашифрованная часть снапшота.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub streams: Vec<StreamEntry>,
    /// Метаданные плагина: версия продукта, параметры initdb, расширения и т. п.
    #[serde(default)]
    pub meta: serde_json::Value,
    /// Образы дисков (продукты «Образ машины по SSH» и «Образ ВМ»).
    #[serde(default)]
    pub images: Vec<ImageEntry>,
}

/// Объект вне снапшотов (сегменты WAL и т. п.).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjInfo {
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

/// pack-файл площадки (для репликации).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PackInfo {
    pub id: String,
    pub size: u64,
}

/// Объект с пространством имён (для репликации).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NsObj {
    pub ns: String,
    #[serde(flatten)]
    pub obj: ObjInfo,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoStats {
    pub packs: u64,
    pub pack_bytes: u64,
    pub chunks: u64,
    pub snapshots: u64,
    pub objects: u64,
    pub object_bytes: u64,
}

/// Политика хранения, применяемая площадкой.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrunePolicy {
    /// Сколько последних копий каждого вида хранить для каждого экземпляра.
    pub keep_last: usize,
    /// Дополнительно хранить все копии моложе N дней.
    #[serde(default)]
    pub keep_days: Option<u32>,
    /// Только посчитать, ничего не удалять.
    #[serde(default)]
    pub dry_run: bool,
    /// Своё число копий для отдельных экземпляров (у каждого экземпляра своя политика).
    #[serde(default)]
    pub per_instance: std::collections::HashMap<String, usize>,
    /// Не удалять снапшоты экземпляров, которых нет в `per_instance` (чужие или удалённые из каталога).
    #[serde(default)]
    pub keep_unknown: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PruneReport {
    pub snapshots_removed: Vec<String>,
    pub packs_removed: u64,
    pub bytes_freed: u64,
    pub packs_partial: u64,
    pub wal_removed: u64,
    pub dry_run: bool,
}

/// Ошибка «не найдено» — превращается в HTTP 404 и обратно.
#[derive(Debug)]
pub struct NotFound(pub String);

/// Пакетное чтение блоков (`/blobs` узла): до стольких блоков в запросе и примерно до стольких байт в ответе.
pub const BLOBS_MAX_IDS: usize = 256;
pub const BLOBS_MAX_BYTES: usize = 32 * 1024 * 1024;
impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "не найдено: {}", self.0) }
}
impl std::error::Error for NotFound {}

/// Ошибка «уже существует с другим содержимым» — репозиторий только дописывается.
#[derive(Debug)]
pub struct Conflict(pub String);
impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "уже существует: {}", self.0) }
}
impl std::error::Error for Conflict {}
