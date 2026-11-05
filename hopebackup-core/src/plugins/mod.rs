//! Плагины продуктов. Ядро знает только контракт: обнаружить, снять копию, восстановить.
//! В MVP плагины встроены в бинарник; внешние плагины (отдельные процессы) — следующий этап.

pub mod appkit;
pub mod clickhouse;
pub mod docker;
pub mod files;
pub mod gitlab;
pub mod hope;
pub mod host;
pub mod image;
pub mod jenkins;
pub mod keycloak;
pub mod kube;
pub mod ldap;
pub mod monitoring;
pub mod mongodb;
pub mod mssql;
pub mod mysql;
pub mod nexus;
pub mod opensearch;
pub mod postgres;
pub mod redis;
pub mod migrate;
pub mod replica;
pub mod replica_vs;
pub mod vmdk;
pub mod s3bucket;
pub mod tarsniff;
pub mod vault;
pub mod vm;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::store::repo::{Repo, WriteStats};
use crate::store::types::SnapshotHeader;

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

pub fn stdout_log() -> LogFn { Arc::new(|s: &str| println!("{s}")) }

pub struct BackupCtx {
    pub repo: Arc<Repo>,
    pub instance: String,
    pub config: Value,
    pub kind: String,
    pub log: LogFn,
    /// Задание (для имён снимков ФС; пусто — запуск из командной строки).
    pub job_id: String,
}

impl BackupCtx {
    /// Метка для имён временных снимков ФС.
    pub fn job_tag(&self) -> String { if self.job_id.is_empty() { crate::util::short_id() } else { self.job_id.clone() } }
}

pub struct BackupOutcome {
    pub header: SnapshotHeader,
    pub stats: WriteStats,
}

pub struct RestoreCtx {
    pub repo: Arc<Repo>,
    pub instance: String,
    pub snapshot_id: String,
    pub params: Value,
    pub log: LogFn,
    pub job_id: String,
    /// Доступ к площадке — для restore_command PostgreSQL, который сам дозапрашивает сегменты WAL.
    pub remote: crate::store::Remote,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self) -> &'static str;
    /// Что есть на хосте: версии, пути, состояние архивации.
    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value>;
    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome>;
    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value>;
    /// Дополнительные действия плагина (например, настройка архивации WAL).
    async fn action(&self, name: &str, cfg: &Value, instance: &str, log: &LogFn) -> Result<Value> {
        let _ = (cfg, instance, log);
        anyhow::bail!("действие «{name}» не поддерживается")
    }
}

pub fn get(id: &str) -> Option<Arc<dyn Plugin>> {
    match id {
        "postgresql" => Some(Arc::new(postgres::Postgres)),
        "files" => Some(Arc::new(files::Files)),
        "gitlab" => Some(Arc::new(gitlab::Gitlab)),
        "vault" => Some(Arc::new(vault::Vault)),
        "s3" => Some(Arc::new(s3bucket::S3Bucket)),
        "host" => Some(Arc::new(host::Host)),
        "hopemail" => Some(Arc::new(hope::HopeApp(hope::Kind::Mail))),
        "hopegw" => Some(Arc::new(hope::HopeApp(hope::Kind::Gateway))),
        "hopeinv" => Some(Arc::new(hope::HopeApp(hope::Kind::Inventory))),
        "hopedfs" => Some(Arc::new(hope::HopeApp(hope::Kind::Dfs))),
        "hopedc" => Some(Arc::new(hope::HopeApp(hope::Kind::Dc))),
        "mysql" => Some(Arc::new(mysql::Mysql)),
        "redis" => Some(Arc::new(redis::Redis)),
        "mssql" => Some(Arc::new(mssql::Mssql)),
        "mongodb" => Some(Arc::new(mongodb::Mongo)),
        "clickhouse" => Some(Arc::new(clickhouse::ClickHouse)),
        "keycloak" => Some(Arc::new(keycloak::Keycloak)),
        "freeipa" => Some(Arc::new(ldap::Ldap)),
        "docker" => Some(Arc::new(docker::Docker)),
        "kubernetes" => Some(Arc::new(kube::Kube)),
        "jenkins" => Some(Arc::new(jenkins::Jenkins)),
        "nexus" => Some(Arc::new(nexus::Nexus)),
        "zabbix" => Some(Arc::new(monitoring::Monitoring)),
        "opensearch" => Some(Arc::new(opensearch::OpenSearch)),
        "vm" => Some(Arc::new(vm::VmImage)),
        "replica" => Some(Arc::new(replica::Replica)),
        "machine" => Some(Arc::new(image::Machine)),
        "qcow2images" => Some(Arc::new(image::formats::Qcow2Product)),
        _ => None,
    }
}

/// Параметры экземпляра, которые восстановление берёт по умолчанию (если не заданы в мастере).
pub fn restore_defaults(plugin: &str) -> &'static [&'static str] {
    match plugin {
        "s3" => &["endpoint", "region", "access_key", "secret_key", "path_style", "fingerprint"],
        "vault" => &["addr", "token", "namespace", "fingerprint"],
        "gitlab" => &["mode", "container", "backup_dir", "keep_local"],
        "host" => &[],
        "hopedc" => &["key_password"],
        "hopemail" | "hopegw" | "hopeinv" | "hopedfs" => &[],
        "mysql" => mysql::restore_keys(),
        "redis" => redis::restore_keys(),
        "mssql" => mssql::restore_keys(),
        "mongodb" => mongodb::restore_keys(),
        "clickhouse" => clickhouse::restore_keys(),
        "kubernetes" => kube::restore_keys(),
        "zabbix" => monitoring::restore_keys(),
        "opensearch" => opensearch::restore_keys(),
        "vm" => vm::restore_keys(),
        "machine" => image::restore_keys(),
        "qcow2images" => &[],
        "replica" => &[],
        "keycloak" => &["service", "container", "home"],
        "freeipa" => &["mode", "config_dir", "service"],
        "docker" | "jenkins" | "nexus" => &[],
        _ => &["verify_cmd", "reload_cmd", "os_user"],
    }
}

/// Запись каталога продуктов: по ней панель строит список «Добавить продукт» и форму параметров.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub plugin: &'static str,
    pub title: &'static str,
    pub tile: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    pub modes: &'static str,
    pub available: bool,
    pub schema: Value,
    pub defaults: Value,
    /// Параметры восстановления (форма мастера); пусто — у плагина своя форма в панели.
    pub restore_schema: Value,
    /// Виды копий помимо полной: [{id, title, hint}] — кнопки и расписание «Журнал/инкремент».
    pub kinds: Value,
    /// Восстановление на момент времени (по журналу).
    pub pitr: bool,
}

impl CatalogEntry {
    pub fn kinds(mut self, k: Value) -> Self { self.kinds = k; self }
    pub fn pitr(mut self) -> Self { self.pitr = true; self }
}

/// Запись каталога для плагина со своими схемами.
#[allow(clippy::too_many_arguments)]
fn entry(id: &'static str, plugin: &'static str, title: &'static str, tile: &'static str, category: &'static str, description: &'static str, modes: &'static str, schema: Value, defaults: Value, restore_schema: Value) -> CatalogEntry {
    CatalogEntry { id, plugin, title, tile, category, description, modes, available: true, schema, defaults, restore_schema, kinds: json!([]), pitr: false }
}


pub fn catalog() -> Vec<CatalogEntry> {
    let files_schema = files::schema();
    let preset = |id: &'static str, title: &'static str, tile: &'static str, desc: &'static str| CatalogEntry {
        id,
        plugin: "files",
        title,
        tile,
        category: "Инфраструктура",
        description: desc,
        modes: "файлы · дедупликация",
        available: true,
        schema: files_schema.clone(),
        defaults: files::preset(id),
        restore_schema: json!({}),
        kinds: json!([]),
        pitr: false,
    };
    let hope = |k: hope::Kind, title: &'static str, tile: &'static str, desc: &'static str, modes: &'static str| CatalogEntry {
        id: k.id(), plugin: k.id(), title, tile, category: "Продукты Hope", description: desc, modes, available: true,
        schema: hope::schema(k), defaults: hope::defaults(k), restore_schema: hope::restore_schema(k), kinds: json!([]), pitr: false,
    };
    vec![
        hope(hope::Kind::Mail, "Hope Mail Server", "HM", "Почтовый сервер: WAL и настройки, тела писем, TLS, программа; снимок ФС или короткая остановка", "полная · дедупликация"),
        hope(hope::Kind::Gateway, "Hope MSG Gateway", "GW", "Почтовый шлюз DMZ: состояние, очередь, карантин, мастер-ключ, keepalived", "полная · дедупликация"),
        hope(hope::Kind::Inventory, "HopeInventory", "HI", "Инвентаризация: настройки, база redb, сертификат, журнал аудита", "полная"),
        hope(hope::Kind::Dfs, "HopeDFS", "DFS", "Файловый сервер: шары с NTFS-правами (xattr), конфигурация узла и кластера", "файлы · xattr · снимок ФС"),
        hope(hope::Kind::Dc, "HopeDC", "DC", "Контроллер домена: база (ewal), SYSVOL, PKI, кластер; экспорт мастер-ключа с паролем", "полная · ключ отдельно"),
        CatalogEntry {
            id: "postgresql",
            plugin: "postgresql",
            title: "PostgreSQL 14+",
            tile: "PG",
            category: "СУБД",
            description: "Кластер, архив WAL, восстановление на момент времени",
            modes: "полная · архив WAL · pg_dump",
            available: true,
            schema: postgres::schema(),
            defaults: json!({ "port": 5432, "user": "postgres", "os_user": "postgres", "host": "/var/run/postgresql" }),
            restore_schema: json!({}),
            kinds: json!([]),
            pitr: false,
        },
        CatalogEntry {
            id: "qdatabase",
            plugin: "postgresql",
            title: "qdatabase",
            tile: "qd",
            category: "СУБД",
            description: "Форк PostgreSQL 15 (/opt/qdatabase-15.0)",
            modes: "полная · архив WAL · pg_dump",
            available: true,
            schema: postgres::schema(),
            defaults: json!({ "bin_dir": "/opt/qdatabase-15.0/bin", "port": 5432, "user": "postgres", "os_user": "postgres", "host": "/var/run/postgresql" }),
            restore_schema: json!({}),
            kinds: json!([]),
            pitr: false,
        },
        entry("mysql", "mysql", "MySQL / MariaDB", "My", "СУБД", "Дамп или mariabackup/xtrabackup, архив binlog, восстановление на момент времени", "дамп · физическая · binlog", mysql::schema(), mysql::defaults(), mysql::restore_schema())
            .kinds(json!([{ "id": "incr", "title": "Журнал binlog", "hint": "закрыть текущий binlog и забрать новые файлы" }])).pitr(),
        entry("mssql", "mssql", "MS SQL Server", "MS", "СУБД", "BACKUP DATABASE / LOG через sqlcmd: полная, разностная, журнал транзакций; цепочка и STOPAT при восстановлении", "полная · разностная · журнал", mssql::schema(), mssql::defaults(), mssql::restore_schema())
            .kinds(json!([{ "id": "incr", "title": "Журнал транзакций", "hint": "BACKUP LOG (у баз в модели SIMPLE — разностная)" }])).pitr(),
        entry("mongodb", "mongodb", "MongoDB", "Mo", "СУБД", "mongodump --archive (у набора реплик — срез oplog) прямо в репозиторий; mongorestore всего или одной базы", "полная · oplog", mongodb::schema(), mongodb::defaults(), mongodb::restore_schema()),
        entry("redis", "redis", "Redis / Valkey", "Rd", "СУБД", "Снимок RDB по протоколу репликации без остановки, redis.conf и ACL; восстановление и при включённом AOF", "снимок RDB", redis::schema(), redis::defaults(), redis::restore_schema()),
        entry("clickhouse", "clickhouse", "ClickHouse", "CH", "СУБД", "BACKUP на диск копий или схема и данные в формате Native; RESTORE и восстановление под другим именем", "полная · дедупликация", clickhouse::schema(), clickhouse::defaults(), clickhouse::restore_schema()),
        preset("nginx", "nginx", "ng", "Конфигурация, сертификаты; проверка nginx -t"),
        preset("haproxy", "HAProxy", "HA", "haproxy.cfg, maps, сертификаты; проверка haproxy -c"),
        preset("openvpn", "OpenVPN", "VPN", "Конфиги, PKI (включая ключ CA), ccd, CRL"),
        CatalogEntry { id: "vault", plugin: "vault", title: "Vault / OpenBao", tile: "Va", category: "Инфраструктура", description: "Снапшот raft через API, конфигурация /etc/vault.d", modes: "снапшот raft", available: true, schema: vault::schema(), defaults: vault::defaults(), restore_schema: vault::restore_schema(), kinds: json!([]), pitr: false },
        entry("keycloak", "keycloak", "Keycloak", "Kc", "Инфраструктура", "Выгрузка realm с пользователями (kc.sh export), conf/providers/themes; база — плагином СУБД", "realm · конфигурация", keycloak::schema(), keycloak::defaults(), keycloak::restore_schema()),
        entry("freeipa", "freeipa", "FreeIPA / LDAP", "IPA", "Инфраструктура", "OpenLDAP: slapcat cn=config и баз, slapadd при восстановлении; FreeIPA: ipa-backup / ipa-restore", "LDIF · ipa-backup", ldap::schema(), ldap::defaults(), ldap::restore_schema()),
        CatalogEntry { id: "gitlab", plugin: "gitlab", title: "GitLab", tile: "GL", category: "Платформы", description: "gitlab-backup + gitlab.rb и секреты; Omnibus или Docker", modes: "полная · дедупликация репозиториев", available: true, schema: gitlab::schema(), defaults: gitlab::defaults(), restore_schema: gitlab::restore_schema(), kinds: json!([]), pitr: false },
        entry("docker", "docker", "Docker", "Dk", "Платформы", "Тома (с паузой контейнеров), проекты compose, описание контейнеров, образы", "тома · дедупликация", docker::schema(), docker::defaults(), docker::restore_schema()),
        entry("kubernetes", "kubernetes", "minikube / Kubernetes", "K8s", "Платформы", "Объекты API по типам, снимок etcd, тома hostPath/local; применение по порядку зависимостей", "ресурсы · etcd · тома", kube::schema(), kube::defaults(), kube::restore_schema()),
        entry("jenkins", "jenkins", "Jenkins", "Jk", "Платформы", "JENKINS_HOME без рабочих каталогов и кэшей: задания, секреты, плагины, история сборок", "файлы · дедупликация", jenkins::schema(), jenkins::defaults(), jenkins::restore_schema()),
        entry("nexus", "nexus", "Nexus / Harbor", "Nx", "Платформы", "Nexus: БД и настройки согласованно, затем blob; Harbor: pg_dumpall, ключи, хранилище образов", "БД · blob · дедупликация", nexus::schema(), nexus::defaults(), nexus::restore_schema()),
        entry("zabbix", "zabbix", "Zabbix / Grafana", "Zb", "Платформы", "Zabbix: дамп базы (по желанию без истории), конфигурация и скрипты; Grafana: grafana.db, плагины, provisioning", "БД · конфигурация", monitoring::schema(), monitoring::defaults(), monitoring::restore_schema()),
        CatalogEntry {
            id: "host",
            plugin: "host",
            title: "Сервер Linux целиком",
            tile: "Lx",
            category: "Хранилища и машины",
            description: "Опись процессов, скриптов, cron и служб; пользователи, пароли и ключи SSH; /etc, приложения, диски",
            modes: "опись · файлы · дедупликация",
            available: true,
            schema: host::schema(),
            defaults: host::defaults(),
            restore_schema: host::restore_schema(),
            kinds: json!([]),
            pitr: false,
        },
        CatalogEntry {
            id: "files",
            plugin: "files",
            title: "Файлы и каталоги",
            tile: "Fs",
            category: "Хранилища и машины",
            description: "Любые пути с исключениями",
            modes: "файлы · дедупликация",
            available: true,
            schema: files_schema.clone(),
            defaults: json!({ "preset": "custom", "paths": [] }),
            restore_schema: json!({}),
            kinds: json!([]),
            pitr: false,
        },
        CatalogEntry { id: "s3", plugin: "s3", title: "S3-хранилище", tile: "S3", category: "Хранилища и машины", description: "Бакеты MinIO, Ceph, Yandex, AWS: объекты и метаданные", modes: "объекты · дедупликация", available: true, schema: s3bucket::schema(), defaults: s3bucket::defaults(), restore_schema: s3bucket::restore_schema(), kinds: json!([]), pitr: false },
        entry("opensearch", "opensearch", "Elasticsearch / OpenSearch", "ES", "Хранилища и машины", "Snapshot API: снимок в репозиторий fs и его каталог в копию; или свой репозиторий кластера (S3)", "снимок · дедупликация", opensearch::schema(), opensearch::defaults(), opensearch::restore_schema()),
        entry("replica", "replica", "Реплика машины (переезд)", "MV", "Переезд", "Блочная реплика дисков Linux-машины для переноса на Proxmox VE, zVirt/oVirt: синхронизации изменений, тест, переключение. Создаётся на странице «Переезд»", "блоки · изменения", replica::schema(), replica::defaults(), replica::restore_schema()),
        entry("vm", "vm", "Образ ВМ целиком", "VM", "Хранилища и машины", "libvirt/KVM, Proxmox VE, oVirt/zVirt/РОСА, OpenNebula/Брест, OpenStack, vSphere (CBT), XCP-ng, Nutanix, SpaceVM, VMmanager: снимок без остановки, диски потоком", "диски · дедупликация блоков", vm::schema(), vm::defaults(), vm::restore_schema()),
        entry("machine", "machine", "Образ машины по SSH", "IMG", "Хранилища и машины", "Образ дисков физической или виртуальной машины по SSH без агента: снимки LVM для согласованности, блоки по 4 МиБ с дедупликацией; развёртывание ВМ на Proxmox/libvirt/oVirt/OpenStack или выгрузка qcow2/vmdk/vhd/raw", "диски · снимок LVM · дедупликация", image::schema(), image::defaults(), image::restore_schema()),
        entry("qcow2images", "qcow2images", "Образы qcow2 (доп. канал)", "QC", "Хранилища и машины", "Берёт последнюю копию продукта «Образ машины по SSH» и собирает готовые файлы .qcow2 (или vmdk/vhd/raw) в каталог по своему расписанию; хранит последние N наборов, проверяет свободное место до создания", "файлы образов · расписание", image::formats::qcow2_schema(), image::formats::qcow2_defaults(), json!({})),
    ]
}

/// Значение строки из конфигурации плагина.
pub fn cfg_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
