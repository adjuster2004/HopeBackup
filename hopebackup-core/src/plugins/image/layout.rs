//! Разбор разделов диска (sysfs) и томов LVM (pvs/lvs) и построение карты подмен Overlay: какие участки
//! диска читать с самого диска «на ходу», какие — со снимка тома LVM (чтобы том был в согласованном
//! состоянии), а какие отдать нулями (подкачка).
//!
//! Планировщик `plan_disk` — чистая функция (разобранные данные → список участков), её проверяют тесты.
//! Сбор данных (`gather`) выполняет команды на машине. Низкоуровневого доступа к блочным устройствам здесь
//! нет: разделы берём из `/sys/class/block`, тома LVM — из `pvs`/`lvs`, согласованность — снимком
//! `lvcreate -s`, чтение — обычным последовательным чтением файла устройства или снимка.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

/// Откуда читать участок диска.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Source {
    /// С самого диска (раздел без снимка — читается «на ходу»).
    Live,
    /// Нулями (подкачка): не читаем и не передаём.
    Zero,
    /// Со снимка тома LVM: устройство снимка и смещение внутри него.
    Snapshot { dev: String, off: u64 },
}

/// Непрерывный участок диска и его источник.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Region {
    pub start: u64,
    pub len: u64,
    pub source: Source,
}

/// Роль раздела при планировании.
#[derive(Clone, Debug, PartialEq)]
pub enum Role {
    Swap,
    /// Физический том LVM (имя устройства раздела, как в `seg_pe_ranges`).
    Pv(String),
    /// Файловая система (тип) — читается «на ходу».
    Fs(String),
    /// Прочее (загрузчик, пусто) — читается «на ходу».
    Other,
}

/// Раздел диска: смещение и размер в байтах.
#[derive(Clone, Debug)]
pub struct Part { pub dev: String, pub start: u64, pub size: u64, pub role: Role }

/// Физический том: смещение данных (первого экстента) в PV и размер экстента, байт.
#[derive(Clone, Debug)]
pub struct Pv { pub pe_start: u64, pub ext: u64 }

/// Сегмент тома: логический том, тип, и диапазоны физических экстентов по PV.
#[derive(Clone, Debug)]
pub struct Lv { pub vg: String, pub name: String, pub linear: bool, pub segs: Vec<Seg> }
/// Один линейный сегмент: PV, первый и последний физический экстент (включительно), логический
/// стартовый экстент тома.
#[derive(Clone, Debug)]
pub struct Seg { pub pv: String, pub pe_first: u64, pub pe_last: u64, pub le_first: u64 }

/// Режим согласованности томов LVM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LvmMode { Auto, Required, Off }

impl LvmMode {
    pub fn parse(s: &str) -> LvmMode { match s { "required" => LvmMode::Required, "off" => LvmMode::Off, _ => LvmMode::Auto } }
}

/// Какой том снять снимком (для реального `lvcreate -s`): `vg/name` и желаемое имя снимка.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapReq { pub vg: String, pub lv: String, pub snap: String }

/// План одного диска: участки, какие тома снять снимком, предупреждения.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub regions: Vec<Region>,
    pub snaps: Vec<SnapReq>,
    pub warnings: Vec<String>,
}

/// Построить план диска. `snap_dev(vg, lv)` — как будет называться устройство снимка тома (чтобы участки
/// сразу ссылались на него). Нелинейные тома (чередование, зеркало, thin) снимком не берутся — их участки
/// читаются «на ходу» (в режиме `Required` это ошибка).
pub fn plan_disk(
    disk: &str,
    size: u64,
    parts: &[Part],
    pvs: &BTreeMap<String, Pv>,
    lvs: &[Lv],
    mode: LvmMode,
    snap_dev: &dyn Fn(&str, &str) -> String,
) -> Result<Plan> {
    let mut plan = Plan::default();
    // Участки-подмены (снимок или нули) поверх базового Live. Собираем, потом раскладываем по порядку.
    let mut over: Vec<Region> = Vec::new();

    for p in parts {
        match &p.role {
            Role::Swap => over.push(Region { start: p.start, len: p.size, source: Source::Zero }),
            Role::Pv(_) if mode == LvmMode::Off => plan.warnings.push(format!("{}: том LVM читается на ходу (режим LVM отключён)", p.dev)),
            _ => {}
        }
    }

    if mode != LvmMode::Off {
        for lv in lvs {
            // Сегменты этого тома, лежащие на разделах ЭТОГО диска.
            let on_this = lv.segs.iter().all(|s| parts.iter().any(|p| matches!(&p.role, Role::Pv(pv) if *pv == s.pv)));
            if !lv.segs.iter().any(|s| parts.iter().any(|p| matches!(&p.role, Role::Pv(pv) if *pv == s.pv))) {
                continue; // том не на этом диске
            }
            let can = lv.linear && on_this && lv.segs.iter().all(|s| pvs.contains_key(&s.pv));
            if !can {
                let msg = format!("том {}/{} читается на ходу: {}", lv.vg, lv.name, if !lv.linear { "не линейный (чередование/зеркало/thin)" } else { "сегменты вне этого диска или нет данных PV" });
                if mode == LvmMode::Required { bail!("{msg} — снимок невозможен (режим «обязательно»)"); }
                plan.warnings.push(msg);
                continue;
            }
            let dev = snap_dev(&lv.vg, &lv.name);
            plan.snaps.push(SnapReq { vg: lv.vg.clone(), lv: lv.name.clone(), snap: snap_name(&lv.name) });
            for s in &lv.segs {
                let pv = &pvs[&s.pv];
                let part = parts.iter().find(|p| matches!(&p.role, Role::Pv(x) if *x == s.pv)).unwrap();
                let disk_start = part.start + pv.pe_start + s.pe_first * pv.ext;
                let len = (s.pe_last + 1 - s.pe_first) * pv.ext;
                let snap_off = s.le_first * pv.ext;
                over.push(Region { start: disk_start, len, source: Source::Snapshot { dev: dev.clone(), off: snap_off } });
            }
        }
    }

    // Разложить подмены по порядку, промежутки — Live; обрезать по размеру диска.
    over.retain(|r| r.start < size && r.len > 0);
    over.sort_by_key(|r| r.start);
    // Снять наложения (не должно быть, но страхуемся): более ранний имеет приоритет.
    let mut pos = 0u64;
    for r in over {
        if r.start >= size { break; }
        let end = (r.start + r.len).min(size);
        if r.start < pos { // перекрытие — подрезать
            if end <= pos { continue; }
        }
        let rs = r.start.max(pos);
        if rs > pos { plan.regions.push(Region { start: pos, len: rs - pos, source: Source::Live }); }
        let src = match r.source { Source::Snapshot { dev, off } => Source::Snapshot { dev, off: off + (rs - r.start) }, s => s };
        plan.regions.push(Region { start: rs, len: end - rs, source: src });
        pos = end;
    }
    if pos < size { plan.regions.push(Region { start: pos, len: size - pos, source: Source::Live }); }
    let _ = disk;
    Ok(plan)
}

/// Имя снимка тома: короткое и предсказуемое.
pub fn snap_name(lv: &str) -> String { format!("hb_{}_snap", lv.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()) }

// ------------------------------------------------------------ рантайм на машине

use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::process::Command;

/// Размер блочного устройства (или файла образа), байт.
pub fn dev_size(path: &str) -> Result<u64> {
    let mut f = std::fs::File::open(path).with_context(|| format!("диск {path}"))?;
    Ok(f.seek(SeekFrom::End(0))?)
}

fn run(prog: &str, args: &[&str]) -> std::io::Result<std::process::Output> { Command::new(prog).args(args).output() }
fn ok_out(prog: &str, args: &[&str]) -> Option<String> {
    run(prog, args).ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

/// Базовое имя диска по пути (/dev/sda → sda, /dev/nvme0n1 → nvme0n1).
fn disk_base(disk: &str) -> String { disk.trim_start_matches("/dev/").to_string() }

/// Разделы диска из /sys/class/block: смещение и размер в байтах, тип ФС (blkid), роль.
fn parts_of(disk: &str) -> Vec<Part> {
    let base = disk_base(disk);
    let dir = std::path::Path::new("/sys/class/block").join(&base);
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&dir) { Ok(r) => r, Err(_) => return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(&base) || !e.path().join("partition").exists() { continue; }
        let start = std::fs::read_to_string(e.path().join("start")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
        let size = std::fs::read_to_string(e.path().join("size")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
        let dev = format!("/dev/{name}");
        let fstype = ok_out("blkid", &["-o", "value", "-s", "TYPE", &dev]).map(|s| s.trim().to_string()).unwrap_or_default();
        let role = if fstype == "swap" { Role::Swap } else if fstype == "LVM2_member" { Role::Pv(dev.clone()) } else if fstype.is_empty() { Role::Other } else { Role::Fs(fstype) };
        out.push(Part { dev, start, size, role });
    }
    out.sort_by_key(|p| p.start);
    out
}

/// Физические тома: pe_start и размер экстента в байтах.
fn pvs_map() -> BTreeMap<String, Pv> {
    let mut m = BTreeMap::new();
    let Some(o) = ok_out("pvs", &["--noheadings", "--units", "b", "--nosuffix", "-o", "pv_name,pe_start,vg_extent_size"]) else { return m };
    for l in o.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 { continue; }
        if let (Ok(pe), Ok(ext)) = (f[1].parse::<u64>(), f[2].parse::<u64>()) { m.insert(f[0].to_string(), Pv { pe_start: pe, ext }); }
    }
    m
}

/// Логические тома и их линейные сегменты (seg_pe_ranges вида `/dev/sda2:0-2559`).
fn lvs_list() -> Vec<Lv> {
    let Some(o) = ok_out("lvs", &["--noheadings", "--units", "b", "--nosuffix", "-o", "vg_name,lv_name,segtype,seg_pe_ranges"]) else { return vec![] };
    // Строки одного тома идут подряд (по одной на сегмент), но lvs обычно печатает все сегменты в одной
    // строке через пробел. Разбираем обе формы.
    let mut by: Vec<Lv> = Vec::new();
    for l in o.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 4 { continue; }
        let (vg, lv, segtype) = (f[0].to_string(), f[1].to_string(), f[2].to_string());
        let ranges = &f[3..];
        let entry = match by.iter_mut().find(|x| x.vg == vg && x.name == lv) {
            Some(e) => e,
            None => { by.push(Lv { vg: vg.clone(), name: lv.clone(), linear: true, segs: vec![] }); by.last_mut().unwrap() }
        };
        if segtype != "linear" { entry.linear = false; }
        for r in ranges {
            // pv:first-last
            if let Some((pv, span)) = r.rsplit_once(':') {
                if let Some((a, b)) = span.split_once('-') {
                    if let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.trim().parse::<u64>()) {
                        let le_first = entry.segs.iter().map(|s| s.pe_last + 1 - s.pe_first).sum();
                        entry.segs.push(Seg { pv: pv.to_string(), pe_first: a, pe_last: b, le_first });
                    }
                }
            }
        }
    }
    by
}

/// Смонтированные ФС: устройство → точка монтирования.
fn mounts() -> Vec<(String, String)> {
    std::fs::read_to_string("/proc/mounts").unwrap_or_default().lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 2 && f[0].starts_with("/dev/")).then(|| (f[0].to_string(), f[1].to_string()))
    }).collect()
}

/// Уборка после чтения диска: снять снимки LVM и разморозить ФС. Страхуется отдельным процессом.
pub struct Cleanup { snaps: Vec<String>, frozen: Vec<String>, guard: Option<std::process::Child> }

impl Drop for Cleanup {
    fn drop(&mut self) {
        for m in self.frozen.iter().rev() { let _ = run("fsfreeze", &["-u", m]); }
        for s in &self.snaps { let _ = run("lvremove", &["-f", s]); }
        if let Some(mut g) = self.guard.take() { let _ = g.kill(); let _ = g.wait(); }
    }
}

const SMALL_FS: u64 = 4 << 30; // «небольшой» раздел с ФС — до 4 ГиБ

/// Собрать план диска, создать снимки LVM и заморозить небольшие ФС. Предупреждения уходят кадрами.
pub fn prepare<W: Write>(disk: &str, size: u64, mode: LvmMode, freeze_small: bool, freeze_timeout: u64, w: &mut super::frames::Writer<W>) -> Result<(Plan, Cleanup)> {
    let parts = parts_of(disk);
    let pvs = pvs_map();
    let lvs = lvs_list();
    let snap_dev = |vg: &str, lv: &str| format!("/dev/{vg}/{}", snap_name(lv));
    let mut plan = plan_disk(disk, size, &parts, &pvs, &lvs, mode, &snap_dev)?;

    // Создать снимки LVM. При неудаче в режиме auto — читать том на ходу (убрать подмену), в required — ошибка.
    let mut created: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for s in &plan.snaps {
        let origin = format!("{}/{}", s.vg, s.lv);
        let r = run("lvcreate", &["-s", "-y", "-n", &s.snap, "-l", "20%ORIGIN", &origin]);
        let good = r.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if good { created.push(snap_dev(&s.vg, &s.lv)); } else {
            let msg = format!("том {}/{}: не удалось создать снимок LVM — читается на ходу", s.vg, s.lv);
            if mode == LvmMode::Required { for c in &created { let _ = run("lvremove", &["-f", c]); } bail!("{msg} (режим «обязательно»)"); }
            w.warn(&msg)?; plan.warnings.push(msg);
            failed.push(snap_dev(&s.vg, &s.lv));
        }
    }
    if !created.is_empty() { let _ = run("udevadm", &["settle"]); }
    // Участки неудавшихся снимков — читать с самого диска.
    if !failed.is_empty() {
        for r in &mut plan.regions {
            if let Source::Snapshot { dev, .. } = &r.source { if failed.contains(dev) { r.source = Source::Live; } }
        }
    }

    // Заморозить небольшие ФС без LVM (на время чтения), если включено.
    let mut frozen: Vec<String> = Vec::new();
    if freeze_small {
        let mnts = mounts();
        for p in &parts {
            if let Role::Fs(_) = p.role {
                if p.size > 0 && p.size <= SMALL_FS {
                    if let Some((_, mp)) = mnts.iter().find(|(d, _)| *d == p.dev) {
                        if mp != "/" && run("fsfreeze", &["-f", mp]).map(|o| o.status.success()).unwrap_or(false) {
                            frozen.push(mp.clone());
                            w.warn(&format!("раздел {} ({}) заморожен на время чтения", p.dev, mp))?;
                        } else {
                            w.warn(&format!("раздел {} читается на ходу (вне LVM)", p.dev))?;
                        }
                    }
                }
            }
        }
    } else {
        for p in &parts { if let Role::Fs(_) = p.role { w.warn(&format!("раздел {} читается на ходу (вне LVM)", p.dev))?; } }
    }

    // Страховка: отдельный процесс снимет заморозку и снимки, если программа не успеет.
    let thaw: String = frozen.iter().map(|m| format!("fsfreeze -u {} 2>/dev/null; ", crate::util::shq(m))).collect();
    let rmsnap: String = created.iter().map(|s| format!("lvremove -f {} 2>/dev/null; ", crate::util::shq(s))).collect();
    let guard = if frozen.is_empty() && created.is_empty() { None } else {
        Command::new("setsid").args(["sh", "-c", &format!("sleep {freeze_timeout}; {thaw}{rmsnap}")])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok()
    };
    Ok((plan, Cleanup { snaps: created, frozen, guard }))
}

/// Чтение диска по плану: с самого диска, со снимков томов или нулями.
pub struct Overlay<'a> { disk: std::fs::File, snaps: std::collections::HashMap<String, std::fs::File>, regions: &'a [Region] }

impl<'a> Overlay<'a> {
    pub fn open(disk: &str, plan: &'a Plan) -> Result<Overlay<'a>> {
        let d = std::fs::File::open(disk).with_context(|| format!("диск {disk}"))?;
        let mut snaps = std::collections::HashMap::new();
        for r in &plan.regions {
            if let Source::Snapshot { dev, .. } = &r.source {
                if !snaps.contains_key(dev) { snaps.insert(dev.clone(), std::fs::File::open(dev).with_context(|| format!("снимок {dev}"))?); }
            }
        }
        Ok(Overlay { disk: d, snaps, regions: &plan.regions })
    }

    /// Заполнить `buf` содержимым диска с `off` (длина `buf`), подставляя снимки и нули по плану.
    pub fn read_block(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let end = off + buf.len() as u64;
        let mut pos = off;
        while pos < end {
            let r = self.regions.iter().find(|r| pos >= r.start && pos < r.start + r.len)
                .ok_or_else(|| anyhow!("смещение {pos} вне плана диска"))?;
            let take = (r.start + r.len - pos).min(end - pos) as usize;
            let dst = &mut buf[(pos - off) as usize..(pos - off) as usize + take];
            match &r.source {
                Source::Live => self.disk.read_exact_at(dst, pos).with_context(|| format!("чтение диска со смещения {pos}"))?,
                Source::Zero => dst.fill(0),
                Source::Snapshot { dev, off: so } => {
                    let f = self.snaps.get(dev).ok_or_else(|| anyhow!("нет снимка {dev}"))?;
                    f.read_exact_at(dst, so + (pos - r.start)).with_context(|| format!("чтение снимка {dev}"))?;
                }
            }
            pos += take as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(vg: &str, lv: &str) -> String { format!("/dev/{vg}/{}", snap_name(lv)) }

    #[test]
    fn swap_zeroed_and_live_gaps() {
        // Диск 100, раздел подкачки [10..30), остальное — Live.
        let parts = vec![Part { dev: "/dev/sda1".into(), start: 10, size: 20, role: Role::Swap }];
        let p = plan_disk("/dev/sda", 100, &parts, &BTreeMap::new(), &[], LvmMode::Auto, &dev).unwrap();
        assert_eq!(p.regions, vec![
            Region { start: 0, len: 10, source: Source::Live },
            Region { start: 10, len: 20, source: Source::Zero },
            Region { start: 30, len: 70, source: Source::Live },
        ]);
    }

    #[test]
    fn linear_lv_mapped_to_snapshot() {
        // PV на разделе /dev/sda2 (смещение 1<<20), pe_start 1<<20 внутри PV, экстент 4<<20.
        let ext = 4u64 << 20;
        let part_start = 1u64 << 20;
        let pe_start = 1u64 << 20;
        let parts = vec![
            Part { dev: "/dev/sda1".into(), start: 0, size: part_start, role: Role::Other },
            Part { dev: "/dev/sda2".into(), start: part_start, size: 100 * ext, role: Role::Pv("/dev/sda2".into()) },
        ];
        let mut pvs = BTreeMap::new();
        pvs.insert("/dev/sda2".to_string(), Pv { pe_start, ext });
        // Том root: экстенты PV 0..1 (два экстента), логический старт 0.
        let lvs = vec![Lv { vg: "vg0".into(), name: "root".into(), linear: true, segs: vec![Seg { pv: "/dev/sda2".into(), pe_first: 0, pe_last: 1, le_first: 0 }] }];
        let size = part_start + 100 * ext;
        let p = plan_disk("/dev/sda", size, &parts, &pvs, &lvs, LvmMode::Auto, &dev).unwrap();
        assert_eq!(p.snaps, vec![SnapReq { vg: "vg0".into(), lv: "root".into(), snap: snap_name("root") }]);
        let seg_start = part_start + pe_start; // 2<<20
        let seg_len = 2 * ext;
        assert!(p.regions.contains(&Region { start: seg_start, len: seg_len, source: Source::Snapshot { dev: dev("vg0", "root"), off: 0 } }));
        // До снимка — Live, после — Live до конца диска.
        assert_eq!(p.regions[0], Region { start: 0, len: seg_start, source: Source::Live });
        assert_eq!(p.regions.last().unwrap().source, Source::Live);
        // Участки покрывают весь диск без разрывов.
        let mut pos = 0;
        for r in &p.regions { assert_eq!(r.start, pos); pos += r.len; }
        assert_eq!(pos, size);
    }

    #[test]
    fn nonlinear_lv_live_with_warning_or_error() {
        let ext = 4u64 << 20;
        let parts = vec![Part { dev: "/dev/sda1".into(), start: 0, size: 100 * ext, role: Role::Pv("/dev/sda1".into()) }];
        let mut pvs = BTreeMap::new();
        pvs.insert("/dev/sda1".to_string(), Pv { pe_start: 0, ext });
        let lvs = vec![Lv { vg: "vg0".into(), name: "striped".into(), linear: false, segs: vec![Seg { pv: "/dev/sda1".into(), pe_first: 0, pe_last: 9, le_first: 0 }] }];
        let p = plan_disk("/dev/sda", 100 * ext, &parts, &pvs, &lvs, LvmMode::Auto, &dev).unwrap();
        assert!(p.snaps.is_empty());
        assert!(p.warnings.iter().any(|w| w.contains("striped")));
        assert!(p.regions.iter().all(|r| r.source == Source::Live));
        // В режиме «обязательно» — ошибка.
        assert!(plan_disk("/dev/sda", 100 * ext, &parts, &pvs, &lvs, LvmMode::Required, &dev).is_err());
    }
}
