//! Сведения о системе для описи: ОС и её семейство, ядро, загрузка (UEFI/BIOS, загрузчик, параметры
//! ядра, initramfs, система инициализации), железо и виртуализация, диски (разделы, LVM, RAID, LUKS,
//! fstab), сеть, безопасность (SELinux, AppArmor, файрвол, sshd), время, репозитории пакетов.
//!
//! Читает только файлы и /proc, /sys; внешние команды — по возможности и без обязательности, чтобы
//! работать на любых дистрибутивах (Debian/Ubuntu, RHEL/CentOS/Alma/Rocky/Oracle, РЕД ОС, SUSE, Astra,
//! ALT, Alpine, Arch, Gentoo…) — в том числе старых.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: OsInfo,
    pub kernel: KernelInfo,
    pub boot: BootInfo,
    pub hardware: Hardware,
    pub storage: Storage,
    pub network: Network,
    pub security: Security,
    pub time: TimeInfo,
    pub uptime_secs: u64,
    pub locale: String,
    pub repos: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OsInfo {
    pub name: String,
    pub pretty: String,
    pub id: String,
    pub id_like: String,
    pub version: String,
    pub version_id: String,
    pub codename: String,
    /// debian | rhel | suse | alpine | arch | gentoo | other
    pub family: String,
    /// Откуда взято: /etc/os-release, /etc/redhat-release…
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KernelInfo {
    pub release: String,
    pub version: String,
    pub arch: String,
    pub cmdline: String,
    /// Ядра в /boot.
    pub installed: Vec<String>,
    pub modules_loaded: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BootInfo {
    /// uefi | bios
    pub firmware: String,
    pub secure_boot: Option<bool>,
    /// grub2 | grub-legacy | systemd-boot | extlinux | lilo | zipl | unknown
    pub bootloader: String,
    pub config: String,
    pub default_entry: String,
    pub grub_cmdline: String,
    pub grub_timeout: String,
    /// systemd | openrc | sysvinit | upstart | runit
    pub init: String,
    pub default_target: String,
    /// dracut | initramfs-tools | mkinitcpio | mkinitfs
    pub initramfs: String,
    /// Нестандартные параметры ядра (их нужно перенести на новую машину).
    pub custom_params: Vec<String>,
    pub boot_mount: String,
    pub efi_mount: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hardware {
    pub cpu_model: String,
    pub cpus: usize,
    pub mem_total: u64,
    /// Доступно памяти сейчас (MemAvailable).
    #[serde(default)]
    pub mem_available: u64,
    pub swap_total: u64,
    /// kvm | vmware | hyperv | xen | virtualbox | qemu | lxc | docker | openvz | none | unknown
    pub virt: String,
    pub vendor: String,
    pub product: String,
    /// Yandex Cloud, AWS, Azure, Google Cloud, OpenStack, VK Cloud…
    pub cloud: String,
    pub cloud_init: bool,
    pub guest_agent: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Disk {
    pub name: String,
    pub size: u64,
    pub model: String,
    pub rotational: bool,
    pub partitions: Vec<Part>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    pub size: u64,
    pub fstype: String,
    pub mount: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Vg {
    pub name: String,
    pub size: u64,
    pub free: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Lv {
    pub vg: String,
    pub name: String,
    pub size: u64,
    /// Атрибуты lvs (`-wi-ao----`; `V` — thin, `s` — снимок, `t` — пул).
    pub attr: String,
    #[serde(default)]
    pub path: String,
    /// Пул thin (для thin-томов).
    #[serde(default)]
    pub pool: String,
    /// Заполнение пула thin, %.
    #[serde(default)]
    pub data_percent: f64,
}

impl Lv {
    pub fn thin(&self) -> bool { self.attr.starts_with('V') }
    /// Устройство тома в виде /dev/mapper/<vg>-<lv> (дефисы в именах удваиваются).
    pub fn mapper(&self) -> String { format!("/dev/mapper/{}-{}", self.vg.replace('-', "--"), self.name.replace('-', "--")) }
    pub fn matches(&self, source: &str) -> bool { source == self.mapper() || (!self.path.is_empty() && source == self.path) || source == format!("/dev/{}/{}", self.vg, self.name) }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Zpool {
    pub name: String,
    pub size: u64,
    pub free: u64,
}

/// Разбор `vgs`, `lvs`, `zpool list` (разделитель «|», размеры в байтах).
pub fn parse_vgs(out: &str) -> Vec<Vg> {
    out.lines().filter_map(|l| { let f: Vec<&str> = l.trim().split('|').map(|x| x.trim()).collect(); (f.len() >= 3).then(|| Vg { name: f[0].into(), size: f[1].parse().unwrap_or(0), free: f[2].parse().unwrap_or(0) }) }).filter(|v| !v.name.is_empty()).collect()
}

pub fn parse_lvs(out: &str) -> Vec<Lv> {
    out.lines().filter_map(|l| {
        let f: Vec<&str> = l.trim().split('|').map(|x| x.trim()).collect();
        (f.len() >= 4).then(|| Lv { vg: f[0].into(), name: f[1].into(), size: f[2].parse().unwrap_or(0), attr: f[3].into(), path: f.get(4).unwrap_or(&"").to_string(), pool: f.get(5).unwrap_or(&"").to_string(), data_percent: f.get(6).and_then(|x| x.replace(',', ".").parse().ok()).unwrap_or(0.0) })
    }).filter(|v| !v.name.is_empty()).collect()
}

pub fn parse_zpools(out: &str) -> Vec<Zpool> {
    out.lines().filter_map(|l| { let f: Vec<&str> = l.split_whitespace().collect(); (f.len() >= 3).then(|| Zpool { name: f[0].into(), size: f[1].parse().unwrap_or(0), free: f[2].parse().unwrap_or(0) }) }).collect()
}

pub const VGS_ARGS: &[&str] = &["vgs", "--noheadings", "--nosuffix", "--units", "b", "--separator", "|", "-o", "vg_name,vg_size,vg_free"];
pub const LVS_ARGS: &[&str] = &["lvs", "--noheadings", "--nosuffix", "--units", "b", "--separator", "|", "-o", "vg_name,lv_name,lv_size,lv_attr,lv_path,pool_lv,data_percent"];
pub const ZPOOL_ARGS: &[&str] = &["zpool", "list", "-Hp", "-o", "name,size,free"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FstabEntry {
    pub spec: String,
    pub mount: String,
    pub fstype: String,
    pub options: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Storage {
    pub disks: Vec<Disk>,
    pub lvm: Vec<String>,
    /// Группы томов LVM (место под снимки).
    #[serde(default)]
    pub vgs: Vec<Vg>,
    /// Логические тома LVM.
    #[serde(default)]
    pub lvs: Vec<Lv>,
    /// Пулы ZFS.
    #[serde(default)]
    pub zpools: Vec<Zpool>,
    pub raid: Vec<String>,
    pub luks: Vec<String>,
    pub swap: Vec<String>,
    pub fstab: Vec<FstabEntry>,
    pub root_device: String,
    pub root_fstype: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Iface {
    pub name: String,
    pub mac: String,
    pub state: String,
    pub addrs: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Network {
    pub hostname: String,
    pub fqdn: String,
    pub interfaces: Vec<Iface>,
    pub gateway: String,
    pub dns: Vec<String>,
    pub search: Vec<String>,
    /// netplan | NetworkManager | ifupdown | network-scripts | systemd-networkd | wicked
    pub config_tool: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Security {
    pub selinux: String,
    pub apparmor: Option<bool>,
    pub firewall: Vec<String>,
    pub sshd_port: String,
    pub sshd_root_login: String,
    pub sshd_password_auth: String,
    pub fips: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimeInfo {
    pub timezone: String,
    /// chronyd | ntpd | systemd-timesyncd | ""
    pub ntp: String,
}

fn read(p: impl AsRef<Path>) -> String { std::fs::read_to_string(p).unwrap_or_default() }
fn exists(p: &str) -> bool { Path::new(p).exists() }

fn kv(text: &str, key: &str) -> String {
    text.lines().find_map(|l| l.trim().strip_prefix(&format!("{key}="))).map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string()).unwrap_or_default()
}

fn cmd(prog: &str, args: &[&str]) -> Option<String> {
    let o = crate::util::run_timeout(prog, args, 20)?;
    if !o.status.success() { return None; }
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Семейство по ID и ID_LIKE.
pub fn family(id: &str, like: &str) -> String {
    let all = format!("{} {}", id.to_lowercase(), like.to_lowercase());
    let has = |w: &str| all.split_whitespace().any(|x| x == w);
    if has("debian") || has("ubuntu") || has("astra") || has("linuxmint") || has("kali") || has("devuan") { "debian" }
    else if has("rhel") || has("fedora") || has("centos") || has("redos") || has("almalinux") || has("rocky") || has("ol") || has("amzn") || has("msvsphere") || has("openeuler") { "rhel" }
    else if has("suse") || has("opensuse") || has("sles") || has("sle") { "suse" }
    else if has("alpine") { "alpine" }
    else if has("arch") || has("manjaro") { "arch" }
    else if has("gentoo") { "gentoo" }
    else if has("altlinux") || has("alt") { "alt" }
    else { "other" }.to_string()
}

/// ОС: os-release (с запасным /usr/lib/os-release), затем файлы *-release старых систем.
pub fn os_from(root_read: &dyn Fn(&str) -> String) -> OsInfo {
    let mut o = OsInfo::default();
    for f in ["/etc/os-release", "/usr/lib/os-release"] {
        let t = root_read(f);
        if t.trim().is_empty() { continue; }
        o.name = kv(&t, "NAME");
        o.pretty = kv(&t, "PRETTY_NAME");
        o.id = kv(&t, "ID");
        o.id_like = kv(&t, "ID_LIKE");
        o.version = kv(&t, "VERSION");
        o.version_id = kv(&t, "VERSION_ID");
        o.codename = { let c = kv(&t, "VERSION_CODENAME"); if c.is_empty() { kv(&t, "UBUNTU_CODENAME") } else { c } };
        o.source = f.into();
        break;
    }
    // Старые системы (CentOS 6, RHEL 6, Debian 6…) и уточнения.
    let rh = root_read("/etc/redhat-release");
    if o.source.is_empty() && !rh.trim().is_empty() {
        o.pretty = rh.trim().to_string();
        o.name = rh.split(" release").next().unwrap_or("").trim().to_string();
        o.version_id = rh.split_whitespace().find(|w| w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)).unwrap_or("").to_string();
        o.id = if rh.contains("CentOS") { "centos" } else if rh.contains("Red Hat") { "rhel" } else if rh.contains("Oracle") { "ol" } else { "rhel" }.into();
        o.source = "/etc/redhat-release".into();
    }
    let deb = root_read("/etc/debian_version");
    if o.source.is_empty() && !deb.trim().is_empty() {
        o.id = "debian".into();
        o.name = "Debian GNU/Linux".into();
        o.version_id = deb.trim().to_string();
        o.pretty = format!("Debian GNU/Linux {}", deb.trim());
        o.source = "/etc/debian_version".into();
    }
    let alp = root_read("/etc/alpine-release");
    if o.source.is_empty() && !alp.trim().is_empty() {
        o.id = "alpine".into();
        o.name = "Alpine Linux".into();
        o.version_id = alp.trim().to_string();
        o.pretty = format!("Alpine Linux v{}", alp.trim());
        o.source = "/etc/alpine-release".into();
    }
    let suse = root_read("/etc/SuSE-release");
    if o.source.is_empty() && !suse.trim().is_empty() {
        o.id = "sles".into();
        o.pretty = suse.lines().next().unwrap_or("").trim().to_string();
        o.name = o.pretty.clone();
        o.version_id = suse.lines().find_map(|l| l.strip_prefix("VERSION = ")).unwrap_or("").to_string();
        o.source = "/etc/SuSE-release".into();
    }
    // Astra Linux: точная версия обновления в /etc/astra_version.
    let astra = root_read("/etc/astra_version");
    if !astra.trim().is_empty() {
        if o.id.is_empty() { o.id = "astra".into(); }
        o.version = format!("{}{}", astra.trim(), if o.version.is_empty() { String::new() } else { format!(" ({})", o.version) });
    }
    let alt = root_read("/etc/altlinux-release");
    if o.source.is_empty() && !alt.trim().is_empty() {
        o.id = "altlinux".into();
        o.pretty = alt.trim().to_string();
        o.name = o.pretty.clone();
        o.source = "/etc/altlinux-release".into();
    }
    if o.pretty.is_empty() { o.pretty = format!("{} {}", o.name, o.version).trim().to_string(); }
    if o.pretty.is_empty() { o.pretty = "Linux (дистрибутив не определён)".into(); }
    o.family = family(&o.id, &o.id_like);
    o
}

/// Параметры ядра, которые ставит сам дистрибутив или загрузчик (остальные — «свои»).
fn standard_param(p: &str) -> bool {
    let k = p.split('=').next().unwrap_or(p);
    ["BOOT_IMAGE", "root", "ro", "rw", "quiet", "splash", "rhgb", "crashkernel", "console", "resume", "rd.lvm.lv", "rd.luks.uuid", "rd.md.uuid", "rootflags", "rootfstype",
     "initrd", "LANG", "vconsole.font", "vconsole.keymap", "rd.lvm.vg", "net.ifnames", "biosdevname", "modprobe.blacklist", "nomodeset", "vga", "earlyprintk", "panic",
     "systemd.unified_cgroup_hierarchy", "plymouth.enable", "loglevel", "noresume", "showopts", "mitigations", "rd.driver.blacklist", "consoleblank", "fsck.mode", "rootdelay",
     "init", "raid", "cgroup_enable", "swapaccount", "apparmor", "security", "selinux", "audit", "ipv6.disable", "boot", "kvm-intel.nested", "elevator", "xen_blkfront.sda_is_xvda",
     "dis_ucode_ldr", "edd", "rd.shell", "rd.debug", "rd.auto", "nvme_core.io_timeout", "nvme.io_timeout", "rd.emergency", "usbcore.autosuspend", "zswap.enabled"]
        .contains(&k)
}

pub fn custom_params(cmdline: &str) -> Vec<String> {
    // После «--» — аргументы init, а не ядра.
    cmdline.split_whitespace().take_while(|p| *p != "--").filter(|p| !standard_param(p)).map(|s| s.to_string()).collect()
}

fn boot() -> BootInfo {
    let mut b = BootInfo { firmware: if exists("/sys/firmware/efi") { "uefi".into() } else { "bios".into() }, ..Default::default() };
    if b.firmware == "uefi" {
        // SecureBoot-<GUID> в efivars: последний байт 1 — включена.
        if let Ok(rd) = std::fs::read_dir("/sys/firmware/efi/efivars") {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("SecureBoot-") {
                    if let Ok(v) = std::fs::read(e.path()) { b.secure_boot = v.last().map(|x| *x == 1); }
                }
            }
        }
    }
    let grub_cfgs = ["/boot/grub2/grub.cfg", "/boot/grub/grub.cfg", "/boot/efi/EFI/redhat/grub.cfg", "/boot/efi/EFI/centos/grub.cfg", "/boot/efi/EFI/ubuntu/grub.cfg", "/boot/efi/EFI/debian/grub.cfg", "/boot/efi/EFI/rocky/grub.cfg", "/boot/efi/EFI/almalinux/grub.cfg", "/boot/efi/EFI/opensuse/grub.cfg", "/boot/efi/EFI/astra/grub.cfg", "/boot/efi/EFI/altlinux/grub.cfg"];
    if let Some(f) = grub_cfgs.iter().find(|f| exists(f)) {
        b.bootloader = "grub2".into();
        b.config = f.to_string();
    } else if exists("/boot/grub/menu.lst") || exists("/boot/grub/grub.conf") {
        b.bootloader = "grub-legacy".into();
        b.config = if exists("/boot/grub/grub.conf") { "/boot/grub/grub.conf" } else { "/boot/grub/menu.lst" }.into();
    } else if exists("/boot/loader/loader.conf") || exists("/efi/loader/loader.conf") || exists("/boot/efi/loader/loader.conf") {
        b.bootloader = "systemd-boot".into();
        b.config = ["/boot/loader/loader.conf", "/efi/loader/loader.conf", "/boot/efi/loader/loader.conf"].iter().find(|f| exists(f)).unwrap_or(&"").to_string();
    } else if exists("/boot/extlinux/extlinux.conf") || exists("/boot/extlinux.conf") || exists("/extlinux.conf") {
        b.bootloader = "extlinux".into();
        b.config = ["/boot/extlinux/extlinux.conf", "/boot/extlinux.conf", "/extlinux.conf"].iter().find(|f| exists(f)).unwrap_or(&"").to_string();
    } else if exists("/etc/zipl.conf") {
        b.bootloader = "zipl".into();
        b.config = "/etc/zipl.conf".into();
    } else if exists("/etc/lilo.conf") {
        b.bootloader = "lilo".into();
        b.config = "/etc/lilo.conf".into();
    } else {
        b.bootloader = "unknown".into();
    }
    let dg = read("/etc/default/grub");
    if !dg.is_empty() {
        let a = kv(&dg, "GRUB_CMDLINE_LINUX");
        let d = kv(&dg, "GRUB_CMDLINE_LINUX_DEFAULT");
        b.grub_cmdline = format!("{a} {d}").trim().to_string();
        b.grub_timeout = kv(&dg, "GRUB_TIMEOUT");
        b.default_entry = kv(&dg, "GRUB_DEFAULT");
    }
    if let Some(k) = cmd("grubby", &["--default-kernel"]) { b.default_entry = k; }
    let pid1 = read("/proc/1/comm").trim().to_string();
    let exe1 = std::fs::read_link("/proc/1/exe").map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    b.init = if pid1 == "systemd" || exe1.contains("systemd") { "systemd" }
        else if exists("/sbin/openrc") || exists("/sbin/openrc-run") || exists("/run/openrc") { "openrc" }
        else if exe1.contains("runit") || pid1 == "runit" { "runit" }
        else if exists("/sbin/initctl") && exists("/etc/init") { "upstart" }
        else if pid1 == "init" { "sysvinit" }
        else if pid1.is_empty() { "unknown" } else { pid1.as_str() }.to_string();
    if b.init == "systemd" {
        b.default_target = std::fs::read_link("/etc/systemd/system/default.target").ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| "graphical.target (по умолчанию)".into());
    } else if let Some(l) = read("/etc/inittab").lines().find(|l| l.contains(":initdefault:")) {
        b.default_target = format!("runlevel {}", l.split(':').nth(1).unwrap_or("?"));
    }
    b.initramfs = if exists("/etc/dracut.conf") || exists("/usr/bin/dracut") || exists("/sbin/dracut") { "dracut" }
        else if exists("/etc/initramfs-tools") { "initramfs-tools" }
        else if exists("/etc/mkinitcpio.conf") { "mkinitcpio" }
        else if exists("/etc/mkinitfs") { "mkinitfs" }
        else { "" }.into();
    b.custom_params = custom_params(&read("/proc/cmdline"));
    b
}

fn hardware() -> Hardware {
    let ci = read("/proc/cpuinfo");
    let mi = read("/proc/meminfo");
    let mem = |k: &str| mi.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    let dmi = |f: &str| read(format!("/sys/class/dmi/id/{f}")).trim().to_string();
    let vendor = dmi("sys_vendor");
    let product = dmi("product_name");
    let bios = dmi("bios_vendor");
    let chassis = dmi("chassis_asset_tag");
    let mut virt = cmd("systemd-detect-virt", &[]).unwrap_or_default();
    if virt.is_empty() || virt == "none" {
        let all = format!("{vendor} {product} {bios}").to_lowercase();
        virt = if all.contains("vmware") { "vmware" } else if all.contains("qemu") || all.contains("kvm") || all.contains("openstack") || all.contains("yandex") { "kvm" }
            else if all.contains("microsoft") && all.contains("virtual") { "microsoft" } else if all.contains("xen") || all.contains("amazon ec2") { "xen" }
            else if all.contains("virtualbox") || all.contains("innotek") { "oracle" } else if exists("/proc/vz") && !exists("/proc/bc") { "openvz" }
            else if exists("/.dockerenv") { "docker" } else if read("/proc/1/environ").contains("container=lxc") { "lxc" }
            else if ci.contains("hypervisor") { "unknown" } else { "none" }.into();
    }
    let all = format!("{vendor} {product} {bios} {chassis}").to_lowercase();
    let cloud = if all.contains("yandex") { "Yandex Cloud" } else if all.contains("amazon") || bios.to_lowercase().contains("amazon") { "AWS" }
        else if all.contains("google") { "Google Cloud" } else if chassis.contains("7783-7084-3265-9085-8269-3286-77") || (vendor.contains("Microsoft") && exists("/var/lib/waagent")) { "Azure" }
        else if all.contains("openstack") { "OpenStack" } else if all.contains("selectel") { "Selectel" } else if all.contains("vk") && all.contains("cloud") { "VK Cloud" }
        else if all.contains("digitalocean") { "DigitalOcean" } else if all.contains("hetzner") { "Hetzner" } else { "" };
    let mut ga = Vec::new();
    for (p, n) in [("/usr/bin/qemu-ga", "qemu-guest-agent"), ("/usr/sbin/qemu-ga", "qemu-guest-agent"), ("/usr/bin/vmtoolsd", "open-vm-tools"), ("/usr/sbin/hv_kvp_daemon", "hyperv-daemons"), ("/usr/sbin/waagent", "waagent"), ("/usr/bin/amazon-ssm-agent", "amazon-ssm-agent")] {
        if exists(p) && !ga.contains(&n.to_string()) { ga.push(n.to_string()); }
    }
    Hardware {
        cpu_model: ci.lines().find_map(|l| l.strip_prefix("model name").and_then(|v| v.split_once(':')).map(|x| x.1.trim().to_string())).or_else(|| ci.lines().find_map(|l| l.strip_prefix("Model").and_then(|v| v.split_once(':')).map(|x| x.1.trim().to_string()))).unwrap_or_default(),
        cpus: ci.lines().filter(|l| l.starts_with("processor")).count().max(1),
        mem_total: mem("MemTotal:"),
        mem_available: mem("MemAvailable:"),
        swap_total: mem("SwapTotal:"),
        virt,
        vendor,
        product,
        cloud: cloud.into(),
        cloud_init: exists("/etc/cloud/cloud.cfg") || exists("/var/lib/cloud"),
        guest_agent: ga,
    }
}

fn storage(mounts: &[super::inventory::Mount]) -> Storage {
    let mut s = Storage::default();
    let mount_of = |dev: &str| mounts.iter().find(|m| m.source == format!("/dev/{dev}") || m.source.ends_with(&format!("/{dev}"))).map(|m| (m.target.clone(), m.fstype.clone()));
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with("loop") && !n.starts_with("ram") && !n.starts_with("zram") && !n.starts_with("sr") && !n.starts_with("fd")).collect();
        names.sort();
        for n in names {
            let base = format!("/sys/block/{n}");
            let size = read(format!("{base}/size")).trim().parse::<u64>().unwrap_or(0) * 512;
            if size == 0 { continue; }
            let mut d = Disk { name: n.clone(), size, model: read(format!("{base}/device/model")).trim().to_string(), rotational: read(format!("{base}/queue/rotational")).trim() == "1", partitions: vec![] };
            if n.starts_with("dm-") { d.model = read(format!("{base}/dm/name")).trim().to_string(); }
            if let Ok(pr) = std::fs::read_dir(&base) {
                let mut parts: Vec<String> = pr.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|p| p.starts_with(&n) && p != &n).collect();
                parts.sort();
                for p in parts {
                    let psz = read(format!("{base}/{p}/size")).trim().parse::<u64>().unwrap_or(0) * 512;
                    let (mount, fstype) = mount_of(&p).unwrap_or_default();
                    d.partitions.push(Part { name: p, size: psz, fstype, mount });
                }
            }
            if d.partitions.is_empty() {
                if let Some((m, f)) = mount_of(&n).or_else(|| (n.starts_with("dm-")).then(|| mount_of(&format!("mapper/{}", d.model))).flatten()) {
                    d.partitions.push(Part { name: n.clone(), size, fstype: f, mount: m });
                }
            }
            s.disks.push(d);
        }
    }
    // LVM: группы томов из резервных копий метаданных (без вызова lvm).
    if let Ok(rd) = std::fs::read_dir("/etc/lvm/backup") { for e in rd.flatten() { s.lvm.push(e.file_name().to_string_lossy().to_string()); } }
    if s.lvm.is_empty() { if let Ok(rd) = std::fs::read_dir("/dev/mapper") { for e in rd.flatten() { let n = e.file_name().to_string_lossy().to_string(); if n != "control" && n.contains('-') && !n.starts_with("luks") { s.lvm.push(n); } } } }
    s.lvm.sort();
    // Группы и тома LVM, пулы ZFS — для анализа места под снимки (команды с пределом времени).
    s.vgs = cmd(VGS_ARGS[0], &VGS_ARGS[1..]).map(|o| parse_vgs(&o)).unwrap_or_default();
    if !s.vgs.is_empty() { s.lvs = cmd(LVS_ARGS[0], &LVS_ARGS[1..]).map(|o| parse_lvs(&o)).unwrap_or_default(); }
    s.zpools = cmd(ZPOOL_ARGS[0], &ZPOOL_ARGS[1..]).map(|o| parse_zpools(&o)).unwrap_or_default();
    s.raid = read("/proc/mdstat").lines().filter(|l| l.starts_with("md")).map(|l| l.trim().to_string()).collect();
    s.luks = read("/etc/crypttab").lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" ")).collect();
    s.swap = read("/proc/swaps").lines().skip(1).filter_map(|l| l.split_whitespace().next().map(|x| x.to_string())).collect();
    s.fstab = read("/etc/fstab").lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 3).then(|| FstabEntry { spec: f[0].into(), mount: f[1].into(), fstype: f[2].into(), options: f.get(3).unwrap_or(&"").to_string() })
    }).collect();
    if let Some(r) = mounts.iter().find(|m| m.target == "/") { s.root_device = r.source.clone(); s.root_fstype = r.fstype.clone(); }
    s
}

fn network() -> Network {
    let mut n = Network { hostname: read("/proc/sys/kernel/hostname").trim().to_string(), ..Default::default() };
    n.fqdn = cmd("hostname", &["-f"]).unwrap_or_default();
    // Адреса: `ip -o addr` (если есть), иначе только IPv4 из /proc/net/fib_trie.
    let ipo = cmd("ip", &["-o", "addr", "show"]).unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|x| x != "lo").collect();
        names.sort();
        for name in names {
            let base = format!("/sys/class/net/{name}");
            let addrs: Vec<String> = ipo.lines().filter(|l| l.split_whitespace().nth(1).map(|x| x.trim_end_matches(':') == name).unwrap_or(false))
                .filter_map(|l| { let w: Vec<&str> = l.split_whitespace().collect(); w.iter().position(|x| *x == "inet" || *x == "inet6").and_then(|i| w.get(i + 1)).map(|a| a.to_string()) })
                .filter(|a| !a.starts_with("fe80")).collect();
            n.interfaces.push(Iface { mac: read(format!("{base}/address")).trim().to_string(), state: read(format!("{base}/operstate")).trim().to_string(), addrs, name });
        }
    }
    for l in read("/proc/net/route").lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            if let Ok(g) = u32::from_str_radix(f[2], 16) { n.gateway = format!("{} ({})", std::net::Ipv4Addr::from(g.swap_bytes()), f[0]); break; }
        }
    }
    let rc = read("/etc/resolv.conf");
    n.dns = rc.lines().filter_map(|l| l.strip_prefix("nameserver").map(|x| x.trim().to_string())).collect();
    n.search = rc.lines().filter_map(|l| l.strip_prefix("search").map(|x| x.trim().to_string())).collect();
    for (p, t) in [("/etc/netplan", "netplan"), ("/etc/NetworkManager/system-connections", "NetworkManager"), ("/etc/network/interfaces", "ifupdown"), ("/etc/sysconfig/network-scripts", "network-scripts"), ("/etc/systemd/network", "systemd-networkd"), ("/etc/sysconfig/network/ifcfg-eth0", "wicked")] {
        if let Ok(md) = std::fs::metadata(p) {
            let nonempty = if md.is_dir() { std::fs::read_dir(p).map(|mut d| d.next().is_some()).unwrap_or(false) } else { md.len() > 0 };
            if nonempty { n.config_tool.push(t.into()); }
        }
    }
    n
}

/// Какие из служб активны — одним вызовом `systemctl is-active a b c` (строка на службу).
fn active_units(units: &[&str]) -> Vec<String> {
    let mut args = vec!["is-active"];
    args.extend_from_slice(units);
    let Some(o) = crate::util::run_timeout("systemctl", &args, 20) else { return vec![] };
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    units.iter().zip(text.lines()).filter(|(_, st)| st.trim() == "active").map(|(u, _)| u.to_string()).collect()
}

fn security() -> Security {
    let mut s = Security::default();
    let enforce = read("/sys/fs/selinux/enforce");
    s.selinux = if !enforce.is_empty() { if enforce.trim() == "1" { "enforcing" } else { "permissive" }.into() }
        else { let c = kv(&read("/etc/selinux/config"), "SELINUX"); if c.is_empty() { String::new() } else { format!("{c} (не загружен)") } };
    let aa = read("/sys/module/apparmor/parameters/enabled");
    if !aa.is_empty() { s.apparmor = Some(aa.trim() == "Y"); }
    let fw = ["firewalld", "ufw", "nftables", "iptables", "ferm", "shorewall"];
    let act = active_units(&fw);
    for f in fw { if act.contains(&f.to_string()) { s.firewall.push(f.into()); } }
    if s.firewall.is_empty() && read("/etc/ufw/ufw.conf").contains("ENABLED=yes") { s.firewall.push("ufw".into()); }
    let mut sshd = read("/etc/ssh/sshd_config");
    if let Ok(rd) = std::fs::read_dir("/etc/ssh/sshd_config.d") { for e in rd.flatten() { sshd = read(e.path()) + "\n" + &sshd; } }
    let opt = |k: &str| sshd.lines().map(|l| l.trim()).filter(|l| !l.starts_with('#')).find_map(|l| { let mut w = l.split_whitespace(); (w.next().map(|x| x.eq_ignore_ascii_case(k)).unwrap_or(false)).then(|| w.next().unwrap_or("").to_string()) }).unwrap_or_default();
    s.sshd_port = { let p = opt("Port"); if p.is_empty() { "22".into() } else { p } };
    s.sshd_root_login = { let p = opt("PermitRootLogin"); if p.is_empty() { "по умолчанию (prohibit-password)".into() } else { p } };
    s.sshd_password_auth = { let p = opt("PasswordAuthentication"); if p.is_empty() { "по умолчанию (yes)".into() } else { p } };
    s.fips = read("/proc/sys/crypto/fips_enabled").trim() == "1";
    s
}

fn time() -> TimeInfo {
    let tz = { let t = read("/etc/timezone").trim().to_string(); if !t.is_empty() { t } else { std::fs::read_link("/etc/localtime").ok().map(|p| p.to_string_lossy().split("zoneinfo/").nth(1).unwrap_or("").to_string()).unwrap_or_default() } };
    let tz = if tz.is_empty() { kv(&read("/etc/sysconfig/clock"), "ZONE") } else { tz };
    let units = ["chronyd", "chrony", "ntpd", "ntp", "systemd-timesyncd", "openntpd"];
    let act = active_units(&units);
    let ntp = units.iter().find(|s| act.contains(&s.to_string()))
        .map(|s| s.to_string())
        .or_else(|| ["/usr/sbin/chronyd", "/usr/sbin/ntpd"].iter().find(|p| exists(p)).map(|p| format!("{} (установлен)", p.rsplit('/').next().unwrap_or(""))))
        .unwrap_or_default();
    TimeInfo { timezone: tz, ntp }
}

fn repos() -> Vec<String> {
    let mut out = Vec::new();
    for l in read("/etc/apt/sources.list").lines().chain(std::fs::read_dir("/etc/apt/sources.list.d").map(|rd| rd.flatten().map(|e| read(e.path())).collect::<Vec<_>>().join("\n")).unwrap_or_default().lines()) {
        let l = l.trim();
        if l.starts_with("deb ") { out.push(l.split_whitespace().filter(|w| !w.starts_with('[')).skip(1).take(2).collect::<Vec<_>>().join(" ")); }
        if let Some(u) = l.strip_prefix("URIs:") { out.push(u.trim().to_string()); }
    }
    for d in ["/etc/yum.repos.d", "/etc/zypp/repos.d"] {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let t = read(e.path());
                let mut cur = String::new();
                for l in t.lines() {
                    let l = l.trim();
                    if l.starts_with('[') { cur = l.trim_matches(|c| c == '[' || c == ']').to_string(); }
                    if l.replace(' ', "") == "enabled=1" && !cur.is_empty() { out.push(cur.clone()); }
                }
            }
        }
    }
    for l in read("/etc/apk/repositories").lines() { if !l.trim().is_empty() && !l.starts_with('#') { out.push(l.trim().to_string()); } }
    out.sort();
    out.dedup();
    out
}

pub fn collect(mounts: &[super::inventory::Mount]) -> SystemInfo {
    let os = os_from(&|p: &str| read(p));
    let un = |f: &str| read(format!("/proc/sys/kernel/{f}")).trim().to_string();
    let arch = cmd("uname", &["-m"]).unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let mut installed: Vec<String> = std::fs::read_dir("/boot").map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("vmlinuz-") || n.starts_with("vmlinux-")).map(|n| n.splitn(2, '-').nth(1).unwrap_or("").to_string()).collect()).unwrap_or_default();
    if installed.is_empty() {
        installed = std::fs::read_dir("/lib/modules").map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    }
    installed.sort();
    let locale = { let l = kv(&read("/etc/default/locale"), "LANG"); if !l.is_empty() { l } else { kv(&read("/etc/locale.conf"), "LANG") } };
    let mut b = boot();
    for m in mounts {
        if m.target == "/boot" { b.boot_mount = format!("{} ({})", m.source, m.fstype); }
        if m.target == "/boot/efi" || m.target == "/efi" { b.efi_mount = format!("{} ({})", m.source, m.fstype); }
    }
    SystemInfo {
        os,
        kernel: KernelInfo { release: un("osrelease"), version: un("version"), arch, cmdline: read("/proc/cmdline").trim().to_string(), installed, modules_loaded: read("/proc/modules").lines().count() },
        boot: b,
        hardware: hardware(),
        storage: storage(mounts),
        network: network(),
        security: security(),
        time: time(),
        uptime_secs: read("/proc/uptime").split_whitespace().next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as u64,
        locale,
        repos: repos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(files: &[(&str, &str)]) -> OsInfo {
        let m: std::collections::HashMap<String, String> = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        os_from(&|p: &str| m.get(p).cloned().unwrap_or_default())
    }

    #[test]
    fn distros() {
        let o = os(&[("/etc/os-release", "NAME=\"CentOS Linux\"\nVERSION=\"7 (Core)\"\nID=\"centos\"\nID_LIKE=\"rhel fedora\"\nVERSION_ID=\"7\"\nPRETTY_NAME=\"CentOS Linux 7 (Core)\"\n")]);
        assert_eq!((o.family.as_str(), o.version_id.as_str()), ("rhel", "7"));
        let o = os(&[("/etc/redhat-release", "CentOS release 6.10 (Final)\n")]);
        assert_eq!((o.id.as_str(), o.version_id.as_str(), o.family.as_str()), ("centos", "6.10", "rhel"));
        let o = os(&[("/etc/os-release", "PRETTY_NAME=\"Astra Linux\"\nNAME=\"Astra Linux\"\nID=astra\nID_LIKE=debian\nVERSION_ID=1.7_x86-64\nVERSION_CODENAME=1.7_x86-64\n"), ("/etc/astra_version", "1.7.5\n")]);
        assert!(o.family == "debian" && o.version.starts_with("1.7.5"));
        let o = os(&[("/etc/os-release", "NAME=\"RED OS\"\nVERSION=\"7.3\"\nID=\"redos\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"7.3\"\nPRETTY_NAME=\"RED OS 7.3\"\n")]);
        assert_eq!(o.family, "rhel");
        let o = os(&[("/etc/os-release", "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.19.1\nPRETTY_NAME=\"Alpine Linux v3.19\"\n")]);
        assert_eq!(o.family, "alpine");
        let o = os(&[("/etc/os-release", "NAME=\"SLES\"\nVERSION=\"15-SP5\"\nVERSION_ID=\"15.5\"\nPRETTY_NAME=\"SUSE Linux Enterprise Server 15 SP5\"\nID=\"sles\"\nID_LIKE=\"suse\"\n")]);
        assert_eq!(o.family, "suse");
        let o = os(&[("/etc/os-release", "NAME=\"ALT Server\"\nVERSION=\"10.1\"\nID=altlinux\nVERSION_ID=10.1\nPRETTY_NAME=\"ALT Server 10.1 (Mendelevium)\"\n")]);
        assert_eq!(o.family, "alt");
        let o = os(&[("/etc/debian_version", "8.11\n")]);
        assert_eq!((o.id.as_str(), o.family.as_str()), ("debian", "debian"));
        let o = os(&[]);
        assert_eq!(o.family, "other");
        assert_eq!(custom_params("BOOT_IMAGE=/vmlinuz-6.1 root=UUID=x ro quiet splash hugepages=1024 intel_iommu=on transparent_hugepage=never"), vec!["hugepages=1024", "intel_iommu=on", "transparent_hugepage=never"]);
    }
}
