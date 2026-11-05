//! Анализ описи: что на этой машине нужно копировать, какими продуктами и как, что учесть при
//! восстановлении и сколько места понадобится. Рекомендации с `catalog_id` можно сразу превратить
//! в экземпляры (параметры уже заполнены по описи).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::inventory::Inventory;
use crate::util::human_bytes;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rec {
    pub id: String,
    /// high | medium | low
    pub priority: String,
    pub title: String,
    pub reason: String,
    /// instance — создать экземпляр; agent — установить агента; manual — сделать вручную.
    pub action: String,
    pub catalog_id: String,
    pub config: Value,
    pub schedule: String,
    pub rpo_min: u32,
    pub keep_last: u32,
    pub available: bool,
    pub details: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Note {
    /// warn | info
    pub level: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Estimate {
    pub parts: Vec<(String, u64)>,
    pub first: u64,
    /// Часть объёмов посчитана не до конца — на самом деле больше.
    pub over: bool,
    /// Место на площадке при хранении 7 копий (с учётом дедупликации, грубо).
    pub site_7: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Analysis {
    pub summary: Vec<(String, String)>,
    pub recommendations: Vec<Rec>,
    pub warnings: Vec<Note>,
    pub restore_notes: Vec<String>,
    pub estimate: Estimate,
    /// Что донастроить на машине перед копиями: место под снимки ФС, память, процессор, место для
    /// агента и WAL, время, сеть.
    #[serde(default)]
    pub prepare: Vec<Prep>,
}

/// Пункт подготовки машины.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Prep {
    /// ok | info | warn | bad
    pub level: String,
    /// Снимки | Память | Процессор | Диск | Время | Сеть
    pub area: String,
    pub title: String,
    pub text: String,
    /// Команды, которые нужно выполнить на машине (по порядку).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Пошаговый план (где нажать в гипервизоре, команды, проверка) — для сложных случаев.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// Чего не делать (например, какие диски не трогать).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caution: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Step {
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub commands: Vec<String>,
}

fn step(title: impl Into<String>, text: impl Into<String>, commands: Vec<String>) -> Step { Step { title: title.into(), text: text.into(), commands } }

fn prep(level: &str, area: &str, title: impl Into<String>, text: impl Into<String>) -> Prep {
    Prep { level: level.into(), area: area.into(), title: title.into(), text: text.into(), commands: vec![], steps: vec![], caution: String::new() }
}

fn round_gib(n: u64) -> u64 { n.div_ceil(1 << 30) * (1 << 30) }

/// Имя, под которым появится новый диск (следующая буква после занятых), и занятые диски того же вида.
fn next_disk(st: &super::system::Storage) -> (String, Vec<String>) {
    let names: Vec<String> = st.disks.iter().map(|d| d.name.trim_start_matches("/dev/").to_string()).collect();
    for pre in ["sd", "vd", "xvd"] {
        let mut used: Vec<String> = names.iter().filter(|n| n.starts_with(pre) && n.len() == pre.len() + 1 && n.as_bytes()[pre.len()].is_ascii_lowercase()).cloned().collect();
        if used.is_empty() { continue; }
        used.sort();
        let last = used.last().unwrap().as_bytes()[pre.len()];
        let next = if last < b'z' { format!("/dev/{pre}{}", (last + 1) as char) } else { format!("/dev/{pre}X") };
        return (next, used.iter().map(|u| format!("/dev/{u}")).collect());
    }
    if names.iter().any(|n| n.starts_with("nvme")) {
        return ("/dev/nvmeXnY".into(), names.iter().filter(|n| n.starts_with("nvme")).map(|u| format!("/dev/{u}")).collect());
    }
    ("/dev/sdX".into(), names.iter().map(|u| format!("/dev/{u}")).collect())
}

/// Занятые диски для предупреждения: «/dev/sdb (80 ГБ: /var/lib/postgresql)».
fn busy_disks(st: &super::system::Storage, busy: &[String]) -> Vec<String> {
    busy.iter().map(|b| {
        let n = b.trim_start_matches("/dev/");
        match st.disks.iter().find(|d| d.name.trim_start_matches("/dev/") == n) {
            Some(d) => {
                let mounts: Vec<String> = d.partitions.iter().filter(|p| !p.mount.is_empty()).map(|p| p.mount.clone()).collect();
                let lvm = d.partitions.iter().any(|p| p.mount.is_empty() && p.fstype.is_empty());
                let what = match (mounts.is_empty(), lvm) {
                    (false, true) => format!("{}, LVM и др.", mounts.join(", ")),
                    (false, false) => mounts.join(", "),
                    (true, true) => "LVM или разделы без точки монтирования".into(),
                    (true, false) => "используется".into(),
                };
                format!("{b} ({}: {what})", human_bytes(d.size))
            }
            None => b.clone(),
        }
    }).collect()
}

/// Где добавить диск к машине — по виду виртуализации или облаку.
fn add_disk_hint(inv: &Inventory, gib: u64) -> String {
    let hw = &inv.system.hardware;
    let cloud = hw.cloud.to_lowercase();
    let product = format!("{} {}", hw.vendor, hw.product).to_lowercase();
    if cloud.contains("yandex") { return format!("Консоль Yandex Cloud: Compute Cloud → Диски → «Создать диск» ({gib} ГБ, в той же зоне), затем ВМ → «Подключить диск». Диск появится без перезагрузки."); }
    if cloud.contains("aws") || cloud.contains("amazon") { return format!("Консоль AWS: EC2 → Volumes → Create volume ({gib} GiB, в той же зоне доступности) → Attach volume к этой ВМ."); }
    if !cloud.is_empty() { return format!("Консоль облака ({}): создайте диск {gib} ГБ и подключите его к этой ВМ.", hw.cloud); }
    match hw.virt.as_str() {
        "vmware" => format!("vSphere Client: ВМ → Edit Settings → Add New Device → Hard Disk, {gib} ГБ, контроллер — тот же, что у остальных дисков (обычно VMware Paravirtual). Перезагрузка не нужна."),
        "kvm" | "qemu" if product.contains("proxmox") || product.contains("qemu") => format!("Proxmox VE: ВМ → Hardware → Add → Hard Disk ({gib} ГБ, шина SCSI или VirtIO — как у остальных дисков). zVirt/oVirt: ВМ → Диски → «Новый». libvirt: virsh vol-create-as … && virsh attach-disk … --persistent. Перезагрузка не нужна."),
        "kvm" | "qemu" => format!("Платформа виртуализации (Proxmox, zVirt/oVirt, libvirt, OpenStack…): добавьте ВМ диск {gib} ГБ на той же шине, что и остальные (SCSI или VirtIO). Перезагрузка не нужна."),
        "microsoft" | "hyperv" => format!("Hyper-V Manager: ВМ → Settings → SCSI Controller → Hard Drive → Add → New Virtual Hard Disk ({gib} ГБ)."),
        "xen" => format!("XCP-ng / Citrix: ВМ → Storage → Add disk ({gib} ГБ)."),
        "none" | "" => format!("Физический сервер: подключите свободный диск (или том СХД) не меньше {gib} ГБ."),
        v => format!("Добавьте к машине диск {gib} ГБ средствами платформы ({v})."),
    }
}

/// Точка монтирования, на которой лежит путь.
fn mount_of<'a>(inv: &'a Inventory, path: &str) -> Option<&'a super::inventory::Mount> {
    inv.mounts.iter().filter(|m| m.class != "pseudo" && m.class != "docker" && under(path, &m.target)).max_by_key(|m| m.target.len())
}

/// Подготовка машины: место под снимки ФС для томов, которые попадут в файловую копию, и прочие ресурсы.
fn prepare(inv: &Inventory, a: &Analysis, covered: &[String], has_pg: bool) -> Vec<Prep> {
    let mut out = Vec::new();
    let st = &inv.system.storage;
    let pct = 10u64;
    // Тома под файловой копией.
    let mut mounts: Vec<&super::inventory::Mount> = Vec::new();
    for p in covered { if let Some(m) = mount_of(inv, p) { if !mounts.iter().any(|x| x.target == m.target) { mounts.push(m); } } }
    let mut vg_need: Vec<(String, u64, Vec<String>)> = Vec::new();
    for m in &mounts {
        if m.class == "network" {
            out.push(prep("info", "Снимки", format!("{} — сетевой каталог", m.target), format!("{} ({}) копируется без снимка: снимок делается на файловом сервере.", m.target, m.fstype)));
            continue;
        }
        let free = m.total.saturating_sub(m.used);
        let free_pct = if m.total > 0 { free * 100 / m.total } else { 100 };
        match m.fstype.as_str() {
            "btrfs" => {
                if free_pct < 10 {
                    let mut p = prep("warn", "Снимки", format!("{}: мало места для снимка btrfs", m.target), format!("Снимок btrfs занимает место в той же ФС по мере изменения данных. Свободно {} ({free_pct}%) — освободите место или расширьте ФС до 10–15% свободного.", human_bytes(free)));
                    p.commands.push(format!("btrfs filesystem usage {}", m.target));
                    out.push(p);
                } else {
                    out.push(prep("ok", "Снимки", format!("{}: снимок btrfs", m.target), format!("Файлы будут читаться из снимка btrfs; свободно {} ({free_pct}%).", human_bytes(free))));
                }
            }
            "zfs" => {
                let pool = m.source.split('/').next().unwrap_or("").to_string();
                let zp = st.zpools.iter().find(|z| z.name == pool);
                let (zfree, zpct) = zp.map(|z| (z.free, if z.size > 0 { z.free * 100 / z.size } else { 100 })).unwrap_or((free, free_pct));
                if zpct < 10 {
                    out.push(prep("warn", "Снимки", format!("{}: мало места в пуле ZFS {pool}", m.target), format!("Снимок ZFS растёт по мере изменения данных. В пуле свободно {} ({zpct}%) — освободите место или добавьте диск в пул (zpool add).", human_bytes(zfree))));
                } else {
                    out.push(prep("ok", "Снимки", format!("{}: снимок ZFS", m.target), format!("Файлы будут читаться из снимка ZFS; в пуле {pool} свободно {} ({zpct}%).", human_bytes(zfree))));
                }
            }
            _ => match st.lvs.iter().find(|lv| lv.matches(&m.source)) {
                Some(lv) if lv.thin() => {
                    if lv.data_percent > 80.0 {
                        out.push(prep("warn", "Снимки", format!("{}: пул thin {} заполнен на {:.0}%", m.target, lv.pool, lv.data_percent), "Снимок thin берёт место из пула по мере изменения данных; при заполнении пула записи на тома останавливаются. Расширьте пул: lvextend -L +N <группа>/<пул>.".to_string()));
                    } else {
                        out.push(prep("ok", "Снимки", format!("{}: снимок LVM thin", m.target), format!("Том {}/{} — thin: снимок без отдельного места (пул заполнен на {:.0}%).", lv.vg, lv.name, lv.data_percent)));
                    }
                }
                Some(lv) => {
                    let need = crate::fssnap::lvm_snap_size(lv.size, pct);
                    match vg_need.iter_mut().find(|x| x.0 == lv.vg) {
                        Some(x) => { x.1 += need; x.2.push(format!("{} ({}/{}, {}) — {}", m.target, lv.vg, lv.name, human_bytes(lv.size), human_bytes(need))); }
                        None => vg_need.push((lv.vg.clone(), need, vec![format!("{} ({}/{}, {}) — {}", m.target, lv.vg, lv.name, human_bytes(lv.size), human_bytes(need))])),
                    }
                }
                None if m.target == "/boot" || m.target.starts_with("/boot/") => {
                    out.push(prep("info", "Снимки", format!("{}: без снимка", m.target), format!("Том {} ({}) не в LVM — копируется без снимка. Здесь ядра и загрузчик: они меняются только при обновлении системы, поэтому копия получается согласованной; переносить {} в LVM не нужно.", m.source, m.fstype, m.target)));
                }
                None => {
                    let mut p = prep("warn", "Снимки", format!("{}: снимок ФС невозможен", m.target), format!("Том {} ({}) не в LVM, ZFS или btrfs — файлы будут копироваться «на ходу»: файлы, которые меняются во время копии, могут попасть в неё в разном состоянии. Базы данных это не затрагивает — они копируются своими средствами (pg_basebackup, дампы). Для согласованной файловой копии перенесите данные на том LVM.", m.source, m.fstype));
                    p.commands = vec![
                        "# новый диск /dev/sdX под LVM (имена — примеры):".into(),
                        "pvcreate /dev/sdX && vgcreate data /dev/sdX".into(),
                        format!("lvcreate -n vol -l 90%FREE data && mkfs.{} /dev/data/vol", if m.fstype == "xfs" { "xfs" } else { "ext4" }),
                        "# перенос данных, запись в /etc/fstab; 10% группы оставьте свободными под снимки".into(),
                    ];
                    out.push(p);
                }
            },
        }
    }
    // Место для агента (спул WAL): если его том — LVM в той же группе, план расширения — общий с местом под снимки.
    let agent_need: u64 = if has_pg { 5 << 30 } else { 1 << 30 };
    let agent_short = mount_of(inv, "/var/lib/hopebackup").and_then(|m| {
        let free = m.total.saturating_sub(m.used);
        (m.total > 0 && free < agent_need).then(|| (m, round_gib(agent_need - free)))
    });
    let agent_lv = agent_short.and_then(|(m, add)| st.lvs.iter().find(|lv| lv.matches(&m.source) && !lv.thin()).map(|lv| (m, lv, add)));
    let mut agent_planned = false;
    for (vg, need, list) in vg_need {
        let free = st.vgs.iter().find(|v| v.name == vg).map(|v| v.free).unwrap_or(0);
        if free >= need {
            out.push(prep("ok", "Снимки", format!("Группа томов {vg}: места под снимки хватает"), format!("Свободно {}, на снимки нужно ≈{} ({pct}% каждого тома, не меньше 1 ГиБ): {}.", human_bytes(free), human_bytes(need), list.join("; "))));
        } else {
            let add = round_gib((need - free) + need / 5);
            // Тот же диск — и под место агенту, если его том в этой группе.
            let grow = agent_lv.filter(|(_, lv, _)| lv.vg == vg).map(|(m, lv, a)| (m.target.clone(), format!("{}/{}", lv.vg, lv.name), a));
            // Диск меньше 5 ГБ добавлять смысла нет: запас на рост данных и следующие тома.
            let disk = (add + grow.as_ref().map(|g| g.2).unwrap_or(0)).max(5 << 30);
            let gib = disk >> 30;
            let mut p = prep("bad", "Снимки", format!("Группа томов {vg}: добавьте к машине диск ≈{gib} ГБ"), format!(
                "На снимки нужно ≈{} ({pct}% каждого тома, не меньше 1 ГиБ; это место под изменения данных, пока идёт копия), а в группе {vg} свободно {}. Тома: {}. Не хватает {}; с запасом на рост данных — {}.{} Без места снимок не создаётся — копия пойдёт «на ходу» (или не пойдёт в режиме «снимок обязателен»). Проще всего добавить к машине один новый диск и включить его в группу — по шагам ниже, без перезагрузки и остановки служб.",
                human_bytes(need), human_bytes(free), list.join("; "), human_bytes(need - free), human_bytes(add),
                grow.as_ref().map(|(t, _, a)| format!(" Агенту не хватает места в {t} — ещё {} из того же диска.", human_bytes(*a))).unwrap_or_default()));
            let (nd, busy) = next_disk(st);
            let busy_txt = busy_disks(st, &busy);
            if !busy_txt.is_empty() {
                p.caution = format!("Не трогайте диски, которые уже заняты: {}. Команда pvcreate уничтожает данные на диске. Новый диск, скорее всего, появится как {nd} — проверьте по lsblk: у него нет разделов и точки монтирования, размер — {gib} ГБ.", busy_txt.join("; "));
            }
            let scsi = nd.starts_with("/dev/sd");
            p.steps.push(step(format!("Добавьте к машине диск {gib} ГБ"), add_disk_hint(inv, gib), vec![]));
            let mut see = Vec::new();
            if scsi { see.push("for h in /sys/class/scsi_host/host*/scan; do echo '- - -' > $h; done   # увидеть новый диск без перезагрузки".to_string()); }
            see.push(format!("lsblk   # новый диск {nd}: {gib}G, без разделов и точки монтирования"));
            p.steps.push(step("Найдите новый диск", if scsi { "Пересканируйте шину SCSI и убедитесь, что диск появился и он пустой." } else { "Диск virtio появляется сам; убедитесь, что он пустой." }, see));
            p.steps.push(step(format!("Включите диск в группу {vg}"), format!("Подставьте имя нового диска, если оно не {nd}."), vec![format!("pvcreate {nd}"), format!("vgextend {vg} {nd}")]));
            if let Some((t, lvn, a)) = &grow {
                p.steps.push(step(format!("Расширьте {t} для агента"), format!("Том и файловая система растут без остановки (ext4 и xfs). Агенту нужно ≥{} свободного: спул WAL копит журналы, пока площадка недоступна.", human_bytes(agent_need)), vec![format!("lvextend -r -L +{}G {lvn}", a >> 30)]));
                agent_planned = true;
            }
            let mut check = vec![format!("vgs {vg}   # VFree ≥ {}", human_bytes(need))];
            if let Some((t, _, _)) = &grow { check.push(format!("df -h {t}   # свободно ≥ {}", human_bytes(agent_need))); }
            p.steps.push(step("Проверьте", "", check));
            p.steps.push(step("Обновите анализ", "В панели — «Опись» у этой машины ещё раз: пункты про снимки и место станут зелёными.", vec![]));
            p.text += " Если добавить диск нельзя: уменьшите «Размер снимка LVM» в параметрах экземпляра (когда запись во время копии небольшая) или оставьте копию «на ходу».";
            out.push(p);
        }
    }
    // Память: агенту во время копии нужно до ~150 МиБ (пакет блоков ≤32 МиБ, их шифротекст, pack-файл 16 МиБ).
    let hw = &inv.system.hardware;
    let avail = if hw.mem_available > 0 { hw.mem_available } else { hw.mem_total };
    if avail > 0 {
        if avail < 256 << 20 {
            let mut p = prep("bad", "Память", format!("Свободно {} памяти — мало для копии", human_bytes(avail)), "Агенту во время копии нужно до 150 МБ (нарезка на блоки, сжатие, шифрование, отправка). При нехватке ядро может завершить агента или приложение. Добавьте ВМ памяти или подкачку:".to_string());
            p.commands = vec!["fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile".into(), "echo '/swapfile none swap sw 0 0' >> /etc/fstab".into()];
            out.push(p);
        } else if avail < 512 << 20 {
            out.push(prep("warn", "Память", format!("Свободно {} памяти", human_bytes(avail)), "Агенту хватит (до 150 МБ во время копии), но запаса мало: ставьте копии на время низкой нагрузки."));
        } else {
            out.push(prep("ok", "Память", format!("Свободно {} памяти", human_bytes(avail)), "Агенту во время копии нужно до 150 МБ — хватает."));
        }
    }
    if hw.cpus == 1 {
        out.push(prep("info", "Процессор", "Один vCPU", "Сжатие и шифрование займут процессор на время копии — расписание на ночь. На быстром канале можно снизить уровень сжатия площадки."));
    } else if hw.cpus > 1 {
        out.push(prep("ok", "Процессор", format!("{} vCPU", hw.cpus), "Сжатие и шифрование идут в несколько потоков."));
    }
    // Место для агента и спула WAL.
    if let Some(m) = mount_of(inv, "/var/lib/hopebackup") {
        let free = m.total.saturating_sub(m.used);
        let need: u64 = if has_pg { 5 << 30 } else { 1 << 30 };
        if m.total > 0 && free < need {
            let mut p = prep("warn", "Диск", format!("Мало места в {} для агента", m.target), format!("Свободно {}; агенту {}. Освободите место или вынесите /var/lib/hopebackup на другой диск.", human_bytes(free), if has_pg { "нужно ≥5 ГБ: спул WAL копит журналы, пока площадка недоступна" } else { "нужно ≥1 ГБ (программа, состояние)" }));
            if agent_planned {
                p.text += " Расширение входит в план для группы томов выше (шаг «Расширьте … для агента»).";
            } else if let Some(lv) = st.lvs.iter().find(|lv| lv.matches(&m.source) && !lv.thin()) {
                let add = round_gib(need - free);
                p.text += &format!(" Том {} в группе {}: можно расширить его на {} без остановки — добавьте это место к диску, который включаете в группу под снимки.", m.target, lv.vg, human_bytes(add));
                p.commands = vec![format!("lvextend -r -L +{}G {}/{}   # расширить том и файловую систему", add >> 30, lv.vg, lv.name), format!("df -h {}", m.target)];
            }
            out.push(p);
        } else if m.total > 0 {
            out.push(prep("ok", "Диск", format!("Место для агента: свободно {}", human_bytes(free)), if has_pg { "Спулу WAL хватит на перерывы связи с площадкой (нужно ≥5 ГБ).".to_string() } else { "Агенту нужно ≥1 ГБ.".to_string() }));
        }
    }
    if inv.system.time.ntp.is_empty() || inv.system.time.ntp.contains("установлен") {
        let mut p = prep("warn", "Время", "Синхронизация времени не работает", "Расписания, окно восстановления на момент времени и проверка сертификатов зависят от точного времени. Включите синхронизацию:".to_string());
        p.commands = vec!["timedatectl set-ntp true   # systemd-timesyncd".into(), "# или: apt install chrony / dnf install chrony; systemctl enable --now chronyd".into()];
        out.push(p);
    }
    // Сеть: оценка первой копии.
    let first = a.estimate.first;
    if first > 0 {
        let t = |bps: f64| -> String { let secs = first as f64 * 8.0 / bps; if secs < 3600.0 { format!("{:.0} мин", (secs / 60.0).ceil()) } else { format!("{:.1} ч", secs / 3600.0) } };
        out.push(prep("info", "Сеть", format!("Первая копия ≈{}", human_bytes(first)), format!("По каналу 100 Мбит/с — около {}, по 1 Гбит/с — около {} (до сжатия и дедупликации). Дальше идут только изменения. Машине нужны исходящие соединения: к серверу управления (4443/tcp) и к площадке (8443/tcp у узла HopeBackup, 443/tcp у S3 и облаков) — или коннектор (SSH-туннель, VPN).", t(100e6), t(1e9))));
    }
    out
}

fn rec(id: &str, prio: &str, title: &str, reason: &str) -> Rec {
    Rec { id: id.into(), priority: prio.into(), title: title.into(), reason: reason.into(), action: "manual".into(), config: json!({}), keep_last: 7, available: true, ..Default::default() }
}

fn under(path: &str, dir: &str) -> bool { dir == "/" || path == dir || path.starts_with(&format!("{}/", dir.trim_end_matches('/'))) }

const DB_IMAGES: &[&str] = &["postgres", "mysql", "mariadb", "mongo", "redis", "clickhouse", "elasticsearch", "opensearch", "cassandra", "influxdb", "timescale", "neo4j"];

/// `has_agent` — на машине уже работает агент HopeBackup.
fn trusted_pg_exe(p: &str) -> bool {
    (p.starts_with("/usr/") || p.starts_with("/opt/")) && !p.contains("/../") && !p.contains("/./") && !p.contains(char::is_whitespace)
}

pub fn analyze(inv: &Inventory, has_agent: bool) -> Analysis {
    let mut a = Analysis::default();
    let sys = &inv.system;
    // ---------------------------------------------------------- сводка
    let os = if sys.os.pretty.is_empty() { inv.os.clone() } else { sys.os.pretty.clone() };
    a.summary.push(("ОС".into(), format!("{os} · ядро {} · {}", if sys.kernel.release.is_empty() { &inv.kernel } else { &sys.kernel.release }, sys.kernel.arch)));
    if !sys.boot.firmware.is_empty() {
        a.summary.push(("Загрузка".into(), format!("{}{} · {} · init {}{}", sys.boot.firmware.to_uppercase(), match sys.boot.secure_boot { Some(true) => ", Secure Boot", _ => "" }, sys.boot.bootloader, sys.boot.init, if sys.boot.initramfs.is_empty() { String::new() } else { format!(" · {}", sys.boot.initramfs) })));
    }
    if sys.hardware.cpus > 0 {
        let virt = match sys.hardware.virt.as_str() { "none" => "физический сервер".to_string(), "" => String::new(), v => format!("ВМ ({v}{})", if sys.hardware.cloud.is_empty() { String::new() } else { format!(", {}", sys.hardware.cloud) }) };
        a.summary.push(("Железо".into(), format!("{} vCPU · {} ОЗУ · {virt}", sys.hardware.cpus, human_bytes(sys.hardware.mem_total))));
    }
    if !sys.storage.root_fstype.is_empty() {
        let mut extra = Vec::new();
        if !sys.storage.lvm.is_empty() { extra.push("LVM"); }
        if !sys.storage.raid.is_empty() { extra.push("RAID"); }
        if !sys.storage.luks.is_empty() { extra.push("LUKS"); }
        a.summary.push(("Диски".into(), format!("{} · корень {} ({}){}", sys.storage.disks.len(), sys.storage.root_device, sys.storage.root_fstype, if extra.is_empty() { String::new() } else { format!(" · {}", extra.join(", ")) })));
    }
    let own_sched = inv.schedules.iter().filter(|s| !s.packaged).count();
    a.summary.push(("Найдено".into(), format!("приложений не из пакетов {}, своих заданий по расписанию {own_sched}, своих служб {}, контейнеров {}, пользователей {}", inv.apps.len(), inv.services.len(), inv.containers.len(), inv.users.len())));

    // ---------------------------------------------------------- агент
    if !has_agent {
        let mut r = rec("agent", "high", "Установить агента HopeBackup", "Копии снимает агент на самой машине: данные идут сразу на площадку, сжатые и зашифрованные. Опись по SSH агента не требует, копии — требуют.");
        r.action = "agent".into();
        r.available = sys.kernel.arch.is_empty() || sys.kernel.arch == "x86_64" || sys.kernel.arch == "aarch64";
        if !r.available { r.details.push(format!("архитектура {} пока не поддерживается агентом (есть сборки для x86_64 и ARM64)", sys.kernel.arch)); }
        if sys.os.family == "alpine" { r.details.push("Alpine: ставится статическая сборка (musl), gcompat не нужен; служба — OpenRC".into()); }
        a.recommendations.push(r);
    }

    // ---------------------------------------------------------- сервер целиком
    let homes_total: u64 = inv.sizes.homes.iter().map(|h| h.bytes).sum();
    let homes_over = inv.sizes.homes.iter().any(|h| h.over);
    let big_homes = homes_total > 50 << 30 || homes_over;
    let app_dirs: Vec<String> = inv.apps.iter().filter(|x| x.include).map(|x| x.path.clone()).collect();
    let compose: Vec<String> = inv.containers.iter().filter(|c| !c.compose_dir.is_empty()).map(|c| c.compose_dir.clone()).collect();
    let db_containers: Vec<&super::inventory::Container> = inv.containers.iter().filter(|c| DB_IMAGES.iter().any(|d| c.image.to_lowercase().split(['/', ':']).any(|p| p.starts_with(d)))).collect();
    let named_volumes = inv.containers.iter().any(|c| c.mounts.iter().any(|m| m.kind == "volume")) && db_containers.is_empty();
    // Диски, на которых живут найденные программы и compose-проекты, — в копию.
    let mut mounts_in: Vec<String> = Vec::new();
    let mut mounts_ask: Vec<&super::inventory::Mount> = Vec::new();
    for m in inv.mounts.iter().filter(|m| m.class == "disk") {
        let used_by_apps = app_dirs.iter().chain(compose.iter()).any(|p| under(p, &m.target)) || inv.processes.iter().any(|p| !p.packaged && under(&p.cwd, &m.target));
        if used_by_apps { mounts_in.push(m.target.clone()); } else if m.used > 1 << 30 && !inv.db_data_dirs.iter().any(|d| under(d, &m.target)) { mounts_ask.push(m); }
    }
    let docker = if inv.containers.is_empty() { "none" } else if named_volumes { "volumes" } else { "compose" };
    let mut host = rec("host", "high", "Сервер Linux целиком", "Всё, что нужно, чтобы поднять машину заново: /etc, пользователи с паролями и ключами SSH, домашние каталоги, свои программы и скрипты, задания cron и службы, список пакетов.");
    host.action = "instance".into();
    host.catalog_id = "host".into();
    host.schedule = "0 1 * * *".into();
    host.rpo_min = 1440;
    host.config = json!({ "home": if big_homes { "ssh" } else { "all" }, "include_root": true, "auto_apps": true, "app_limit_mb": 2048, "extra_paths": [], "mounts": mounts_in, "docker": docker, "skip_db_data": true, "exclude": [], "use_global_exclude": true, "snapshot": "auto" });
    host.details.push(format!("пользователи: {} (с ключами SSH: {})", inv.users.len(), inv.users.iter().filter(|u| !u.ssh_files.is_empty()).count()));
    host.details.push(format!("домашние каталоги: {}{}", human_bytes(homes_total), if big_homes { " — большие, по умолчанию только ключи SSH и профили" } else { " — целиком" }));
    if !app_dirs.is_empty() { host.details.push(format!("приложения: {}{}", app_dirs.iter().take(6).cloned().collect::<Vec<_>>().join(", "), if app_dirs.len() > 6 { " …" } else { "" })); }
    if own_sched > 0 { host.details.push(format!("свои задания по расписанию: {own_sched}, их скрипты — в копии")); }
    if !mounts_in.is_empty() { host.details.push(format!("диски с данными приложений: {}", mounts_in.join(", "))); }
    match docker { "volumes" => host.details.push("Docker: compose-проекты и тома".into()), "compose" => host.details.push("Docker: compose-проекты".into()), _ => {} }
    if !inv.db_data_dirs.is_empty() { host.details.push(format!("каталоги данных СУБД исключены: {}", inv.db_data_dirs.join(", "))); }
    // Журналы — в копии, только если /var/log попадает в неё отдельным диском (или вместе с /var).
    if mounts_in.iter().any(|m| m == "/var/log" || m == "/var") {
        host.details.push("журналы /var/log копируются (диск с данными); не нужны — добавьте /var/log в «Исключения»".into());
    } else {
        host.details.push("журналы /var/log не копируются; нужны — добавьте их каталоги в «Дополнительные пути» (например, /var/log/nginx)".into());
    }
    a.recommendations.push(host);
    if big_homes {
        let mut r = rec("homes", "medium", "Домашние каталоги целиком", &format!("Домашние каталоги занимают {}{} — по умолчанию копируются только ключи SSH и профили.", human_bytes(homes_total), if homes_over { " и больше" } else { "" }));
        r.action = "config".into();
        r.catalog_id = "host".into();
        r.config = json!({ "home": "all" });
        for h in inv.sizes.homes.iter().take(8) { r.details.push(format!("{} — {}{}", h.path, human_bytes(h.bytes), if h.over { "+" } else { "" })); }
        a.recommendations.push(r);
    }
    for m in &mounts_ask {
        let mut r = rec(&format!("mount:{}", m.target), "medium", &format!("Диск {} ({} занято)", m.target, human_bytes(m.used)), "Отдельный диск с данными, не связанный с найденными программами. Если на нём рабочие данные — добавьте его к копии сервера.");
        r.action = "config".into();
        r.catalog_id = "host".into();
        r.config = json!({ "mounts_add": [m.target] });
        r.details.push(format!("{} · {}", m.source, m.fstype));
        a.recommendations.push(r);
    }
    for app in inv.apps.iter().filter(|x| !x.include) {
        let mut r = rec(&format!("app:{}", app.path), "medium", &format!("Каталог приложения {} целиком", app.path), &format!("Каталог больше предела ({}{}): сейчас копируются только файлы запуска.", human_bytes(app.size), if app.size_over { "+" } else { "" }));
        r.action = "config".into();
        r.catalog_id = "host".into();
        r.config = json!({ "extra_paths_add": [app.path] });
        r.details = app.sources.clone();
        a.recommendations.push(r);
    }

    // ---------------------------------------------------------- продукты
    let listen_port = |names: &[&str]| inv.listening.iter().find(|l| names.iter().any(|n| l.process.starts_with(n))).map(|l| l.port);
    for p in &inv.products {
        match p.id.as_str() {
            "postgresql" => {
                // Каталог программ — только из системных мест установки (/usr, /opt) и от процесса пользователя
                // postgres/root; иначе пусто: агент сам найдёт каталог и проверит, что он принадлежит root.
                let pg = inv.processes.iter().find(|x| (x.exe.ends_with("/postgres") || x.exe.ends_with("/postmaster")) && trusted_pg_exe(&x.exe) && ["postgres", "root", "qdatabase", "0", "gitlab-psql"].contains(&x.user.as_str()));
                let bin = pg.map(|x| x.exe.rsplit_once('/').map(|y| y.0.to_string()).unwrap_or_default()).unwrap_or_default();
                let qdb = bin.contains("qdatabase");
                // Встроенная PostgreSQL GitLab Omnibus: свой каталог сокета и суперпользователь gitlab-psql.
                let gitlab = bin.starts_with("/opt/gitlab/");
                let mut r = rec("postgresql", "high", if qdb { "qdatabase (СУБД)" } else if gitlab { "PostgreSQL GitLab (встроенная)" } else { "PostgreSQL" }, "СУБД работает: файловая копия её каталога несогласованна. Плагин снимает копию pg_basebackup и непрерывно отправляет WAL — восстановление на любой момент времени.");
                r.action = "instance".into();
                r.catalog_id = if qdb { "qdatabase" } else { "postgresql" }.into();
                r.schedule = "0 1 * * *".into();
                r.rpo_min = 15;
                let port = listen_port(&["postgres", "postmaster"]).unwrap_or(5432);
                r.config = if gitlab {
                    json!({ "bin_dir": bin, "port": port, "user": "gitlab-psql", "os_user": "gitlab-psql", "host": "/var/opt/gitlab/postgresql" })
                } else {
                    json!({ "bin_dir": bin, "port": port, "user": "postgres", "os_user": "postgres", "host": "/var/run/postgresql" })
                };
                if gitlab { r.details.push("встроенная PostgreSQL GitLab: сокет /var/opt/gitlab/postgresql, суперпользователь gitlab-psql, перезапуск — gitlab-ctl restart postgresql".into()); }
                r.details.push(format!("каталог программ {bin}, порт {port}; после создания — «Обнаружить» и «Настроить архив WAL»"));
                for d in &inv.db_data_dirs { if d.contains("postgres") || d.contains("pgsql") || d.contains("qdatabase") { r.details.push(format!("каталог данных {d}")); } }
                a.recommendations.push(r);
            }
            "gitlab" => {
                let c = inv.containers.iter().find(|c| c.image.contains("gitlab"));
                let mut r = rec("gitlab", "high", "GitLab", "gitlab-backup с секретами (gitlab-secrets.json) и конфигурацией — без них CI-переменные, токены и 2FA не восстановить.");
                r.action = "instance".into();
                r.catalog_id = "gitlab".into();
                r.schedule = "0 2 * * *".into();
                r.config = match c { Some(c) => json!({ "mode": "docker", "container": c.name }), None => json!({ "mode": "omnibus" }) };
                a.recommendations.push(r);
            }
            "vault" => {
                let port = listen_port(&["vault", "bao"]).unwrap_or(8200);
                let mut r = rec("vault", "high", "Vault / OpenBao", "Снапшот raft через API — согласованная копия хранилища секретов.");
                r.action = "instance".into();
                r.catalog_id = "vault".into();
                r.schedule = "0 */6 * * *".into();
                r.rpo_min = 360;
                r.config = json!({ "addr": format!("https://127.0.0.1:{port}") });
                r.details.push("нужен токен с правом read на sys/storage/raft/snapshot — укажите его в параметрах".into());
                a.recommendations.push(r);
            }
            "nginx" | "haproxy" | "openvpn" => {
                let mut r = rec(&p.id, "low", &format!("{} — отдельный экземпляр", p.title), "Конфигурация уже попадает в копию сервера (/etc). Отдельный экземпляр нужен, если хотите восстанавливать её с проверкой (nginx -t, haproxy -c) и перезагрузкой службы.");
                r.action = "instance".into();
                r.catalog_id = p.id.clone();
                r.schedule = "0 1 * * *".into();
                a.recommendations.push(r);
            }
            "s3" => {
                let port = listen_port(&["minio"]).unwrap_or(9000);
                let mut r = rec("s3", "medium", "MinIO: бакеты", "Объекты бакетов с метаданными копируются через API (нужен ключ на чтение).");
                r.action = "instance".into();
                r.catalog_id = "s3".into();
                r.config = json!({ "endpoint": format!("http://127.0.0.1:{port}"), "path_style": true });
                a.recommendations.push(r);
            }
            id => {
                // Продукты со своими плагинами: экземпляр с подсказками в параметрах.
                let (prio, sched, rpo, why, cfg): (&str, &str, u32, &str, Value) = match id {
                    "mysql" => ("high", "0 1 * * *", 60, "Согласованный дамп или mariabackup и архив binlog — восстановление на любой момент.", json!({ "socket": "/run/mysqld/mysqld.sock", "user": "root", "method": "auto", "binlog": true })),
                    "mssql" => ("high", "0 1 * * *", 60, "BACKUP DATABASE и журналы транзакций — цепочка и восстановление на момент.", json!({ "host": "localhost", "user": "sa" })),
                    "mongodb" => ("high", "0 1 * * *", 1440, "mongodump с oplog (набор реплик) — согласованно, без остановки.", json!({ "uri": format!("mongodb://127.0.0.1:{}", listen_port(&["mongod"]).unwrap_or(27017)) })),
                    "redis" => ("medium", "0 */6 * * *", 360, "Снимок RDB по протоколу репликации — без остановки и без доступа к каталогу.", json!({ "host": "127.0.0.1", "port": listen_port(&["redis-server", "valkey"]).unwrap_or(6379) })),
                    "clickhouse" => ("high", "0 2 * * *", 1440, "BACKUP на диск копий или схема и данные Native.", json!({ "host": "127.0.0.1", "port": 9000 })),
                    "etcd" | "kubernetes" => ("high", "0 */6 * * *", 360, "Объекты кластера, снимок etcd и тома hostPath.", json!({ "etcd": "auto" })),
                    "docker" => ("medium", "0 1 * * *", 1440, "Тома копируются с паузой контейнеров (согласованно), проекты compose — с файлами .env.", json!({ "quiesce": "pause", "compose": true })),
                    "openldap" => ("high", "0 1 * * *", 1440, "slapcat cn=config и баз — восстановление slapadd в чистый каталог.", json!({ "mode": "openldap" })),
                    "freeipa" => ("high", "0 1 * * *", 1440, "ipa-backup — штатная копия FreeIPA (данные на ходу).", json!({ "mode": "freeipa", "ipa_backup": "data" })),
                    "zabbix" => ("high", "0 1 * * *", 1440, "Дамп базы Zabbix (по желанию без истории), конфигурация и скрипты оповещений.", json!({ "product": "zabbix" })),
                    "grafana" => ("medium", "0 1 * * *", 1440, "grafana.db согласованной копией SQLite, плагины и provisioning.", json!({ "product": "grafana" })),
                    "libvirt" => ("high", "0 1 * * *", 1440, "Образы ВМ: внешний снимок дисков без остановки, слои qcow2.", json!({ "platform": "libvirt", "all_vms": true })),
                    "proxmox" => ("high", "0 1 * * *", 1440, "vzdump ВМ и контейнеров потоком в репозиторий (без сжатия — дедупликация).", json!({ "platform": "proxmox", "all_vms": true })),
                    "keycloak" => ("medium", "0 1 * * *", 1440, "Выгрузка realm с пользователями и конфигурация; базу добавьте плагином СУБД.", json!({ "export_mode": "live" })),
                    "jenkins" => ("medium", "0 1 * * *", 1440, "JENKINS_HOME без рабочих каталогов и кэшей.", json!({})),
                    "nexus" => ("medium", "0 2 * * *", 1440, "БД и настройки согласованно, хранилища blob на ходу.", json!({ "product": "nexus" })),
                    "opensearch" => ("medium", "0 2 * * *", 1440, "Snapshot API: снимок в репозиторий fs и его каталог в копию.", json!({ "url": format!("https://127.0.0.1:{}", listen_port(&["java"]).unwrap_or(9200)) })),
                    "hopemail" | "hopegw" | "hopeinv" | "hopedfs" | "hopedc" => ("high", "0 1 * * *", 1440, "Продукт Hope: данные согласованно (снимок ФС или короткая остановка), секреты и программа.", json!({ "consistency": "auto" })),
                    _ => continue,
                };
                let mut r = rec(id, prio, &p.title, why);
                r.action = "instance".into();
                r.catalog_id = match id { "openldap" => "freeipa", "grafana" => "zabbix", "etcd" => "kubernetes", "libvirt" | "proxmox" => "vm", x => x }.to_string();
                r.schedule = sched.into();
                r.rpo_min = rpo;
                r.config = cfg;
                if !p.note.is_empty() { r.details.push(p.note.clone()); }
                a.recommendations.push(r);
            }
        }
    }
    for c in &db_containers {
        a.warnings.push(Note { level: "warn".into(), text: format!("контейнер {} ({}) — СУБД в контейнере: копия тома на ходу несогласованна; добавьте экземпляр плагина СУБД с параметром «Контейнер Docker» = {} (или Docker с паузой контейнеров)", c.name, c.image, c.name) });
    }

    // ---------------------------------------------------------- предупреждения
    for m in inv.mounts.iter().filter(|m| m.class == "network") {
        a.warnings.push(Note { level: "info".into(), text: format!("сетевой каталог {} ({} {}) — копируйте его на файловом сервере-источнике, а не с каждого клиента", m.target, m.fstype, m.source) });
    }
    if sys.time.ntp.is_empty() && !sys.os.id.is_empty() {
        a.warnings.push(Note { level: "warn".into(), text: "не найдена синхронизация времени (chrony, ntpd, timesyncd): расписания и восстановление на момент времени зависят от точных часов".into() });
    }
    if !inv.as_root && inv.method != "" {
        a.warnings.push(Note { level: "warn".into(), text: "опись снята без прав root: пароли пользователей, чужие домашние каталоги и часть файлов не видны — дайте пользователю sudo".into() });
    }
    if inv.method == "shell" {
        a.warnings.push(Note { level: "warn".into(), text: "сокращённая опись (без программы HopeBackup на машине): процессы, расписания и приложения разобраны не полностью".into() });
    }
    for w in &inv.warnings { if w.contains("больше предела") || w.contains("СУБД") { continue; } a.warnings.push(Note { level: "info".into(), text: w.clone() }); }

    // ---------------------------------------------------------- восстановление на новую машину
    if !sys.os.pretty.is_empty() {
        a.restore_notes.push(format!("Новая ВМ — тот же дистрибутив и версия: {} (ядро {}), архитектура {}.", sys.os.pretty, sys.kernel.release, sys.kernel.arch));
    }
    if !sys.boot.firmware.is_empty() {
        a.restore_notes.push(format!("Загрузка {}{}: шаблон ВМ должен загружаться так же; загрузчик {} ставит шаблон.", sys.boot.firmware.to_uppercase(), if sys.boot.secure_boot == Some(true) { " с Secure Boot" } else { "" }, sys.boot.bootloader));
    }
    if !sys.boot.custom_params.is_empty() {
        a.restore_notes.push(format!("Свои параметры ядра: {} — добавьте их в GRUB_CMDLINE_LINUX на новой машине (/etc/default/grub при восстановлении не заменяется).", sys.boot.custom_params.join(" ")));
    }
    if !sys.storage.lvm.is_empty() || !sys.storage.raid.is_empty() || !sys.storage.luks.is_empty() {
        a.restore_notes.push("Разметка: LVM/RAID/шифрование создаются шаблоном новой ВМ, данные восстанавливаются поверх; fstab и crypttab не заменяются.".into());
    }
    let addrs: Vec<String> = sys.network.interfaces.iter().flat_map(|i| i.addrs.iter().map(move |a| format!("{} {a}", i.name))).collect();
    if !addrs.is_empty() {
        a.restore_notes.push(format!("Сеть не переносится (новая ВМ получит свой адрес). Прежние адреса: {}; настройка — {}.", addrs.join(", "), if sys.network.config_tool.is_empty() { "не определена".into() } else { sys.network.config_tool.join(", ") }));
    }
    let net_fstab: Vec<String> = sys.storage.fstab.iter().filter(|f| ["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs", "glusterfs", "ceph"].contains(&f.fstype.as_str())).map(|f| format!("{} → {}", f.spec, f.mount)).collect();
    if !net_fstab.is_empty() { a.restore_notes.push(format!("Сетевые каталоги из fstab подключите заново: {}.", net_fstab.join(", "))); }
    if !inv.packages.manual.is_empty() {
        a.restore_notes.push(format!("{} пакетов установлено вручную — при полном восстановлении на тот же дистрибутив ставятся из описи («Установить пакеты»).", inv.packages.manual.len()));
    }
    if sys.security.selinux.starts_with("enforcing") || sys.security.selinux.starts_with("permissive") {
        a.restore_notes.push("SELinux: после полного восстановления файлы перемаркируются при следующей загрузке (autorelabel).".into());
    }
    if sys.hardware.cloud_init { a.restore_notes.push("cloud-init: настройки /etc/cloud не переносятся — новая ВМ получает свои.".into()); }

    // ---------------------------------------------------------- оценка
    let mut est = Estimate::default();
    let s = &inv.sizes;
    est.parts.push(("/etc".into(), s.etc.bytes));
    est.parts.push(("/root".into(), s.root_home.bytes));
    est.parts.push(("/usr/local".into(), s.usr_local.bytes));
    est.parts.push((if big_homes { "ключи и профили".to_string() } else { "домашние каталоги".to_string() }, if big_homes { 0 } else { homes_total }));
    let apps_total: u64 = inv.apps.iter().filter(|x| x.include).map(|x| x.size).sum();
    est.parts.push(("приложения".into(), apps_total));
    for m in &mounts_in {
        if let Some(mm) = inv.mounts.iter().find(|x| &x.target == m) {
            // Приложения на этом диске уже посчитаны выше.
            let apps_here: u64 = inv.apps.iter().filter(|x| x.include && under(&x.path, m)).map(|x| x.size).sum();
            est.parts.push((m.clone(), mm.used.saturating_sub(apps_here)));
        }
    }
    est.over = s.etc.over || s.root_home.over || s.usr_local.over || (!big_homes && homes_over) || inv.apps.iter().any(|x| x.include && x.size_over);
    est.first = est.parts.iter().map(|p| p.1).sum();
    est.site_7 = est.first + est.first * 3 / 10;
    a.estimate = est;

    // Подготовка машины: тома под файловой копией «сервера целиком».
    let mut covered: Vec<String> = vec!["/".into(), "/etc".into()];
    if !big_homes { for u in inv.users.iter().filter(|u| u.login && !u.home.is_empty()) { covered.push(u.home.clone()); } }
    covered.extend(mounts_in.iter().cloned());
    covered.extend(app_dirs.iter().cloned());
    let has_pg = inv.products.iter().any(|p| p.id == "postgresql");
    a.prepare = prepare(inv, &a, &covered, has_pg);

    // Порядок: важное первым.
    let rank = |p: &str| match p { "high" => 0, "medium" => 1, _ => 2 };
    a.recommendations.sort_by_key(|r| rank(&r.priority));
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    /// ВМ Zabbix в vSphere: система на LVM (sda), база — целиком на sdb, мало места под снимки и в /var.
    #[test]
    fn plan_for_vmware_vm() {
        use super::super::system::{Disk, Lv, Part, Vg};
        let mut inv = Inventory { method: "agent".into(), as_root: true, ..Default::default() };
        inv.system.hardware.virt = "vmware".into();
        inv.system.hardware.mem_available = 2 << 30;
        let g = 1u64 << 30;
        inv.mounts.push(Mount { target: "/".into(), class: "system".into(), source: "/dev/mapper/vg_root-lv_root".into(), fstype: "ext4".into(), total: 10 * g, used: 4 * g, ..Default::default() });
        inv.mounts.push(Mount { target: "/var".into(), class: "system".into(), source: "/dev/mapper/vg_root-lv_var".into(), fstype: "ext4".into(), total: 3 * g, used: g / 2 + g / 10, ..Default::default() });
        inv.mounts.push(Mount { target: "/var/lib/postgresql".into(), class: "disk".into(), source: "/dev/sdb".into(), fstype: "ext4".into(), total: 80 * g, used: 20 * g, ..Default::default() });
        inv.system.storage.disks.push(Disk { name: "sda".into(), size: 20 * g, partitions: vec![
            Part { name: "sda1".into(), size: g / 2, fstype: "vfat".into(), mount: "/boot/efi".into() },
            Part { name: "sda2".into(), size: g, fstype: "ext4".into(), mount: "/boot".into() },
            Part { name: "sda3".into(), size: 18 * g, ..Default::default() }], ..Default::default() });
        inv.system.storage.disks.push(Disk { name: "sdb".into(), size: 80 * g, partitions: vec![Part { name: "sdb".into(), size: 80 * g, fstype: "ext4".into(), mount: "/var/lib/postgresql".into() }], ..Default::default() });
        inv.system.storage.vgs.push(Vg { name: "vg_root".into(), size: 18 * g, free: g / 2 });
        inv.system.storage.lvs.push(Lv { vg: "vg_root".into(), name: "lv_root".into(), size: 10 * g, attr: "-wi-ao----".into(), ..Default::default() });
        inv.system.storage.lvs.push(Lv { vg: "vg_root".into(), name: "lv_var".into(), size: 3 * g, attr: "-wi-ao----".into(), ..Default::default() });
        // Опись для просмотра в панели (проверка вида): HB_DUMP_INV=<файл> cargo test plan_for_vmware_vm.
        if let Ok(f) = std::env::var("HB_DUMP_INV") { let _ = std::fs::write(f, serde_json::to_vec(&serde_json::json!({ "inventory": inv })).unwrap()); }
        let pr = prepare(&inv, &Analysis::default(), &["/".to_string(), "/var".to_string()], true);
        let vg = pr.iter().find(|p| p.title.contains("vg_root")).expect("план для vg_root");
        let cmds: Vec<String> = vg.steps.iter().flat_map(|x| x.commands.clone()).collect();
        assert!(vg.steps[0].text.contains("vSphere"), "{}", vg.steps[0].text);
        assert!(cmds.contains(&"pvcreate /dev/sdc".to_string()) && cmds.contains(&"vgextend vg_root /dev/sdc".to_string()), "{cmds:?}");
        assert!(cmds.iter().any(|c| c.starts_with("lvextend -r -L +3G vg_root/lv_var")), "{cmds:?}");
        assert!(vg.caution.contains("/dev/sdb (80.0 ГБ: /var/lib/postgresql)") && vg.caution.contains("/dev/sda"), "{}", vg.caution);
        assert!(!cmds.iter().any(|c| c.contains("/dev/sdb") && !c.starts_with('#')), "команды не должны трогать sdb: {cmds:?}");
        let agent = pr.iter().find(|p| p.title.contains("для агента")).expect("место агента");
        assert!(agent.text.contains("входит в план") && agent.commands.is_empty(), "{}", agent.text);
        let boot = pr.iter().find(|p| p.title.starts_with("/boot"));
        assert!(boot.is_none() || boot.unwrap().level == "info");
    }

    #[test]
    fn new_disk_name() {
        let mut st = super::super::system::Storage::default();
        for n in ["sda", "sdb"] { st.disks.push(super::super::system::Disk { name: n.into(), ..Default::default() }); }
        let (n, busy) = next_disk(&st);
        assert_eq!(n, "/dev/sdc");
        assert_eq!(busy, vec!["/dev/sda", "/dev/sdb"]);
    }
    use super::super::inventory::*;

    #[test]
    fn recommendations() {
        let mut inv = Inventory { method: "ssh".into(), as_root: true, ..Default::default() };
        inv.system.kernel.arch = "x86_64".into();
        inv.system.os.pretty = "Ubuntu 22.04".into();
        inv.system.boot.firmware = "uefi".into();
        inv.system.boot.custom_params = vec!["hugepages=512".into()];
        inv.products.push(Product { id: "postgresql".into(), title: "PostgreSQL".into(), plugin: "postgresql".into(), available: true, note: String::new() });
        inv.products.push(Product { id: "mysql".into(), title: "MySQL".into(), plugin: "mysql".into(), available: false, note: String::new() });
        inv.processes.push(Proc { exe: "/usr/lib/postgresql/16/bin/postgres".into(), user: "postgres".into(), packaged: true, ..Default::default() });
        inv.listening.push(Listen { proto: "tcp".into(), addr: "0.0.0.0".into(), port: 5433, pid: 1, process: "postgres".into() });
        inv.apps.push(App { path: "/srv/app".into(), files: vec!["/srv/app/run.sh".into()], include: true, size: 1000, ..Default::default() });
        inv.mounts.push(Mount { target: "/srv".into(), class: "disk".into(), used: 5 << 30, ..Default::default() });
        inv.mounts.push(Mount { target: "/data".into(), class: "disk".into(), used: 50 << 30, source: "/dev/sdc1".into(), fstype: "xfs".into(), ..Default::default() });
        inv.mounts.push(Mount { target: "/mnt/nfs".into(), class: "network".into(), fstype: "nfs4".into(), ..Default::default() });
        let a = analyze(&inv, false);
        let ids: Vec<&str> = a.recommendations.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"agent") && ids.contains(&"host") && ids.contains(&"postgresql") && ids.contains(&"mysql") && ids.contains(&"mount:/data"));
        let host = a.recommendations.iter().find(|r| r.id == "host").unwrap();
        assert_eq!(host.config["mounts"], json!(["/srv"]));
        let pg = a.recommendations.iter().find(|r| r.id == "postgresql").unwrap();
        assert_eq!((pg.config["port"].as_u64(), pg.config["bin_dir"].as_str()), (Some(5433), Some("/usr/lib/postgresql/16/bin")));
        assert!(a.restore_notes.iter().any(|n| n.contains("hugepages=512")) && a.restore_notes.iter().any(|n| n.contains("UEFI")));
        assert!(a.warnings.iter().any(|w| w.text.contains("/mnt/nfs")));
        assert_eq!(analyze(&inv, true).recommendations.iter().filter(|r| r.id == "agent").count(), 0);
        // GitLab Omnibus: встроенная PostgreSQL — свой сокет и gitlab-psql.
        inv.processes.retain(|p| !p.exe.ends_with("/postgres"));
        inv.processes.push(Proc { exe: "/opt/gitlab/embedded/postgresql/14/bin/postgres".into(), user: "gitlab-psql".into(), ..Default::default() });
        let pg = analyze(&inv, true).recommendations.into_iter().find(|r| r.id == "postgresql").unwrap();
        assert_eq!((pg.config["host"].as_str(), pg.config["user"].as_str(), pg.config["os_user"].as_str(), pg.config["bin_dir"].as_str()),
            (Some("/var/opt/gitlab/postgresql"), Some("gitlab-psql"), Some("gitlab-psql"), Some("/opt/gitlab/embedded/postgresql/14/bin")));
    }

    #[test]
    fn prepare_snapshots_and_resources() {
        use super::super::system::{Lv, Vg};
        let mut inv = Inventory { method: "agent".into(), as_root: true, ..Default::default() };
        inv.system.hardware.cpus = 1;
        inv.system.hardware.mem_available = 200 << 20;
        inv.mounts.push(Mount { target: "/".into(), class: "system".into(), source: "/dev/mapper/vg--sys-root".into(), fstype: "ext4".into(), total: 40 << 30, used: 20 << 30, ..Default::default() });
        inv.mounts.push(Mount { target: "/srv".into(), class: "disk".into(), source: "/dev/sdc1".into(), fstype: "xfs".into(), total: 100 << 30, used: 10 << 30, ..Default::default() });
        inv.apps.push(App { path: "/srv/app".into(), include: true, size: 1000, ..Default::default() });
        inv.system.storage.vgs.push(Vg { name: "vg-sys".into(), size: 41 << 30, free: 1 << 29 });
        inv.system.storage.lvs.push(Lv { vg: "vg-sys".into(), name: "root".into(), size: 40 << 30, attr: "-wi-ao----".into(), ..Default::default() });
        for n in ["sda", "sdc"] { inv.system.storage.disks.push(super::super::system::Disk { name: n.into(), ..Default::default() }); }
        let a = analyze(&inv, true);
        let vg = a.prepare.iter().find(|p| p.title.contains("vg-sys")).expect("пункт про группу томов");
        assert_eq!(vg.level, "bad");
        // Новый диск — следующая буква после занятых (sdc уже под /srv — его pvcreate уничтожил бы данные).
        let cmds: Vec<&String> = vg.steps.iter().flat_map(|x| x.commands.iter()).collect();
        assert!(cmds.iter().any(|c| *c == "vgextend vg-sys /dev/sdd"), "{cmds:?}");
        assert!(vg.caution.contains("/dev/sdc") && vg.caution.contains("/dev/sdd"), "{}", vg.caution);
        assert!(vg.text.contains("4.0 ГБ"), "нужно 10% от 40 ГБ: {}", vg.text);
        assert!(a.prepare.iter().any(|p| p.title.contains("/srv") && p.title.contains("невозможен")));
        assert!(a.prepare.iter().any(|p| p.area == "Память" && p.level == "bad"));
        assert!(a.prepare.iter().any(|p| p.area == "Процессор" && p.level == "info"));
        // Места хватает.
        inv.system.storage.vgs[0].free = 8 << 30;
        let a = analyze(&inv, true);
        assert_eq!(a.prepare.iter().find(|p| p.title.contains("vg-sys")).unwrap().level, "ok");
    }
}
