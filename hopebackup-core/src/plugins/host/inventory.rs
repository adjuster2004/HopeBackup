//! Опись сервера: что на нём работает и откуда это взялось.
//!
//! * процессы (`/proc`): исполняемый файл, скрипт интерпретатора (sh, bash, python, perl, php, node, java -jar…),
//!   рабочий каталог, служба systemd, принадлежность пакету;
//! * запуск по расписанию: cron (системный, cron.d, пользовательский), anacron, таймеры systemd, at;
//! * свои службы systemd (файлы в /etc/systemd/system и /usr/local/lib/systemd);
//! * монтирования: сетевые (NFS, SMB, sshfs…), отдельные диски, bind; занятое место;
//! * контейнеры Docker: compose-проект, тома и привязанные каталоги;
//! * пользователи (вход, ключи SSH, sudo), открытые порты, пакеты, установленные вручную;
//! * узнанные продукты (PostgreSQL, GitLab, Vault…) — для них лучше отдельный экземпляр с плагином.
//!
//! Всё, что не принадлежит пакетам, становится «приложением»: его каталог (если не слишком большой) или
//! сами файлы попадают в копию.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::exclude::Excludes;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub host: String,
    pub os: String,
    pub os_id: String,
    #[serde(default)]
    pub os_like: String,
    pub kernel: String,
    pub time: String,
    pub processes: Vec<Proc>,
    pub apps: Vec<App>,
    pub schedules: Vec<Sched>,
    pub services: Vec<Svc>,
    pub mounts: Vec<Mount>,
    pub containers: Vec<Container>,
    pub users: Vec<UserInfo>,
    pub listening: Vec<Listen>,
    pub packages: Packages,
    pub products: Vec<Product>,
    /// Каталоги данных СУБД: файловая копия работающей СУБД несогласованна — их копируют плагины.
    pub db_data_dirs: Vec<String>,
    pub warnings: Vec<String>,
    /// ОС, ядро, загрузка, железо, диски, сеть, безопасность.
    #[serde(default)]
    pub system: super::system::SystemInfo,
    /// Объёмы основных частей (для оценки копии).
    #[serde(default)]
    pub sizes: Sizes,
    /// Опись снята от root (иначе часть сведений недоступна).
    #[serde(default)]
    pub as_root: bool,
    /// Как снята: agent | ssh | shell (без бинарника, сокращённая).
    #[serde(default)]
    pub method: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SizeItem {
    pub path: String,
    pub bytes: u64,
    /// Подсчёт остановлен на пределе (на самом деле больше).
    pub over: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sizes {
    pub etc: SizeItem,
    pub root_home: SizeItem,
    pub usr_local: SizeItem,
    pub homes: Vec<SizeItem>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    pub name: String,
    pub exe: String,
    pub cmdline: String,
    pub cwd: String,
    #[serde(default)]
    pub unit: String,
    /// Скрипт или jar, который выполняет интерпретатор.
    #[serde(default)]
    pub script: String,
    pub packaged: bool,
    /// Каталог приложения (если файл не из пакета).
    #[serde(default)]
    pub app: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct App {
    /// Каталог приложения или отдельный файл.
    pub path: String,
    /// Файлы-точки входа (скрипты, программы).
    pub files: Vec<String>,
    /// Откуда известно: «процесс 1234», «cron /etc/cron.d/x», «служба foo.service».
    pub sources: Vec<String>,
    pub size: u64,
    /// Размер посчитан не до конца (каталог больше предела).
    pub size_over: bool,
    /// Попадает в копию автоматически (иначе — только файлы-точки входа).
    pub include: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sched {
    /// cron | anacron | timer | at | cron-dir
    pub kind: String,
    pub source: String,
    pub user: String,
    pub schedule: String,
    pub command: String,
    pub scripts: Vec<String>,
    /// Задание из пакета (системное) — для сведения, в копию его скрипты не добавляются.
    #[serde(default)]
    pub packaged: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Svc {
    pub unit: String,
    pub file: String,
    pub exec: Vec<String>,
    pub user: String,
    pub enabled: bool,
    pub custom: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Mount {
    pub target: String,
    pub source: String,
    pub fstype: String,
    /// system | disk | network | bind | pseudo | docker
    pub class: String,
    pub total: u64,
    pub used: u64,
    /// Для bind: какой каталог исходной ФС смонтирован.
    #[serde(default)]
    pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub compose_dir: String,
    pub mounts: Vec<CMount>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CMount {
    /// volume | bind
    pub kind: String,
    pub source: String,
    pub dest: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
    pub groups: Vec<String>,
    pub login: bool,
    pub has_password: bool,
    pub ssh_files: Vec<String>,
    pub sudo: bool,
    pub crontab: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Listen {
    pub proto: String,
    pub addr: String,
    pub port: u16,
    pub pid: u32,
    pub process: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Packages {
    /// dpkg | rpm | apk | ""
    pub manager: String,
    pub total: usize,
    /// Установленные вручную (или все, если различить нельзя).
    pub manual: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub title: String,
    pub plugin: String,
    pub available: bool,
    pub note: String,
}

// ------------------------------------------------------------------ вспомогательное

const SYSTEM_DIRS: &[&str] = &["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/lib", "/usr/lib64", "/lib", "/lib64", "/usr/libexec", "/usr/share", "/snap", "/usr/games"];

fn in_dir(p: &str, d: &str) -> bool { p == d || p.starts_with(&format!("{d}/")) }

pub fn is_system_path(p: &str) -> bool { SYSTEM_DIRS.iter().any(|d| in_dir(p, d)) }

fn read(p: impl AsRef<Path>) -> String { std::fs::read_to_string(p).unwrap_or_default() }

fn cmd(prog: &str, args: &[&str]) -> Option<String> {
    let o = crate::util::run_timeout(prog, args, 60)?;
    if !o.status.success() { return None; }
    Some(String::from_utf8_lossy(&o.stdout).to_string())
}

pub fn have(prog: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file())).unwrap_or(false)
}

fn is_file(p: &str) -> bool { std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false) }

/// Размер каталога с пределом: (байт, превышен ли предел). Исключения учитываются.
pub fn dir_size(p: &str, limit: u64, ex: &Excludes) -> (u64, bool) {
    let mut total = 0u64;
    let mut entries = 0u64;
    let mut stack = vec![PathBuf::from(p)];
    while let Some(d) = stack.pop() {
        let s = d.to_string_lossy();
        if ex.excluded_under(&s, p) { continue; }
        let Ok(md) = std::fs::symlink_metadata(&d) else { continue };
        if md.is_file() { total += md.len(); } else if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&d) { for e in rd.flatten() { stack.push(e.path()); } }
        }
        entries += 1;
        if total > limit || entries > 200_000 { return (total, true); }
    }
    (total, false)
}

/// Слова команды оболочки (кавычки снимаются; разделители ; | & < > ( ) ` — отдельные слова).
pub fn sh_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match q {
            Some(qc) => { if c == qc { q = None } else { cur.push(c) } }
            None => match c {
                '\'' | '"' => q = Some(c),
                '\\' => { if let Some(n) = it.next() { cur.push(n) } }
                c if c.is_whitespace() => { if !cur.is_empty() { out.push(std::mem::take(&mut cur)) } }
                ';' | '|' | '&' | '<' | '>' | '(' | ')' | '`' => {
                    if !cur.is_empty() { out.push(std::mem::take(&mut cur)) }
                    out.push(c.to_string());
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

fn interpreter(exe: &str) -> Option<&'static str> {
    let b = exe.rsplit('/').next().unwrap_or(exe);
    let b = b.trim_end_matches(" (deleted)");
    let strip = |p: &str| b == p || (b.starts_with(p) && b[p.len()..].chars().all(|c| c.is_ascii_digit() || c == '.'));
    if ["sh", "bash", "dash", "zsh", "ksh", "ash", "mksh", "busybox"].contains(&b) { return Some("sh"); }
    if strip("python") || strip("pypy") { return Some("python"); }
    if strip("perl") { return Some("perl"); }
    if strip("ruby") { return Some("ruby"); }
    if b == "node" || b == "nodejs" || b == "bun" || b == "deno" { return Some("node"); }
    if strip("php") || b == "php-cli" { return Some("php"); }
    if strip("lua") || b == "luajit" { return Some("lua"); }
    if b == "java" { return Some("java"); }
    if b == "Rscript" || b == "tclsh" || b == "expect" || b == "awk" || b == "gawk" { return Some("other"); }
    None
}

fn resolve(arg: &str, cwd: &str) -> Option<String> {
    let p = if arg.starts_with('/') { PathBuf::from(arg) } else if !cwd.is_empty() { Path::new(cwd).join(arg) } else { return None };
    let c = std::fs::canonicalize(&p).ok()?;
    let s = c.to_string_lossy().to_string();
    if is_file(&s) { Some(s) } else { None }
}

/// Скрипт (или jar), который выполняет интерпретатор.
fn script_of(kind: &str, args: &[String], cwd: &str) -> Option<String> {
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if kind == "java" {
            if a == "-jar" { return args.get(i + 1).and_then(|j| resolve(j, cwd)); }
            i += 1;
            continue;
        }
        if a == "-c" || a == "-e" || a == "-m" || a == "-r" || a == "--eval" {
            // Код в командной строке: ищем пути внутри него.
            if kind == "sh" && a == "-c" {
                if let Some(code) = args.get(i + 1) { return paths_in_command(code).into_iter().next(); }
            }
            return None;
        }
        if a.starts_with('-') { i += 1; continue; }
        return resolve(a, cwd);
    }
    None
}

/// Файлы, на которые ссылается команда (cron, ExecStart, sh -c): абсолютные пути и ./x после cd.
pub fn paths_in_command(c: &str) -> Vec<String> {
    let w = sh_words(c);
    let mut out = Vec::new();
    let mut dir = String::new();
    let mut i = 0;
    while i < w.len() {
        let t = &w[i];
        if t == "cd" { if let Some(d) = w.get(i + 1) { if d.starts_with('/') { dir = d.clone(); } } i += 2; continue; }
        let cand = if t.starts_with('/') { Some(t.clone()) } else if (t.starts_with("./") || t.starts_with("../")) && !dir.is_empty() { Some(format!("{dir}/{t}")) } else { None };
        if let Some(c) = cand {
            let c = c.split('=').next_back().unwrap_or(&c).to_string();
            if let Some(r) = resolve(&c, "") {
                if !is_system_path(&r) && !r.starts_with("/dev/") && !r.starts_with("/proc/") && !out.contains(&r) { out.push(r); }
            }
        }
        i += 1;
    }
    out
}

/// Служебные каталоги, которые не бывают «приложением»: псевдо-ФС, загрузка, временные файлы, журналы,
/// кэши и базы пакетного менеджера. Пути из командных строк служб (pid-файлы в /run, efivars, grub) сюда и ведут.
pub fn not_app_dir(p: &str) -> bool {
    ["/run", "/var/run", "/sys", "/proc", "/dev", "/boot", "/tmp", "/var/tmp", "/var/log", "/var/cache", "/var/lib/dpkg", "/var/lib/apt", "/var/lib/rpm", "/var/lib/dnf", "/var/lib/yum", "/lost+found"]
        .iter().any(|d| in_dir(p, d))
}

/// Каталог приложения для файла не из пакета.
pub fn app_root(f: &str, homes: &[String]) -> String {
    for base in ["/opt/", "/srv/", "/app/", "/apps/", "/usr/local/src/", "/var/www/", "/data/apps/"] {
        if let Some(rest) = f.strip_prefix(base) {
            if let Some((first, _)) = rest.split_once('/') { return format!("{base}{first}"); }
            return f.to_string();
        }
    }
    if f.starts_with("/usr/local/") || f.starts_with("/etc/") { return f.to_string(); }
    let parent = Path::new(f).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    if parent.is_empty() || parent == "/" || is_system_path(&parent) { return f.to_string(); }
    if homes.iter().any(|h| *h == parent) || parent == "/root" { return f.to_string(); }
    parent
}

// ------------------------------------------------------------------ пакеты

struct PkgDb {
    manager: String,
    files: HashSet<String>,
}

impl PkgDb {
    fn load() -> PkgDb {
        let mut files = HashSet::new();
        if Path::new("/var/lib/dpkg/info").is_dir() {
            if let Ok(rd) = std::fs::read_dir("/var/lib/dpkg/info") {
                for e in rd.flatten() {
                    if e.path().extension().map(|x| x == "list").unwrap_or(false) {
                        for l in read(e.path()).lines() { files.insert(l.to_string()); }
                    }
                }
            }
            return PkgDb { manager: "dpkg".into(), files };
        }
        if have("rpm") { return PkgDb { manager: "rpm".into(), files }; }
        if Path::new("/lib/apk/db/installed").exists() {
            let mut dir = String::new();
            for l in read("/lib/apk/db/installed").lines() {
                if let Some(d) = l.strip_prefix("F:") { dir = format!("/{d}"); }
                if let Some(f) = l.strip_prefix("R:") { files.insert(format!("{dir}/{f}")); }
            }
            return PkgDb { manager: "apk".into(), files };
        }
        PkgDb { manager: String::new(), files }
    }

    /// Каким файлам нет пакета: для rpm — одним вызовом rpm -qf.
    fn unowned(&self, cands: &[String]) -> HashSet<String> {
        let mut out = HashSet::new();
        match self.manager.as_str() {
            "dpkg" | "apk" => {
                for c in cands {
                    // dpkg хранит пути без разрешения /usr-merge: проверяем оба варианта.
                    let alt = c.strip_prefix("/usr").map(|s| s.to_string()).unwrap_or_else(|| format!("/usr{c}"));
                    if !self.files.contains(c) && !self.files.contains(&alt) { out.insert(c.clone()); }
                }
            }
            "rpm" => {
                if cands.is_empty() { return out; }
                let mut args = vec!["-qf", "--qf", "%{NAME}\\n"];
                args.extend(cands.iter().map(|s| s.as_str()));
                let o = crate::util::run_timeout("rpm", &args, 120);
                if let Some(o) = o {
                    let text = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
                    for c in cands { if text.lines().any(|l| l.contains(c.as_str()) && l.contains("not owned")) { out.insert(c.clone()); } }
                }
            }
            _ => {
                for c in cands { if !is_system_path(c) { out.insert(c.clone()); } }
            }
        }
        out
    }
}

fn packages(db: &PkgDb) -> Packages {
    let mut p = Packages { manager: db.manager.clone(), ..Default::default() };
    match db.manager.as_str() {
        "dpkg" => {
            p.total = cmd("dpkg-query", &["-W", "-f", "${db:Status-Abbrev}\\n"]).map(|s| s.lines().filter(|l| l.starts_with("ii")).count()).unwrap_or(0);
            p.manual = cmd("apt-mark", &["showmanual"]).map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or_default();
        }
        "rpm" => {
            let all: Vec<String> = cmd("rpm", &["-qa", "--qf", "%{NAME}\\n"]).map(|s| s.lines().map(|l| l.to_string()).collect()).unwrap_or_default();
            p.total = all.len();
            p.manual = cmd("dnf", &["repoquery", "--userinstalled", "--qf", "%{name}", "-C"]).map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or(all);
        }
        "apk" => {
            p.manual = read("/etc/apk/world").lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            p.total = read("/lib/apk/db/installed").lines().filter(|l| l.starts_with("P:")).count();
        }
        _ => {}
    }
    p.manual.sort();
    p.manual.dedup();
    p
}

// ------------------------------------------------------------------ пользователи

pub struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: String,
    pub shell: String,
}

pub fn parse_passwd(text: &str) -> Vec<Passwd> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 || l.starts_with('#') { return None; }
            Some(Passwd { name: f[0].into(), uid: f[2].parse().ok()?, gid: f[3].parse().ok()?, gecos: f[4].into(), home: f[5].into(), shell: f[6].into() })
        })
        .collect()
}

/// Группы: имя → (gid, участники).
pub fn parse_group(text: &str) -> BTreeMap<String, (u32, Vec<String>)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 4 || l.starts_with('#') { return None; }
            let members = f[3].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            Some((f[0].to_string(), (f[2].parse().ok()?, members)))
        })
        .collect()
}

pub fn is_regular(uid: u32) -> bool { uid == 0 || (1000..60000).contains(&uid) }

pub fn login_shell(sh: &str) -> bool { !(sh.ends_with("nologin") || sh.ends_with("/false") || sh.is_empty() || sh.ends_with("/sync")) }

fn users() -> Vec<UserInfo> {
    let pw = parse_passwd(&read("/etc/passwd"));
    let gr = parse_group(&read("/etc/group"));
    let shadow: HashMap<String, String> = read("/etc/shadow").lines().filter_map(|l| { let mut f = l.split(':'); Some((f.next()?.to_string(), f.next()?.to_string())) }).collect();
    let sudoers = read("/etc/sudoers") + &std::fs::read_dir("/etc/sudoers.d").map(|rd| rd.flatten().map(|e| read(e.path())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
    let spool: HashSet<String> = ["/var/spool/cron/crontabs", "/var/spool/cron"].iter().filter_map(|d| std::fs::read_dir(d).ok()).flat_map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string())).collect();
    let mut out = Vec::new();
    for u in pw {
        let ssh: Vec<String> = std::fs::read_dir(Path::new(&u.home).join(".ssh")).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
        if ssh.is_empty() && !(u.uid == 0 || (is_regular(u.uid) && login_shell(&u.shell))) { continue; }
        let mut groups: Vec<String> = gr.iter().filter(|(_, (gid, m))| *gid == u.gid || m.contains(&u.name)).map(|(n, _)| n.clone()).collect();
        groups.sort();
        let sudo = u.uid == 0 || groups.iter().any(|g| g == "sudo" || g == "wheel" || g == "admin") || sudoers.lines().any(|l| !l.trim_start().starts_with('#') && l.split_whitespace().next() == Some(u.name.as_str()));
        let hash = shadow.get(&u.name).cloned().unwrap_or_default();
        let mut ssh = ssh;
        ssh.sort();
        out.push(UserInfo {
            login: login_shell(&u.shell),
            has_password: !hash.is_empty() && !hash.starts_with('!') && !hash.starts_with('*'),
            crontab: spool.contains(&u.name),
            name: u.name,
            uid: u.uid,
            gid: u.gid,
            home: u.home,
            shell: u.shell,
            groups,
            ssh_files: ssh,
            sudo,
        });
    }
    out
}

// ------------------------------------------------------------------ процессы

struct RawProc {
    pid: u32,
    ppid: u32,
    uid: u32,
    name: String,
    exe: String,
    args: Vec<String>,
    cwd: String,
    unit: String,
}

fn raw_procs() -> Vec<RawProc> {
    let me = std::process::id();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me { continue; }
        let base = e.path();
        let raw = std::fs::read(base.join("cmdline")).unwrap_or_default();
        if raw.is_empty() { continue; } // поток ядра
        let args: Vec<String> = raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect();
        let status = read(base.join("status"));
        let field = |k: &str| status.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string())).unwrap_or_default();
        let ppid: u32 = field("PPid:").parse().unwrap_or(0);
        if ppid == 2 { continue; } // потомки kthreadd
        let uid: u32 = field("Uid:").split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let exe = std::fs::read_link(base.join("exe")).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let cwd = std::fs::read_link(base.join("cwd")).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let cg = read(base.join("cgroup"));
        let unit = cg.lines().filter_map(|l| l.rsplit('/').next()).find(|s| s.ends_with(".service") || s.ends_with(".scope")).unwrap_or("").to_string();
        out.push(RawProc { pid, ppid, uid, name: field("Name:"), exe, args, cwd, unit });
    }
    out.sort_by_key(|p| p.pid);
    out
}

// ------------------------------------------------------------------ расписания и службы

fn cron_line(l: &str, with_user: bool) -> Option<(String, String, String)> {
    let l = l.trim();
    if l.is_empty() || l.starts_with('#') { return None; }
    // Переменные окружения: NAME=value
    if let Some((k, _)) = l.split_once('=') { if !k.contains(char::is_whitespace) && !k.starts_with('*') && !k.starts_with('@') && !k.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) { return None; } }
    let mut rest = l;
    let take = |rest: &mut &str| -> Option<String> {
        let t = rest.trim_start();
        let end = t.find(char::is_whitespace)?;
        let (a, b) = t.split_at(end);
        *rest = b;
        Some(a.to_string())
    };
    let sched = if l.starts_with('@') { take(&mut rest)? } else { (0..5).map(|_| take(&mut rest)).collect::<Option<Vec<_>>>()?.join(" ") };
    let user = if with_user { take(&mut rest)? } else { String::new() };
    Some((sched, user, rest.trim().to_string()))
}

fn schedules(out: &mut Vec<Sched>) {
    let mut sys: Vec<PathBuf> = vec![PathBuf::from("/etc/crontab")];
    if let Ok(rd) = std::fs::read_dir("/etc/cron.d") { sys.extend(rd.flatten().map(|e| e.path())); }
    for f in sys {
        for l in read(&f).lines() {
            if let Some((s, u, c)) = cron_line(l, true) {
                // run-parts /etc/cron.daily — сами каталоги разбираются ниже.
                if c.contains("run-parts") || c.contains("anacron") { continue; }
                out.push(Sched { kind: "cron".into(), source: f.to_string_lossy().into(), user: u, schedule: s, scripts: paths_in_command(&c), command: c, packaged: false });
            }
        }
    }
    for d in ["/var/spool/cron/crontabs", "/var/spool/cron"] {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            if !e.path().is_file() { continue; }
            let user = e.file_name().to_string_lossy().to_string();
            for l in read(e.path()).lines() {
                if let Some((s, _, c)) = cron_line(l, false) {
                    out.push(Sched { kind: "cron".into(), source: e.path().to_string_lossy().into(), user: user.clone(), schedule: s, scripts: paths_in_command(&c), command: c, packaged: false });
                }
            }
        }
    }
    for (d, when) in [("/etc/cron.hourly", "ежечасно"), ("/etc/cron.daily", "ежедневно"), ("/etc/cron.weekly", "еженедельно"), ("/etc/cron.monthly", "ежемесячно")] {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path().to_string_lossy().to_string();
            if p.ends_with(".placeholder") || !is_file(&p) { continue; }
            let mut scripts = vec![p.clone()];
            scripts.extend(paths_in_command(&read(&p).lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join(" ; ")));
            scripts.dedup();
            out.push(Sched { kind: "cron-dir".into(), source: d.into(), user: "root".into(), schedule: when.into(), command: p, scripts, packaged: false });
        }
    }
    for l in read("/etc/anacrontab").lines() {
        let t = l.trim();
        if t.is_empty() || t.starts_with('#') || t.contains('=') && !t.contains(' ') { continue; }
        let f: Vec<&str> = t.splitn(4, char::is_whitespace).collect();
        if f.len() == 4 && !f[3].contains("run-parts") {
            out.push(Sched { kind: "anacron".into(), source: "/etc/anacrontab".into(), user: "root".into(), schedule: format!("каждые {} дн.", f[0]), scripts: paths_in_command(f[3]), command: f[3].into(), packaged: false });
        }
    }
    for d in ["/var/spool/cron/atjobs", "/var/spool/at"] {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('.') || n == "spool" || !e.path().is_file() { continue; }
            let body = read(e.path());
            let cmdl = body.lines().filter(|l| !l.is_empty() && !l.starts_with('#') && !l.contains("export") && !l.starts_with("cd ") && !l.contains("umask")).last().unwrap_or("").to_string();
            out.push(Sched { kind: "at".into(), source: e.path().to_string_lossy().into(), user: String::new(), schedule: "однократно".into(), scripts: paths_in_command(&cmdl), command: cmdl, packaged: false });
        }
    }
}

const UNIT_DIRS: &[&str] = &["/etc/systemd/system", "/run/systemd/system", "/usr/local/lib/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system"];

fn unit_file(name: &str) -> Option<PathBuf> {
    UNIT_DIRS.iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn unit_values(text: &str, key: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.trim().strip_prefix(&format!("{key}="))).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect()
}

fn services(out: &mut Vec<Svc>, sched: &mut Vec<Sched>) {
    let mut enabled: HashSet<String> = HashSet::new();
    if let Ok(rd) = std::fs::read_dir("/etc/systemd/system") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".wants") || n.ends_with(".requires") {
                if let Ok(r2) = std::fs::read_dir(e.path()) { for x in r2.flatten() { enabled.insert(x.file_name().to_string_lossy().to_string()); } }
            }
        }
    }
    // Свои юниты: обычные файлы в /etc/systemd/system и /usr/local/lib/systemd/system.
    let mut custom: HashSet<String> = HashSet::new();
    for d in ["/etc/systemd/system", "/usr/local/lib/systemd/system"] {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                let md = std::fs::symlink_metadata(e.path());
                if md.map(|m| m.is_file()).unwrap_or(false) && (n.ends_with(".service") || n.ends_with(".timer")) { custom.insert(n); }
            }
        }
    }
    let mut names: Vec<String> = enabled.iter().chain(custom.iter()).filter(|n| n.ends_with(".service") || n.ends_with(".timer")).cloned().collect();
    names.sort();
    names.dedup();
    for n in names {
        let Some(f) = unit_file(&n) else { continue };
        let text = read(&f);
        if n.ends_with(".timer") {
            let svc = unit_values(&text, "Unit").into_iter().next().unwrap_or_else(|| n.replace(".timer", ".service"));
            let st = unit_file(&svc).map(read).unwrap_or_default();
            let exec: Vec<String> = unit_values(&st, "ExecStart");
            let scripts: Vec<String> = exec.iter().flat_map(|e| paths_in_command(e.trim_start_matches(['-', '@', ':', '+', '!']))).collect();
            let when: Vec<String> = ["OnCalendar", "OnBootSec", "OnUnitActiveSec", "OnActiveSec"].iter().flat_map(|k| unit_values(&text, k).into_iter().map(move |v| format!("{k}={v}"))).collect();
            sched.push(Sched { kind: "timer".into(), source: f.to_string_lossy().into(), user: unit_values(&st, "User").into_iter().next().unwrap_or_else(|| "root".into()), schedule: when.join(", "), command: exec.join(" ; "), scripts, packaged: !(custom.contains(&n) || custom.contains(&svc)) });
        } else {
            let exec: Vec<String> = ["ExecStartPre", "ExecStart", "ExecStartPost", "ExecReload", "ExecStop"].iter().flat_map(|k| unit_values(&text, k)).collect();
            let scripts: Vec<String> = exec.iter().flat_map(|e| paths_in_command(e.trim_start_matches(['-', '@', ':', '+', '!']))).collect();
            let _ = scripts;
            out.push(Svc { unit: n.clone(), file: f.to_string_lossy().into(), user: unit_values(&text, "User").into_iter().next().unwrap_or_default(), enabled: enabled.contains(&n), custom: custom.contains(&n), exec });
        }
    }
}

// ------------------------------------------------------------------ монтирования

fn unescape_mount(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) { out.push(v as char); i += 4; continue; }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

const PSEUDO: &[&str] = &["proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs", "pstore", "bpf", "debugfs", "tracefs", "mqueue", "hugetlbfs", "configfs", "fusectl", "binfmt_misc", "autofs", "efivarfs", "rpc_pipefs", "nsfs", "ramfs", "selinuxfs", "squashfs", "fuse.lxcfs", "fuse.gvfsd-fuse", "fuse.portal", "overlay", "zram"];
const NETWORK: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs", "fuse.sshfs", "glusterfs", "fuse.glusterfs", "ceph", "fuse.ceph", "fuse.rclone", "davfs", "fuse.davfs2", "9p", "virtiofs", "fuse.s3fs", "lustre", "beegfs", "gpfs"];

pub fn statvfs(p: &str) -> (u64, u64) {
    let Ok(c) = std::ffi::CString::new(p) else { return (0, 0) };
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 { return (0, 0); }
        let bs = s.f_frsize as u64;
        let total = s.f_blocks as u64 * bs;
        let free = s.f_bfree as u64 * bs;
        (total, total.saturating_sub(free))
    }
}

pub fn mounts() -> Vec<Mount> {
    let mut by_target: BTreeMap<String, Mount> = BTreeMap::new();
    for l in read("/proc/self/mountinfo").lines() {
        let Some((left, right)) = l.split_once(" - ") else { continue };
        let lf: Vec<&str> = left.split(' ').collect();
        let rf: Vec<&str> = right.split(' ').collect();
        if lf.len() < 5 || rf.len() < 2 { continue; }
        let root = unescape_mount(lf[3]);
        let target = unescape_mount(lf[4]);
        let fstype = rf[0].to_string();
        let source = unescape_mount(rf[1]);
        let class = if target.starts_with("/var/lib/docker") || target.starts_with("/run/docker") || target.starts_with("/var/lib/containers") || target.starts_with("/run/containerd") || target.starts_with("/var/lib/kubelet") || target.starts_with("/snap/") {
            "docker"
        } else if NETWORK.contains(&fstype.as_str()) {
            "network"
        } else if PSEUDO.contains(&fstype.as_str()) || target.starts_with("/proc") || target.starts_with("/sys") || target.starts_with("/dev") || target.starts_with("/run") {
            if fstype == "overlay" && target == "/" { "system" } else { "pseudo" }
        } else if root != "/" {
            "bind"
        } else if ["/", "/boot", "/boot/efi", "/usr", "/efi"].contains(&target.as_str()) {
            "system"
        } else {
            "disk"
        };
        let (total, used) = if class == "pseudo" || class == "docker" { (0, 0) } else { statvfs(&target) };
        by_target.insert(target.clone(), Mount { target, source, fstype, class: class.into(), total, used, root: if root == "/" { String::new() } else { root } });
    }
    by_target.into_values().collect()
}

// ------------------------------------------------------------------ Docker

fn containers(warn: &mut Vec<String>) -> Vec<Container> {
    if !have("docker") || !Path::new("/var/run/docker.sock").exists() { return vec![]; }
    let Some(ids) = cmd("docker", &["ps", "-aq"]) else { warn.push("Docker найден, но список контейнеров недоступен".into()); return vec![] };
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() { return vec![]; }
    let mut args = vec!["inspect"];
    args.extend(ids.iter());
    let Some(js) = cmd("docker", &args) else { return vec![] };
    let v: serde_json::Value = serde_json::from_str(&js).unwrap_or_default();
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let s = |p: &str| c.pointer(p).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let mounts = c.get("Mounts").and_then(|m| m.as_array()).map(|m| m.iter().map(|x| CMount {
                        kind: x.get("Type").and_then(|v| v.as_str()).unwrap_or("").into(),
                        source: x.get("Source").and_then(|v| v.as_str()).unwrap_or("").into(),
                        dest: x.get("Destination").and_then(|v| v.as_str()).unwrap_or("").into(),
                        name: x.get("Name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    }).filter(|m| m.kind == "volume" || m.kind == "bind").collect()).unwrap_or_default();
                    Container {
                        id: s("/Id").chars().take(12).collect(),
                        name: s("/Name").trim_start_matches('/').into(),
                        image: s("/Config/Image"),
                        state: s("/State/Status"),
                        compose_dir: c.pointer("/Config/Labels/com.docker.compose.project.working_dir").and_then(|x| x.as_str()).unwrap_or("").into(),
                        mounts,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ порты

fn listening(procs: &[RawProc]) -> Vec<Listen> {
    let mut inode_pid: HashMap<String, u32> = HashMap::new();
    for p in procs {
        if let Ok(rd) = std::fs::read_dir(format!("/proc/{}/fd", p.pid)) {
            for e in rd.flatten() {
                if let Ok(t) = std::fs::read_link(e.path()) {
                    let t = t.to_string_lossy();
                    if let Some(i) = t.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']')) { inode_pid.entry(i.to_string()).or_insert(p.pid); }
                }
            }
        }
    }
    let names: HashMap<u32, String> = procs.iter().map(|p| (p.pid, p.name.clone())).collect();
    let mut out = Vec::new();
    for (f, proto, v6) in [("/proc/net/tcp", "tcp", false), ("/proc/net/tcp6", "tcp", true), ("/proc/net/udp", "udp", false), ("/proc/net/udp6", "udp", true)] {
        for l in read(f).lines().skip(1) {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() < 10 { continue; }
            let listen = if proto == "tcp" { c[3] == "0A" } else { c[3] == "07" };
            if !listen { continue; }
            let Some((a, port)) = c[1].split_once(':') else { continue };
            let port = u16::from_str_radix(port, 16).unwrap_or(0);
            let addr = if v6 {
                if a.chars().all(|x| x == '0') { "::".to_string() } else if a.ends_with("0100007F") || a == "00000000000000000000000001000000" { "::1".into() } else { "IPv6".into() }
            } else {
                let n = u32::from_str_radix(a, 16).unwrap_or(0);
                std::net::Ipv4Addr::from(n.swap_bytes()).to_string()
            };
            let pid = inode_pid.get(c[9]).copied().unwrap_or(0);
            if out.iter().any(|x: &Listen| x.proto == proto && x.port == port && x.pid == pid && x.addr == addr) { continue; }
            out.push(Listen { proto: proto.into(), addr, port, pid, process: names.get(&pid).cloned().unwrap_or_default() });
        }
    }
    out.sort_by_key(|l| (l.proto.clone(), l.port));
    out
}

// ------------------------------------------------------------------ продукты

fn products(procs: &[RawProc], warn: &mut Vec<String>) -> (Vec<Product>, Vec<String>) {
    let mut out: Vec<Product> = Vec::new();
    let mut data = Vec::new();
    let mut add = |id: &str, title: &str, plugin: &str, available: bool, note: &str| {
        if !out.iter().any(|p| p.id == id) { out.push(Product { id: id.into(), title: title.into(), plugin: plugin.into(), available, note: note.into() }); }
    };
    let arg = |p: &RawProc, k: &str| -> Option<String> {
        for (i, a) in p.args.iter().enumerate() {
            if a == k { return p.args.get(i + 1).cloned(); }
            if let Some(v) = a.strip_prefix(&format!("{k}=")) { return Some(v.to_string()); }
        }
        None
    };
    for p in procs {
        let exe = p.exe.rsplit('/').next().unwrap_or("").trim_end_matches(" (deleted)");
        match exe {
            "postgres" | "postmaster" => {
                add("postgresql", "PostgreSQL", "postgresql", true, "архив WAL и восстановление на момент времени — отдельным экземпляром");
                if let Some(d) = arg(p, "-D") { data.push(d); } else if p.ppid == 1 || !procs.iter().any(|q| q.pid == p.ppid && q.name.starts_with("postgres")) { if !p.cwd.is_empty() && p.cwd != "/" { data.push(p.cwd.clone()); } }
            }
            "mysqld" | "mariadbd" => {
                add("mysql", "MySQL / MariaDB", "mysql", true, "дамп или mariabackup, архив binlog — отдельным экземпляром");
                data.push(arg(p, "--datadir").unwrap_or_else(|| "/var/lib/mysql".into()));
            }
            "mongod" => {
                add("mongodb", "MongoDB", "mongodb", true, "mongodump с oplog — отдельным экземпляром");
                data.push(arg(p, "--dbpath").unwrap_or_else(|| "/var/lib/mongodb".into()));
            }
            "clickhouse" | "clickhouse-server" => { add("clickhouse", "ClickHouse", "clickhouse", true, "BACKUP или Native — отдельным экземпляром"); data.push("/var/lib/clickhouse".into()); }
            "sqlservr" => { add("mssql", "MS SQL Server", "mssql", true, "BACKUP DATABASE/LOG — отдельным экземпляром"); data.push("/var/opt/mssql/data".into()); }
            "etcd" => { add("etcd", "etcd", "kubernetes", true, "снимок etcdctl — экземпляром Kubernetes"); data.push(arg(p, "--data-dir").unwrap_or_else(|| "/var/lib/etcd".into())); }
            "vault" | "bao" => add("vault", "Vault / OpenBao", "vault", true, "снапшот raft через API"),
            "nginx" => add("nginx", "nginx", "files", true, ""),
            "haproxy" => add("haproxy", "HAProxy", "files", true, ""),
            "openvpn" => add("openvpn", "OpenVPN", "files", true, ""),
            "redis-server" | "valkey-server" => add("redis", "Redis", "redis", true, "снимок RDB без остановки — отдельным экземпляром"),
            "dockerd" => add("docker", "Docker", "docker", true, "тома с паузой контейнеров, compose — отдельным экземпляром"),
            "minio" => add("s3", "MinIO", "s3", true, "бакеты — плагином S3"),
            "slapd" => { add("openldap", "OpenLDAP", "freeipa", true, "slapcat cn=config и баз"); data.push("/var/lib/ldap".into()); }
            "ns-slapd" => add("freeipa", "FreeIPA / 389 Directory", "freeipa", true, "ipa-backup — отдельным экземпляром"),
            "zabbix_server" => add("zabbix", "Zabbix", "zabbix", true, "дамп базы и конфигурация"),
            "grafana" | "grafana-server" => add("grafana", "Grafana", "zabbix", true, "grafana.db, плагины, provisioning"),
            "libvirtd" | "virtqemud" => add("libvirt", "libvirt / KVM", "vm", true, "образы ВМ со снимком дисков без остановки"),
            "pvedaemon" | "pveproxy" => add("proxmox", "Proxmox VE", "vm", true, "vzdump ВМ и контейнеров потоком"),
            "kubelet" | "k3s" | "k3s-server" => add("kubernetes", "Kubernetes", "kubernetes", true, "объекты API, etcd, тома hostPath"),
            "hopedb-server" => add("hopemail", "Hope Mail Server", "hopemail", true, ""),
            "hope-msg-gateway" => add("hopegw", "Hope MSG Gateway", "hopegw", true, ""),
            "hopeinv" => add("hopeinv", "HopeInventory", "hopeinv", true, ""),
            "hopedfs" => add("hopedfs", "HopeDFS", "hopedfs", true, ""),
            "hopedc-core" => add("hopedc", "HopeDC", "hopedc", true, ""),
            "java" => {
                let a = p.args.join(" ");
                if a.contains("kc.home.dir") || a.contains("keycloak") { add("keycloak", "Keycloak", "keycloak", true, "выгрузка realm; база — плагином СУБД"); }
                else if a.contains("jenkins.war") { add("jenkins", "Jenkins", "jenkins", true, "JENKINS_HOME без рабочих каталогов"); }
                else if a.contains("karaf.data") || a.contains("sonatype") { add("nexus", "Nexus Repository", "nexus", true, "БД и blob"); }
                else if a.contains("org.opensearch.bootstrap") { add("opensearch", "OpenSearch", "opensearch", true, "Snapshot API"); }
                else if a.contains("org.elasticsearch.bootstrap") { add("opensearch", "Elasticsearch", "opensearch", true, "Snapshot API"); }
            }
            _ => {}
        }
        if p.exe.starts_with("/opt/gitlab/") { add("gitlab", "GitLab", "gitlab", true, "gitlab-backup и секреты — отдельным экземпляром"); }
    }
    if out.iter().any(|p| p.id == "gitlab") { data.push("/var/opt/gitlab/postgresql/data".into()); data.push("/var/opt/gitlab/git-data".into()); }
    data.sort();
    data.dedup();
    data.retain(|d| Path::new(d).is_dir());
    if !data.is_empty() { warn.push(format!("каталоги данных СУБД не копируются файлами (несогласованно на работающей СУБД): {}", data.join(", "))); }
    (out, data)
}

// ------------------------------------------------------------------ сборка описи

pub struct Opts {
    pub app_limit: u64,
    pub ex: Excludes,
}

/// Облегчённая опись для агента: только процессы, продукты, контейнеры и порты — без пакетов, приложений,
/// размеров и пользователей. По ней агент раз в несколько часов сообщает серверу, что работает на машине.
pub fn light() -> Inventory {
    let raw = raw_procs();
    let users_db: HashMap<u32, String> = parse_passwd(&read("/etc/passwd")).into_iter().map(|p| (p.uid, p.name)).collect();
    let mut inv = Inventory { host: crate::util::hostname(), time: chrono::Utc::now().to_rfc3339(), as_root: crate::util::is_root(), ..Default::default() };
    inv.processes = raw.iter().map(|p| Proc { pid: p.pid, ppid: p.ppid, user: users_db.get(&p.uid).cloned().unwrap_or_else(|| p.uid.to_string()), name: p.name.clone(),
        exe: p.exe.trim_end_matches(" (deleted)").to_string(), cmdline: String::new(), cwd: p.cwd.clone(), unit: p.unit.clone(), script: String::new(), packaged: true, app: String::new() }).collect();
    let (products, data) = products(&raw, &mut inv.warnings);
    inv.products = products;
    inv.db_data_dirs = data;
    inv.containers = containers(&mut inv.warnings);
    inv.listening = listening(&raw);
    inv
}

pub fn collect(o: &Opts) -> Inventory {
    let osr = read("/etc/os-release");
    let osv = |k: &str| osr.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("").trim_matches('"').to_string();
    let mut inv = Inventory {
        host: crate::util::hostname(),
        os: osv("PRETTY_NAME"),
        os_id: osv("ID"),
        os_like: osv("ID_LIKE"),
        kernel: read("/proc/sys/kernel/osrelease").trim().to_string(),
        time: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    let db = PkgDb::load();
    let raw = raw_procs();
    let users_db: HashMap<u32, String> = parse_passwd(&read("/etc/passwd")).into_iter().map(|p| (p.uid, p.name)).collect();
    inv.users = users();
    let homes: Vec<String> = inv.users.iter().map(|u| u.home.clone()).collect();

    // Процессы → кандидаты (исполняемые файлы и скрипты).
    let mut procs: Vec<Proc> = Vec::new();
    for p in &raw {
        let exe = p.exe.trim_end_matches(" (deleted)").to_string();
        if exe.ends_with("/hopebackup") { continue; }
        let script = interpreter(&exe).and_then(|k| script_of(k, &p.args, &p.cwd)).unwrap_or_default();
        procs.push(Proc { pid: p.pid, ppid: p.ppid, user: users_db.get(&p.uid).cloned().unwrap_or_else(|| p.uid.to_string()), name: p.name.clone(), exe, cmdline: p.args.join(" ").chars().take(400).collect(), cwd: p.cwd.clone(), unit: p.unit.clone(), script, packaged: true, app: String::new() });
    }
    schedules(&mut inv.schedules);
    services(&mut inv.services, &mut inv.schedules);

    // Принадлежность пакетам — одним проходом по всем кандидатам.
    let mut cands: Vec<String> = procs.iter().flat_map(|p| [p.exe.clone(), p.script.clone()]).filter(|s| !s.is_empty() && s.starts_with('/')).collect();
    cands.extend(inv.schedules.iter().flat_map(|s| s.scripts.clone()));
    cands.extend(inv.schedules.iter().filter(|s| s.kind != "cron-dir").map(|s| s.source.clone()));
    cands.extend(inv.services.iter().map(|s| s.file.clone()));
    cands.extend(inv.services.iter().flat_map(|s| s.exec.iter().flat_map(|e| paths_in_command(e.trim_start_matches(['-', '@', ':', '+', '!'])))));
    cands.sort();
    cands.dedup();
    let unowned = db.unowned(&cands);
    // Служба или таймер «свои», если юнит не из пакета или запускает файл не из пакета (кроме /etc).
    let own_file = |f: &String| unowned.contains(f) && !f.starts_with("/etc/") && is_file(f);
    inv.services.retain(|s| s.custom || s.exec.iter().any(|e| paths_in_command(e.trim_start_matches(['-', '@', ':', '+', '!'])).iter().any(own_file)));
    for s in inv.schedules.iter_mut() {
        s.packaged = match s.kind.as_str() {
            "timer" => s.packaged && !s.scripts.iter().any(own_file),
            "cron" | "anacron" => !unowned.contains(&s.source) && !s.scripts.iter().any(own_file),
            "cron-dir" => !s.scripts.iter().any(|f| unowned.contains(f)),
            _ => false,
        };
    }
    inv.schedules.retain(|s| s.kind != "timer" || !s.packaged);

    // Приложения: файлы не из пакетов, сгруппированные по каталогу.
    let mut apps: BTreeMap<String, App> = BTreeMap::new();
    let add_app = |f: &str, src: String, apps: &mut BTreeMap<String, App>| -> String {
        let root = app_root(f, &homes);
        if not_app_dir(&root) { return String::new(); }
        let a = apps.entry(root.clone()).or_insert_with(|| App { path: root.clone(), ..Default::default() });
        if !a.files.iter().any(|x| x == f) { a.files.push(f.to_string()); }
        if !a.sources.contains(&src) && a.sources.len() < 12 { a.sources.push(src); }
        root
    };
    for p in procs.iter_mut() {
        let main = if !p.script.is_empty() { p.script.clone() } else { p.exe.clone() };
        if main.is_empty() || !main.starts_with('/') || !is_file(&main) || !unowned.contains(&main) { continue; }
        // Скрипты в /etc, /tmp, /proc и т. п. — не приложения (или уже в копии /etc).
        if main.starts_with("/tmp/") || main.starts_with("/proc/") || main.starts_with("/dev/") { continue; }
        p.packaged = false;
        let src = if p.unit.ends_with(".service") { format!("служба {} (pid {})", p.unit, p.pid) } else { format!("процесс {} (pid {})", p.name, p.pid) };
        p.app = add_app(&main, src, &mut apps);
    }
    for s in &inv.schedules {
        if s.packaged { continue; }
        for f in &s.scripts {
            if unowned.contains(f) || s.kind == "cron-dir" {
                if f.starts_with("/etc/") { continue; } // /etc копируется целиком
                add_app(f, format!("{} {}", s.kind, s.source), &mut apps);
            }
        }
    }
    for s in &inv.services {
        for e in &s.exec {
            for f in paths_in_command(e.trim_start_matches(['-', '@', ':', '+', '!'])) {
                if unowned.contains(&f) && !f.starts_with("/etc/") { add_app(&f, format!("служба {}", s.unit), &mut apps); }
            }
        }
    }
    let (products, data) = products(&raw, &mut inv.warnings);
    inv.products = products;
    inv.db_data_dirs = data;
    // Размер приложений; слишком большие — только файлы-точки входа.
    for a in apps.values_mut() {
        if Path::new(&a.path).is_dir() {
            let (sz, over) = dir_size(&a.path, o.app_limit, &o.ex);
            a.size = sz;
            a.size_over = over;
            a.include = !over && !inv.db_data_dirs.iter().any(|d| in_dir(d, &a.path) || in_dir(&a.path, d));
            if !a.include { inv.warnings.push(format!("каталог приложения {} больше предела — копируются только файлы запуска ({})", a.path, a.files.join(", "))); }
        } else {
            a.size = std::fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0);
            a.include = true;
        }
    }
    inv.apps = apps.into_values().collect();
    inv.processes = procs;
    inv.mounts = mounts();
    inv.containers = containers(&mut inv.warnings);
    inv.listening = listening(&raw);
    inv.packages = packages(&db);
    inv.system = super::system::collect(&inv.mounts);
    inv.as_root = crate::util::is_root();
    let sz = |p: &str| { let (b, over) = if Path::new(p).exists() { dir_size(p, 1 << 40, &o.ex) } else { (0, false) }; SizeItem { path: p.into(), bytes: b, over } };
    inv.sizes = Sizes {
        etc: sz("/etc"),
        root_home: sz("/root"),
        // В копию идут только bin, sbin, etc и lib/systemd из /usr/local.
        usr_local: { let parts: Vec<SizeItem> = ["/usr/local/bin", "/usr/local/sbin", "/usr/local/etc", "/usr/local/lib/systemd"].iter().map(|p| sz(p)).collect(); SizeItem { path: "/usr/local".into(), bytes: parts.iter().map(|x| x.bytes).sum(), over: parts.iter().any(|x| x.over) } },
        homes: inv.users.iter().filter(|u| u.uid >= 1000 && u.login && u.home.starts_with('/') && u.home.len() > 1 && !is_system_path(&u.home)).map(|u| sz(&u.home)).collect(),
    };
    if !inv.as_root { inv.warnings.push("опись снята не от root: пароли, чужие домашние каталоги и часть служебных файлов не видны".into()); }
    for m in &inv.mounts {
        if m.class == "network" { inv.warnings.push(format!("сетевой каталог {} ({} {}) не копируется, пока его не отметить", m.target, m.fstype, m.source)); }
    }
    inv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_cron() {
        assert_eq!(sh_words("cd /opt/x && ./run.sh 'a b' >/dev/null 2>&1"), vec!["cd", "/opt/x", "&", "&", "./run.sh", "a b", ">", "/dev/null", "2", ">", "&", "1"]);
        let (s, u, c) = cron_line("*/5 * * * * root /usr/local/bin/sync.sh --quiet", true).unwrap();
        assert_eq!((s.as_str(), u.as_str(), c.as_str()), ("*/5 * * * *", "root", "/usr/local/bin/sync.sh --quiet"));
        let (s, _, c) = cron_line("@reboot /opt/app/start.sh", false).unwrap();
        assert_eq!((s.as_str(), c.as_str()), ("@reboot", "/opt/app/start.sh"));
        assert!(cron_line("MAILTO=root", true).is_none() && cron_line("# x", true).is_none());
        assert_eq!(app_root("/opt/myapp/bin/run.sh", &[]), "/opt/myapp");
        for p in ["/run", "/run/nginx.pid", "/sys/firmware/efi/efivars", "/boot/grub", "/var/log", "/var/log/app.log"] { assert!(not_app_dir(p), "{p}"); }
        for p in ["/opt/nginx-waf", "/var/lib/app", "/srv/x", "/usr/local/bin/x.sh"] { assert!(!not_app_dir(p), "{p}"); }
        assert_eq!(app_root("/usr/local/bin/x.sh", &[]), "/usr/local/bin/x.sh");
        assert_eq!(app_root("/data/jobs/nightly.sh", &[]), "/data/jobs");
        assert_eq!(app_root("/home/ivan/run.sh", &["/home/ivan".into()]), "/home/ivan/run.sh");
        assert_eq!(interpreter("/usr/bin/python3.11"), Some("python"));
        assert_eq!(interpreter("/bin/bash"), Some("sh"));
        assert_eq!(interpreter("/usr/sbin/nginx"), None);
    }
}
