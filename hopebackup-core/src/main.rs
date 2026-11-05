//! HopeBackup — резервное копирование инфраструктуры на удалённые площадки и восстановление.
//!
//! Один бинарник, роль задаётся командой:
//! `server` — сервер управления с панелью; `node` — узел хранения на площадке; `agent` — агент на хосте
//! с продуктом; `wal-push` / `wal-fetch` — команды для archive_command / restore_command PostgreSQL;
//! `restore-exec` — исполнитель восстановления, который сервер запускает на цели по SSH.

pub mod agent;
pub mod cloud;
pub mod dist;
pub mod connectors;
pub mod exclude;
pub mod exec;
pub mod fssnap;
pub mod node;
pub mod plugins;
pub mod proto;
pub mod server;
pub mod sshc;
pub mod store;
pub mod tlsutil;
pub mod updater;
pub mod util;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "hopebackup", version = concat!(env!("CARGO_PKG_VERSION"), " (сборка ", env!("HOPEBACKUP_BUILD_TIME"), ")"), about = "HopeBackup — резервное копирование и восстановление инфраструктуры")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Сервер управления: панель https://сервер:4443/, API, расписания (несколько адресов — через запятую)
    Server {
        #[arg(long, default_value = "0.0.0.0:4443", env = "HOPEBACKUP_LISTEN")]
        listen: String,
    },
    /// Узел хранения на площадке
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// Агент на хосте с продуктом
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// archive_command: положить сегмент WAL в спул агента
    WalPush {
        #[arg(long)]
        spool: PathBuf,
        path: PathBuf,
    },
    /// restore_command: получить сегмент WAL с площадки
    WalFetch {
        #[arg(long)]
        job: PathBuf,
        name: String,
        dest: PathBuf,
    },
    /// Выполнить задание из stdin (запускается сервером на цели восстановления по SSH)
    RestoreExec,
    /// Прочитать диски этой машины блоками и выдать кадры образа в stdout (запускается сервером по SSH; вручную не нужен)
    #[command(hide = true)]
    DiskRead {
        /// Размер блока, байт
        #[arg(long, default_value_t = 4 * 1024 * 1024)]
        block: u32,
        /// Диски через запятую (/dev/sda,/dev/sdb); пусто — все диски
        #[arg(long, default_value = "")]
        disks: String,
        /// Снимки LVM: auto | required | off
        #[arg(long, default_value = "auto")]
        lvm: String,
        /// Замораживать небольшие ФС без LVM на время чтения
        #[arg(long)]
        freeze_small: bool,
        /// Предел заморозки, с
        #[arg(long, default_value_t = 600)]
        freeze_timeout: u64,
    },
    /// Пара ключей WireGuard: «закрытый открытый»
    WgGenkey,
    /// Опись этого сервера (процессы, скрипты, cron, службы, монтирования, пользователи) в JSON
    Inventory {
        /// Параметры плагина «Сервер Linux целиком» (JSON)
        #[arg(long)]
        config: Option<String>,
    },
    /// Действие плагина на этой машине (например, `--plugin replica --name prepare-kvm`)
    Action {
        #[arg(long)]
        plugin: String,
        #[arg(long)]
        name: String,
        /// Параметры плагина (JSON)
        #[arg(long)]
        config: Option<String>,
        #[arg(long, default_value = "")]
        instance: String,
    },
    /// Установщик обновления (запускает служба из раздела «Обновления»; вручную не нужен)
    #[command(hide = true)]
    ApplyUpdate {
        request: String,
    },
    /// Убрать VPN-интерфейсы и IPsec-соединения завершившихся процессов (ExecStopPost служб)
    VpnCleanup,
    /// Работа с репозиторием напрямую (без сервера): создание, список, копия каталога, восстановление
    Repo {
        #[command(flatten)]
        remote: RemoteArgs,
        #[command(subcommand)]
        cmd: RepoCmd,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Создать конфигурацию узла: токены, сертификат
    Init {
        #[arg(long, default_value = "/var/lib/hopebackup-node", env = "HOPEBACKUP_NODE_DATA")]
        data: PathBuf,
        #[arg(long, default_value = "0.0.0.0:8443")]
        listen: String,
    },
    /// Запустить узел
    Run {
        #[arg(long, default_value = "/var/lib/hopebackup-node", env = "HOPEBACKUP_NODE_DATA")]
        data: PathBuf,
        /// Без TLS (только для стенда)
        #[arg(long)]
        plain: bool,
    },
    /// Выпустить токен доступа
    Token {
        #[arg(long, default_value = "/var/lib/hopebackup-node", env = "HOPEBACKUP_NODE_DATA")]
        data: PathBuf,
        #[arg(long)]
        name: String,
        /// read | append | admin
        #[arg(long, default_value = "append")]
        role: String,
        #[arg(long, default_value = "")]
        repo: String,
    },
    /// Показать отпечаток сертификата узла
    Fingerprint {
        #[arg(long, default_value = "/var/lib/hopebackup-node", env = "HOPEBACKUP_NODE_DATA")]
        data: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Подключить агента к серверу управления
    Enroll {
        #[arg(long)]
        server: String,
        // Токены, выданные до 0.8.3, могли начинаться с «-».
        #[arg(long, required_unless_present = "token_file", allow_hyphen_values = true)]
        token: Option<String>,
        /// Файл с токеном (вместо --token: токен не виден в списке процессов); удаляется после чтения
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,
        #[arg(long)]
        fingerprint: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Запустить агента
    Run,
}

#[derive(clap::Args)]
struct RemoteArgs {
    /// Адрес узла (https://узел:8443), бакет S3 (s3://бакет/префикс), облако (cloud://) или локальный каталог
    #[arg(long, env = "HOPEBACKUP_URL")]
    url: String,
    /// Облачный диск: JSON с параметрами (provider gdrive|yadisk|webdav, url, username, password, folder,
    /// client_id, client_secret, refresh_token, access_token) или @файл.json
    #[arg(long, env = "HOPEBACKUP_CLOUD", default_value = "", hide_env_values = true)]
    cloud: String,
    /// S3: адрес хранилища (https://storage.yandexcloud.net, https://s3.eu-central-1.amazonaws.com, http://minio:9000)
    #[arg(long, env = "HOPEBACKUP_S3_ENDPOINT", default_value = "")]
    s3_endpoint: String,
    #[arg(long, env = "HOPEBACKUP_S3_REGION", default_value = "")]
    s3_region: String,
    #[arg(long, env = "HOPEBACKUP_S3_ACCESS_KEY", default_value = "")]
    s3_access_key: String,
    #[arg(long, env = "HOPEBACKUP_S3_SECRET_KEY", default_value = "", hide_env_values = true)]
    s3_secret_key: String,
    /// S3: адресация по пути (MinIO, Ceph)
    #[arg(long, env = "HOPEBACKUP_S3_PATH_STYLE")]
    s3_path_style: bool,
    /// S3: Object Lock (GOVERNANCE | COMPLIANCE) и срок, дней
    #[arg(long, default_value = "")]
    s3_lock_mode: String,
    #[arg(long, default_value_t = 0)]
    s3_lock_days: u32,
    #[arg(long, default_value = "main", env = "HOPEBACKUP_REPO")]
    repo: String,
    #[arg(long, default_value = "", env = "HOPEBACKUP_TOKEN", allow_hyphen_values = true)]
    token: String,
    #[arg(long, default_value = "", env = "HOPEBACKUP_FINGERPRINT")]
    fingerprint: String,
    /// Пароль репозитория (лучше через переменную окружения HOPEBACKUP_PASSWORD)
    #[arg(long, env = "HOPEBACKUP_PASSWORD", hide_env_values = true, allow_hyphen_values = true)]
    password: String,
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Создать репозиторий
    Init,
    /// Список снапшотов
    List {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Копия: файлы и каталоги (по умолчанию) или продукт плагином (--plugin gitlab|vault|s3|postgresql)
    Backup {
        #[arg(long)]
        instance: String,
        /// Плагин продукта (по умолчанию files)
        #[arg(long, default_value = "files")]
        plugin: String,
        /// Параметры плагина в JSON (как у экземпляра в панели)
        #[arg(long)]
        config: Option<String>,
        /// Вид копии: full, dump, files
        #[arg(long)]
        kind: Option<String>,
        paths: Vec<String>,
    },
    /// Восстановить снапшот: файлы — в каталог (--dest), продукт — плагином с параметрами (--params)
    Restore {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        snapshot: String,
        #[arg(long, default_value = "")]
        dest: String,
        #[arg(long, default_value = "files")]
        plugin: String,
        /// Параметры восстановления в JSON
        #[arg(long)]
        params: Option<String>,
    },
    /// Состав снапшота: потоки, размер, сколько блоков новых относительно предыдущего снапшота экземпляра
    Show {
        snapshot: String,
    },
    /// Статистика репозитория
    Stats,
    /// Очистка по политике хранения (нужен токен администратора узла или ключ S3 с правом удаления)
    Prune {
        /// Сколько последних копий каждого вида хранить для каждого экземпляра
        #[arg(long, default_value_t = 7)]
        keep_last: usize,
        #[arg(long)]
        keep_days: Option<u32>,
        /// Только показать, что будет удалено
        #[arg(long)]
        dry_run: bool,
    },
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("HOPEBACKUP_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("hopebackup=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_writer(std::io::stderr).init();
}

/// Паника println! из-за закрытой читателем трубы stdout.
fn stdout_closed(panic_msg: &str) -> bool {
    panic_msg.starts_with("failed printing to stdout") && panic_msg.contains("os error 32")
}

fn main() {
    util::tune_allocator();
    // SSH_ASKPASS: ssh запускает эту же программу и читает пароль из её вывода.
    if let Some(f) = std::env::var_os("HOPEBACKUP_ASKPASS") {
        // Режим SSH_ASKPASS (вход на цель восстановления по паролю): напечатать пароль из файла 0600.
        println!("{}", std::fs::read_to_string(&f).unwrap_or_default());
        std::process::exit(0);
    }

    let cli = Cli::parse();
    // Разовая команда, чей вывод читают через трубу (`hopebackup … list | head -1`): если читатель закрыл трубу,
    // выйти тихо с кодом 0, как обычные утилиты, а не падать с паникой «failed printing to stdout: Broken pipe».
    // SIGPIPE по-прежнему игнорируется: запись в трубы дочерних процессов даёт ошибку EPIPE, а не гибель процесса.
    // Службы (server, node run, agent run) не трогаем.
    let daemon = matches!(cli.cmd, Cmd::Server { .. } | Cmd::Node { cmd: NodeCmd::Run { .. } } | Cmd::Agent { cmd: AgentCmd::Run });
    if !daemon {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let p = info.payload();
            let msg = p.downcast_ref::<String>().map(String::as_str).or_else(|| p.downcast_ref::<&str>().copied()).unwrap_or("");
            if stdout_closed(msg) { std::process::exit(0); }
            default_hook(info);
        }));
    }
    // wal-push вызывается PostgreSQL на каждый сегмент: без рантайма и логирования, максимально просто.
    if let Cmd::WalPush { spool, path } = &cli.cmd {
        if let Err(e) = plugins::postgres::wal_push(spool, path) {
            eprintln!("hopebackup wal-push: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Установщик обновления: свой простой рантайм, без логирования службы.
    if let Cmd::ApplyUpdate { request } = &cli.cmd {
        tlsutil::install_provider();
        std::process::exit(updater::run(request));
    }
    init_logging();
    tlsutil::install_provider();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("рантайм");
    let code = rt.block_on(async move {
        match run(cli).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ошибка: {e:#}");
                1
            }
        }
    });
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Server { listen } => server::run(&listen).await?,
        Cmd::Node { cmd } => match cmd {
            NodeCmd::Init { data, listen } => node::init(&data, &listen)?,
            NodeCmd::Run { data, plain } => node::run(data, plain).await?,
            NodeCmd::Token { data, name, role, repo } => {
                let role = match role.as_str() { "read" => node::Role::Read, "append" => node::Role::Append, "admin" => node::Role::Admin, _ => bail!("роль: read | append | admin") };
                println!("{}", node::add_token(&data, &name, role, &repo)?);
            }
            NodeCmd::Fingerprint { data } => {
                let (c, _) = tlsutil::ensure_cert(&data, &[])?;
                println!("{}", tlsutil::fingerprint_pem(&c)?);
            }
        },
        Cmd::Agent { cmd } => match cmd {
            AgentCmd::Enroll { server, token, token_file, fingerprint, name } => {
                let token = match (token, token_file) {
                    (Some(t), _) => t,
                    (None, Some(f)) => { let t = std::fs::read_to_string(&f)?.trim().to_string(); let _ = std::fs::remove_file(&f); t }
                    (None, None) => anyhow::bail!("укажите --token или --token-file"),
                };
                let c = agent::enroll(&server, &token, fingerprint.as_deref(), name.as_deref()).await?;
                let start = if std::path::Path::new("/run/systemd/system").exists() { "systemctl enable --now hopebackup-agent" } else if std::path::Path::new("/sbin/openrc-run").exists() { "rc-update add hopebackup-agent default && rc-service hopebackup-agent start" } else { "hopebackup agent run" };
                println!("Агент «{}» подключён к {} (id {}). Запустите службу: {start}", c.name, c.server, c.agent_id);
            }
            AgentCmd::Run => agent::run(agent::load_config()?).await?,
        },
        Cmd::WalPush { .. } | Cmd::ApplyUpdate { .. } => unreachable!(),
        Cmd::WalFetch { job, name, dest } => {
            if !plugins::postgres::wal_fetch(&job, &name, &dest).await? { return Ok(1); }
        }
        Cmd::RestoreExec => { util::PATIENT.store(true, std::sync::atomic::Ordering::Relaxed); agent::restore_exec().await? }
        Cmd::DiskRead { block, disks, lvm, freeze_small, freeze_timeout } => plugins::image::disk_read(block, &disks, &lvm, freeze_small, freeze_timeout).await?,
        Cmd::WgGenkey => {
            let (sk, pk) = connectors::wireguard::genkey();
            println!("{sk} {pk}");
        }
        Cmd::Inventory { config } => {
            let cfg: serde_json::Value = match &config { Some(c) => serde_json::from_str(c).map_err(|e| anyhow::anyhow!("--config: {e}"))?, None => serde_json::json!({}) };
            let log: plugins::LogFn = std::sync::Arc::new(|s: &str| eprintln!("{s}"));
            let v = plugins::get("host").unwrap().discover(&cfg, &log).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Action { plugin, name, config, instance } => {
            let cfg: serde_json::Value = match &config { Some(c) => serde_json::from_str(c).map_err(|e| anyhow::anyhow!("--config: {e}"))?, None => serde_json::json!({}) };
            let p = plugins::get(&plugin).ok_or_else(|| anyhow::anyhow!("неизвестный плагин {plugin}"))?;
            let v = p.action(&name, &cfg, &instance, &plugins::stdout_log()).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::VpnCleanup => {
            let done = connectors::net::cleanup_dead().await;
            connectors::ipsec::cleanup_dead(&done).await;
            for i in done { println!("убран канал {i}"); }
        }
        Cmd::Repo { remote, cmd } => repo_cmd(remote, cmd).await?,
    }
    Ok(0)
}

async fn repo_cmd(r: RemoteArgs, cmd: RepoCmd) -> Result<()> {
    let mut remote = store::Remote::node(&r.url, &r.repo, &r.token, &r.fingerprint);
    if let Some(rest) = r.url.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let mut b = server::state::S3Site { provider: if r.s3_endpoint.contains("yandexcloud") { "yandex".into() } else if r.s3_endpoint.is_empty() || r.s3_endpoint.contains("amazonaws") { "aws".into() } else { "s3".into() }, endpoint: r.s3_endpoint.clone(), region: r.s3_region.clone(), bucket: bucket.into(), prefix: prefix.into(), access_key: r.s3_access_key.clone(), secret_key: r.s3_secret_key.clone(), path_style: r.s3_path_style, lock_mode: r.s3_lock_mode.clone(), lock_days: r.s3_lock_days, ..Default::default() };
        b.normalize();
        remote.s3 = Some(b.access(false));
    }
    if !r.cloud.is_empty() {
        let txt = match r.cloud.strip_prefix('@') { Some(f) => std::fs::read_to_string(f)?, None => r.cloud.clone() };
        let c: cloud::drive::CloudAccess = serde_json::from_str(&txt).map_err(|e| anyhow::anyhow!("--cloud: {e}"))?;
        remote.cloud = Some(c);
    }
    let be = store::backend_for(&remote)?;
    match cmd {
        RepoCmd::Init => {
            let repo = store::Repo::init(be, &r.password).await?;
            println!("репозиторий создан: {}", repo.cfg.id);
        }
        RepoCmd::List { instance } => {
            for h in be.list_snapshots(instance.as_deref()).await? {
                println!("{}  {}  {:<24} {:<6} {:>10}  новых {:>10}  по каналу {:>10}", h.id, h.time.format("%Y-%m-%d %H:%M:%S"), h.instance, h.kind, util::human_bytes(h.size), util::human_bytes(h.new_bytes), util::human_bytes(h.uploaded));
            }
        }
        RepoCmd::Stats => println!("{}", serde_json::to_string_pretty(&be.stats().await?)?),
        RepoCmd::Prune { keep_last, keep_days, dry_run } => {
            let rep = be.prune(&store::types::PrunePolicy { keep_last, keep_days, dry_run, per_instance: Default::default(), keep_unknown: false }).await?;
            println!("{}снапшотов {}, pack-файлов {} ({}), сегментов WAL {}", if dry_run { "будет удалено: " } else { "удалено: " }, rep.snapshots_removed.len(), rep.packs_removed, util::human_bytes(rep.bytes_freed), rep.wal_removed);
            for s in rep.snapshots_removed { println!("  {s}"); }
        }
        RepoCmd::Show { snapshot } => {
            let repo = store::Repo::open(be.clone(), &r.password).await?;
            let (h, m) = repo.manifest(&snapshot).await?;
            let prev = be.list_snapshots(Some(&h.instance)).await?.into_iter().filter(|x| x.time < h.time && x.kind == h.kind).last();
            let known: std::collections::HashSet<store::types::ChunkId> = match &prev {
                Some(p) => repo.manifest(&p.id).await?.1.streams.iter().flat_map(|s| s.chunks.iter().copied()).collect(),
                None => Default::default(),
            };
            println!("{} · {} · {} · {}", h.id, h.instance, h.kind, h.time);
            if let Some(p) = &prev { println!("сравнение с предыдущим: {}", p.id); }
            for st in &m.streams {
                let new = st.chunks.iter().filter(|c| !known.contains(c)).count();
                println!("  {:<28} {:>10}  блоков {:>6}  новых {:>6} ({:.0}%)", st.name, util::human_bytes(st.size), st.chunks.len(), new, if st.chunks.is_empty() { 0.0 } else { 100.0 * new as f64 / st.chunks.len() as f64 });
            }
        }
        RepoCmd::Backup { instance, plugin, config, kind, paths } => {
            let repo = Arc::new(store::Repo::open(be, &r.password).await?.with_level(3));
            let p = plugins::get(&plugin).ok_or_else(|| anyhow::anyhow!("неизвестный плагин {plugin}"))?;
            let mut cfg: serde_json::Value = match &config { Some(c) => serde_json::from_str(c).map_err(|e| anyhow::anyhow!("--config: {e}"))?, None => serde_json::json!({}) };
            if !paths.is_empty() { cfg["paths"] = serde_json::json!(paths); }
            let kind = kind.unwrap_or_else(|| if plugin == "files" { "files".into() } else { "full".into() });
            let ctx = plugins::BackupCtx { repo: repo.clone(), instance, config: cfg, kind, log: plugins::stdout_log(), job_id: String::new() };
            let out = p.backup(&ctx).await;
            repo.backend.release().await;
            let out = out?;
            println!("снапшот {}: прочитано {}, новых {}, отправлено {}", out.header.id, util::human_bytes(out.stats.bytes_read), util::human_bytes(out.stats.new_bytes), util::human_bytes(out.stats.uploaded));
        }
        RepoCmd::Restore { instance, snapshot, dest, plugin, params } => {
            let repo = Arc::new(store::Repo::open(be, &r.password).await?);
            let p = plugins::get(&plugin).ok_or_else(|| anyhow::anyhow!("неизвестный плагин {plugin}"))?;
            let mut params: serde_json::Value = match &params { Some(c) => serde_json::from_str(c).map_err(|e| anyhow::anyhow!("--params: {e}"))?, None => serde_json::json!({}) };
            if !dest.is_empty() { params["root"] = serde_json::json!(dest); }
            let ctx = plugins::RestoreCtx { repo, instance, snapshot_id: snapshot, params, log: plugins::stdout_log(), job_id: util::short_id(), remote };
            let v = p.restore(&ctx).await?;
            if plugin != "files" { println!("{}", serde_json::to_string_pretty(&v)?); }
        }
    }
    Ok(())
}
