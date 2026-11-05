//! Параметры облачной площадки (Google Диск, Яндекс Диск, Nextcloud/WebDAV) и выбор клиента.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::objstore::ObjStore;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CloudAccess {
    /// `gdrive` | `yadisk` | `webdav` (Nextcloud, ownCloud, любой WebDAV).
    pub provider: String,
    /// WebDAV: адрес (Nextcloud: https://облако/remote.php/dav/files/<логин>).
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// WebDAV: пароль приложения.
    #[serde(default)]
    pub password: String,
    /// Папка площадки: Google — имя папки в корне; Яндекс — `app:/HopeBackup` или `disk:/…`; WebDAV — подпапка.
    #[serde(default)]
    pub folder: String,
    /// Google: идентификатор существующей папки (необязательно).
    #[serde(default)]
    pub folder_id: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Яндекс: токен OAuth (живёт год; при наличии токена обновления сервер продлевает его сам).
    #[serde(default)]
    pub access_token: String,
    /// Срок токена доступа (unix-время), 0 — неизвестен.
    #[serde(default)]
    pub token_expires: i64,
    /// Учётная запись (для панели): почта Google, логин Яндекса, логин Nextcloud.
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub fingerprint: String,
    /// Свой адрес API (имитаторы в тестах, прокси); пусто — адреса сервиса.
    #[serde(default)]
    pub endpoint: String,
    /// Только в запросе панели: результат входа OAuth, который сервер подставит в токены.
    #[serde(default, skip_serializing)]
    pub oauth_flow: String,
}

impl CloudAccess {
    pub fn title(&self) -> &'static str {
        match self.provider.as_str() { "gdrive" => "Google Диск", "yadisk" => "Яндекс Диск", _ => "WebDAV" }
    }

    pub fn url_label(&self) -> String {
        match self.provider.as_str() {
            "gdrive" => format!("gdrive:/{}", if self.folder.is_empty() { "HopeBackup" } else { &self.folder }),
            "yadisk" => if self.folder.is_empty() { "app:/HopeBackup".into() } else { self.folder.clone() },
            _ => format!("{}/{}", self.url.trim_end_matches('/'), self.folder.trim_matches('/')),
        }
    }

    pub fn store(&self) -> Result<Arc<dyn ObjStore>> {
        Ok(match self.provider.as_str() {
            "webdav" | "nextcloud" => {
                let folder = if self.folder.trim().is_empty() { "HopeBackup" } else { self.folder.as_str() };
                Arc::new(super::webdav::WebDav::new(&self.url, folder, &self.username, &self.password, &self.fingerprint)?)
            }
            "yadisk" => Arc::new(super::yadisk::YaDisk::new(&self.endpoint, &self.access_token, &self.folder)?),
            "gdrive" => Arc::new(super::gdrive::GDrive::new(&self.endpoint, &self.client_id, &self.client_secret, &self.refresh_token, &self.folder, &self.folder_id)?),
            p => bail!("неизвестное облако «{p}»"),
        })
    }
}
