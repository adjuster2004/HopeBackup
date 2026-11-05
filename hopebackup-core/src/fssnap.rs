//! Снимки файловых систем перед файловой копией: LVM, ZFS, btrfs.
//!
//! Агент находит точки монтирования, на которых лежат копируемые пути (и вложенные в них), снимает снимок
//! каждой, где это возможно, и читает файлы из снимка — все файлы копии оказываются в одном состоянии на
//! момент снимка. После копии снимки удаляются (`Drop`), оставшиеся от прерванных копий — при запуске агента
//! и перед новым снимком.
//!
//! * LVM (обычный том): `lvcreate -s -L <размер>` — ядро замораживает ФС на время создания; снимок
//!   монтируется только для чтения (XFS — с `nouuid`). Размер — доля тома (по умолчанию 10%, не меньше
//!   1 ГиБ), но не больше свободного места группы томов. После копии проверяется, не переполнился ли снимок
//!   (переполненный снимок — ошибка копии, а не тихо неполные файлы).
//! * LVM thin: `lvcreate -s -kn` без размера (место берётся из пула).
//! * ZFS: `zfs snapshot`, чтение из `<точка>/.zfs/snapshot/<имя>`.
//! * btrfs: `btrfs subvolume snapshot -r <точка> <точка>/.hopebackup-snap/<имя>`.
//!
//! Режимы: `auto` — снимок, где возможно, иначе копия «на ходу» с предупреждением; `required` — без снимка
//! копия не выполняется; `off` — без снимков. Команды «перед снимком» и «после снимка» (например, сбросить
//! буферы приложения) выполняются вокруг создания снимков.

use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::plugins::files::Remap;
use crate::plugins::LogFn;

pub const BTRFS_DIR: &str = ".hopebackup-snap";
const LV_PREFIX: &str = "hbsnap_";
const ZFS_PREFIX: &str = "hopebackup-";
const RUN_DIR: &str = "/run/hopebackup/snap";

/// Снимки, которые сейчас используются копиями этого агента (их уборка не трогает).
static ACTIVE: Mutex<Option<HashSet<String>>> = parking_lot::const_mutex(None);

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Off,
    Auto,
    Required,
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub mode: Mode,
    /// Размер снимка LVM, % тома.
    pub size_pct: u64,
    pub pre: String,
    pub post: String,
}

impl Opts {
    pub fn from_config(cfg: &serde_json::Value) -> Opts {
        let s = |k: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let mode = match s("snapshot").as_str() { "off" | "none" => Mode::Off, "required" => Mode::Required, _ => Mode::Auto };
        let pct = cfg.get("snapshot_size_pct").and_then(|v| v.as_u64()).filter(|v| (1..=100).contains(v)).unwrap_or(10);
        Opts { mode, size_pct: pct, pre: s("pre_snapshot"), post: s("post_snapshot") }
    }
}

/// Точка монтирования из /proc/self/mountinfo.
#[derive(Clone, Debug)]
pub struct MountPt {
    pub target: String,
    pub fstype: String,
    pub source: String,
    /// Корень внутри ФС (подтом btrfs, bind-монтирование).
    pub root: String,
}

fn unescape(s: &str) -> String { s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\") }

pub fn parse_mountinfo(text: &str) -> Vec<MountPt> {
    let mut out = Vec::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split(' ').collect();
        let Some(dash) = f.iter().position(|x| *x == "-") else { continue };
        if f.len() < dash + 3 || dash < 5 { continue; }
        out.push(MountPt { target: unescape(f[4]), root: unescape(f[3]), fstype: f[dash + 1].to_string(), source: unescape(f[dash + 2]) });
    }
    out
}

fn mountinfo() -> Vec<MountPt> {
    // Для стендовых проверок (имитация LVM) — подмена таблицы монтирований.
    if std::env::var("HOPEBACKUP_TESTING").as_deref() == Ok("1") {
        if let Ok(p) = std::env::var("HOPEBACKUP_TEST_MOUNTINFO") { return parse_mountinfo(&std::fs::read_to_string(p).unwrap_or_default()); }
    }
    parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
}

fn under(p: &str, dir: &str) -> bool { dir == "/" || p == dir || p.starts_with(&format!("{dir}/")) }

/// Точки монтирования, которые затрагивает копия путей: содержащие пути и вложенные в них (последнее
/// монтирование в одну точку закрывает предыдущие).
pub fn involved(paths: &[String], mounts: &[MountPt]) -> Vec<MountPt> {
    let mut last: Vec<MountPt> = Vec::new();
    for m in mounts { last.retain(|x| x.target != m.target); last.push(m.clone()); }
    let mut out: Vec<MountPt> = Vec::new();
    for p in paths {
        if let Some(m) = last.iter().filter(|m| under(p, &m.target)).max_by_key(|m| m.target.len()) {
            if !out.iter().any(|x| x.target == m.target) { out.push(m.clone()); }
        }
        for m in last.iter().filter(|m| under(&m.target, p) && m.target != *p) {
            if !out.iter().any(|x| x.target == m.target) { out.push(m.clone()); }
        }
    }
    out
}

const PSEUDO: &[&str] = &["proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs", "pstore", "bpf", "debugfs", "tracefs", "mqueue", "hugetlbfs", "configfs", "autofs", "efivarfs", "overlay", "squashfs", "binfmt_misc", "fusectl", "rpc_pipefs", "nsfs", "ramfs", "fuse.lxcfs"];
const NETWORK: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs", "glusterfs", "ceph", "9p", "virtiofs"];

fn run(prog: &str, args: &[&str], secs: u64) -> Result<String> {
    let o = crate::util::run_timeout(prog, args, secs).ok_or_else(|| anyhow::anyhow!("{prog}: не запустилась или не завершилась за {secs} с"))?;
    if !o.status.success() { bail!("{prog} {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()); }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

fn sh(cmd: &str, log: &LogFn) -> Result<()> {
    log(&format!("$ {cmd}"));
    let o = crate::util::run_timeout("sh", &["-c", cmd], 600).ok_or_else(|| anyhow::anyhow!("«{cmd}» не завершилась за 600 с"))?;
    for l in String::from_utf8_lossy(&o.stdout).lines().chain(String::from_utf8_lossy(&o.stderr).lines()) { if !l.trim().is_empty() { log(&format!("  {l}")); } }
    if !o.status.success() { bail!("команда «{cmd}» завершилась с кодом {}", o.status.code().unwrap_or(-1)); }
    Ok(())
}

#[derive(Debug)]
enum Kind {
    Lvm { vg: String, snap: String, thin: bool },
    Zfs { name: String },
    Btrfs { path: PathBuf },
}

#[derive(Debug)]
struct Snap {
    target: String,
    kind: Kind,
    /// Где монтирован снимок LVM (размонтировать при уборке).
    mounted: Option<PathBuf>,
    tag: String,
}

/// Набор снимков одной копии. При удалении — снимки убираются.
pub struct SnapSet {
    snaps: Vec<Snap>,
    pub remap: Remap,
    pub notes: Vec<String>,
    log: LogFn,
}

/// LVM-том по устройству: (группа, том, размер, атрибуты).
fn lvm_of(source: &str) -> Option<(String, String, u64, String)> {
    if !(source.starts_with("/dev/mapper/") || source.starts_with("/dev/dm-") || source.matches('/').count() == 3) { return None; }
    let out = run("lvs", &["--noheadings", "--nosuffix", "--units", "b", "--separator", "|", "-o", "vg_name,lv_name,lv_size,lv_attr", source], 30).ok()?;
    let l = out.lines().find(|l| !l.trim().is_empty())?;
    let f: Vec<&str> = l.trim().split('|').map(|x| x.trim()).collect();
    if f.len() < 4 { return None; }
    Some((f[0].to_string(), f[1].to_string(), f[2].parse().ok()?, f[3].to_string()))
}

fn vg_free(vg: &str) -> Option<u64> {
    run("vgs", &["--noheadings", "--nosuffix", "--units", "b", "-o", "vg_free", vg], 30).ok()?.trim().parse().ok()
}

pub fn human(n: u64) -> String { crate::util::human_bytes(n) }

/// Размер снимка LVM: доля тома, не меньше 1 ГиБ (и не больше тома), кратно 4 МиБ.
pub fn lvm_snap_size(lv_size: u64, pct: u64) -> u64 {
    let gib = 1u64 << 30;
    let want = (lv_size / 100 * pct).max(gib).min(lv_size.max(gib));
    want.div_ceil(4 << 20) * (4 << 20)
}

impl SnapSet {
    /// Снимки для путей. Ошибка — только в режиме `required` (или если не выполнилась команда «перед снимком»).
    pub fn prepare(paths: &[String], o: &Opts, tag: &str, log: LogFn) -> Result<SnapSet> {
        let mut set = SnapSet { snaps: vec![], remap: Remap::default(), notes: vec![], log: log.clone() };
        if o.mode == Mode::Off { return Ok(set); }
        let tag: String = tag.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        let tag = tag.chars().rev().take(12).collect::<String>().chars().rev().collect::<String>();
        cleanup_stale(&log);
        let mounts = involved(paths, &mountinfo());
        if !o.pre.is_empty() { sh(&o.pre, &log)?; }
        let res = (|| -> Result<()> {
            for (i, m) in mounts.iter().enumerate() {
                if PSEUDO.contains(&m.fstype.as_str()) || NETWORK.contains(&m.fstype.as_str()) {
                    set.remap.add(&m.target, PathBuf::from(&m.target));
                    if NETWORK.contains(&m.fstype.as_str()) { set.notes.push(format!("{} ({}) — сетевой каталог, копируется без снимка", m.target, m.fstype)); }
                    continue;
                }
                match set.snap_one(m, i, o, &tag) {
                    Ok(view) => set.remap.add(&m.target, view),
                    Err(e) => {
                        let msg = format!("{} ({}, {}): снимок невозможен — {e:#}", m.target, m.fstype, m.source);
                        if o.mode == Mode::Required { bail!("{msg}"); }
                        set.notes.push(format!("{msg}; копируется «на ходу»"));
                        set.remap.add(&m.target, PathBuf::from(&m.target));
                    }
                }
            }
            Ok(())
        })();
        if !o.post.is_empty() { if let Err(e) = sh(&o.post, &log) { log(&format!("внимание: команда после снимка: {e:#}")); } }
        res?;
        for s in &set.snaps { log(&format!("снимок {}: {}", s.target, match &s.kind { Kind::Lvm { vg, snap, thin } => format!("LVM {vg}/{snap}{}", if *thin { " (thin)" } else { "" }), Kind::Zfs { name } => format!("ZFS {name}"), Kind::Btrfs { path } => format!("btrfs {}", path.display()) })); }
        for n in &set.notes { log(&format!("внимание: {n}")); }
        Ok(set)
    }

    fn snap_one(&mut self, m: &MountPt, idx: usize, o: &Opts, tag: &str) -> Result<PathBuf> {
        let reg = |t: &str| { ACTIVE.lock().get_or_insert_with(HashSet::new).insert(t.to_string()); };
        match m.fstype.as_str() {
            "zfs" => {
                let name = format!("{}@{ZFS_PREFIX}{tag}", m.source);
                reg(&name);
                run("zfs", &["snapshot", &name], 120)?;
                self.snaps.push(Snap { target: m.target.clone(), kind: Kind::Zfs { name: name.clone() }, mounted: None, tag: name.clone() });
                Ok(PathBuf::from(&m.target).join(".zfs/snapshot").join(format!("{ZFS_PREFIX}{tag}")))
            }
            "btrfs" => {
                let dir = PathBuf::from(&m.target).join(BTRFS_DIR);
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(tag);
                let ps = path.to_string_lossy().to_string();
                reg(&ps);
                run("btrfs", &["subvolume", "snapshot", "-r", &m.target, &ps], 120)?;
                self.snaps.push(Snap { target: m.target.clone(), kind: Kind::Btrfs { path: path.clone() }, mounted: None, tag: ps });
                Ok(path)
            }
            _ => {
                let Some((vg, lv, size, attr)) = lvm_of(&m.source) else { bail!("том не в LVM, ZFS или btrfs — снимок ФС на нём не поддерживается") };
                if attr.starts_with('s') { bail!("{vg}/{lv} — сам снимок LVM"); }
                let snap = format!("{LV_PREFIX}{tag}_{idx}");
                let thin = attr.starts_with('V');
                reg(&snap);
                if thin {
                    run("lvcreate", &["-q", "-s", "-kn", "-n", &snap, &format!("{vg}/{lv}")], 180)?;
                } else {
                    let want = lvm_snap_size(size, o.size_pct);
                    let free = vg_free(&vg).unwrap_or(0);
                    if free < want { bail!("в группе томов {vg} свободно {}, а для снимка тома {lv} ({}) нужно {} ({}% тома, не меньше 1 ГиБ) — добавьте место в группу (vgextend) или уменьшите размер снимка в параметрах", human(free), human(size), human(want), o.size_pct); }
                    run("lvcreate", &["-q", "-s", "-n", &snap, "-L", &format!("{want}b"), &format!("{vg}/{lv}")], 180)?;
                }
                let dir = PathBuf::from(RUN_DIR).join(tag).join(idx.to_string());
                std::fs::create_dir_all(&dir)?;
                let dev = format!("/dev/{vg}/{snap}");
                let opts = if m.fstype == "xfs" { "ro,nouuid" } else { "ro" };
                self.snaps.push(Snap { target: m.target.clone(), kind: Kind::Lvm { vg: vg.clone(), snap: snap.clone(), thin }, mounted: None, tag: snap.clone() });
                run("mount", &["-t", &m.fstype, "-o", opts, &dev, &dir.to_string_lossy()], 120)?;
                if let Some(s) = self.snaps.last_mut() { s.mounted = Some(dir.clone()); }
                // Подтом или bind: корень внутри ФС.
                let root = m.root.trim_start_matches('/');
                Ok(if root.is_empty() { dir } else { dir.join(root) })
            }
        }
    }

    /// После чтения: не переполнился ли снимок LVM (тогда файлы в копии могли быть неполными).
    pub fn check(&self) -> Result<()> {
        for s in &self.snaps {
            if let Kind::Lvm { vg, snap, thin: false } = &s.kind {
                let out = run("lvs", &["--noheadings", "-o", "lv_attr,snap_percent", &format!("{vg}/{snap}")], 30).unwrap_or_default();
                let f: Vec<&str> = out.split_whitespace().collect();
                let invalid = f.first().map(|a| a.chars().nth(4) == Some('I')).unwrap_or(false);
                let pct: f64 = f.get(1).and_then(|p| p.parse().ok()).unwrap_or(0.0);
                if invalid || pct >= 100.0 { bail!("снимок {vg}/{snap} переполнился во время копии — увеличьте размер снимка (сейчас запись на томе {} была больше места под снимок)", s.target); }
                if pct > 80.0 { (self.log)(&format!("внимание: снимок {vg}/{snap} заполнен на {pct:.0}% — увеличьте размер снимка")); }
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool { self.snaps.is_empty() }
}

fn remove(s: &Snap, log: &LogFn) {
    if let Some(d) = &s.mounted {
        if run("umount", &[&d.to_string_lossy()], 60).is_err() { let _ = run("umount", &["-l", &d.to_string_lossy()], 60); }
        let _ = std::fs::remove_dir(d);
    }
    let r = match &s.kind {
        Kind::Lvm { vg, snap, .. } => run("lvremove", &["-q", "-f", &format!("{vg}/{snap}")], 180).map(|_| ()),
        Kind::Zfs { name } => run("zfs", &["destroy", name], 120).map(|_| ()),
        Kind::Btrfs { path } => run("btrfs", &["subvolume", "delete", &path.to_string_lossy()], 120).map(|_| ()),
    };
    if let Err(e) = r { log(&format!("внимание: снимок {} не удалён: {e:#} — его уберёт агент при следующем запуске", s.target)); }
    if let Some(a) = ACTIVE.lock().as_mut() { a.remove(&s.tag); }
}

impl Drop for SnapSet {
    fn drop(&mut self) {
        for s in self.snaps.iter().rev() { remove(s, &self.log); }
        if !self.snaps.is_empty() { (self.log)(&format!("снимки удалены: {}", self.snaps.len())); }
    }
}

/// Убрать снимки, оставшиеся от прерванных копий (не используемые сейчас этим агентом).
pub fn cleanup_stale(log: &LogFn) {
    let active = ACTIVE.lock().clone().unwrap_or_default();
    // Смонтированные снимки LVM.
    for m in parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()).iter().rev() {
        if m.target.starts_with(RUN_DIR) && !active.iter().any(|a| m.source.ends_with(a.as_str())) {
            let _ = run("umount", &["-l", &m.target], 60);
        }
    }
    if let Ok(out) = run("lvs", &["--noheadings", "--separator", "|", "-o", "vg_name,lv_name"], 30) {
        for l in out.lines() {
            let f: Vec<&str> = l.trim().split('|').collect();
            if f.len() == 2 && f[1].starts_with(LV_PREFIX) && !active.contains(f[1]) {
                log(&format!("уборка: оставшийся снимок LVM {}/{}", f[0], f[1]));
                let _ = run("lvremove", &["-q", "-f", &format!("{}/{}", f[0], f[1])], 180);
            }
        }
    }
    if let Ok(out) = run("zfs", &["list", "-H", "-t", "snapshot", "-o", "name"], 30) {
        for n in out.lines().map(|l| l.trim()).filter(|n| n.contains(&format!("@{ZFS_PREFIX}")) && !active.contains(*n)) {
            log(&format!("уборка: оставшийся снимок ZFS {n}"));
            let _ = run("zfs", &["destroy", n], 120);
        }
    }
    for m in parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()).iter().filter(|m| m.fstype == "btrfs") {
        let dir = PathBuf::from(&m.target).join(BTRFS_DIR);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path().to_string_lossy().to_string();
            if active.contains(&p) { continue; }
            log(&format!("уборка: оставшийся снимок btrfs {p}"));
            let _ = run("btrfs", &["subvolume", "delete", &p], 120);
        }
    }
    if let Ok(rd) = std::fs::read_dir(RUN_DIR) {
        for e in rd.flatten() { let _ = std::fs::remove_dir(e.path().join("0")); let _ = std::fs::remove_dir(e.path()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mounts_and_sizes() {
        let mi = "22 1 253:0 / / rw,relatime shared:1 - ext4 /dev/mapper/vg0-root rw\n\
                  30 22 253:1 / /home rw shared:2 - xfs /dev/mapper/vg0-home rw\n\
                  31 22 0:40 /@data /srv/data rw - btrfs /dev/sdb1 rw,subvol=/@data\n\
                  32 22 0:41 / /srv/data/nfs rw - nfs4 nas:/x rw\n\
                  33 22 0:5 / /proc rw - proc proc rw\n\
                  34 22 0:42 / /mnt/a\\040b rw - zfs tank/ab rw\n";
        let m = parse_mountinfo(mi);
        assert_eq!(m.len(), 6);
        assert_eq!(m[5].target, "/mnt/a b");
        assert_eq!(m[2].root, "/@data");
        let inv = involved(&["/etc".into(), "/srv/data".into()], &m);
        let t: Vec<&str> = inv.iter().map(|x| x.target.as_str()).collect();
        assert_eq!(t, vec!["/", "/srv/data", "/srv/data/nfs"]);
        let inv = involved(&["/home/u".into()], &m);
        assert_eq!(inv.iter().map(|x| x.target.as_str()).collect::<Vec<_>>(), vec!["/home"]);
        assert_eq!(lvm_snap_size(100 << 30, 10), 10 << 30);
        assert_eq!(lvm_snap_size(2 << 30, 10), 1 << 30);
        assert_eq!(lvm_snap_size(512 << 20, 10), 1 << 30);
    }
}
