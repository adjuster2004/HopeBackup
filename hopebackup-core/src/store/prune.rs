//! Политика хранения: какие снапшоты оставить, какие сегменты WAL больше не нужны. Общая для узла
//! площадки и S3, чтобы очистка везде вела себя одинаково.

use std::collections::HashMap;

use super::types::{PrunePolicy, SnapshotHeader};

/// Виды копий, которые продолжают цепочку от полной (журнал binlog, журнал транзакций / инкремент):
/// хранятся, пока хранится полная, от которой они начинаются, — а не по счётчику.
pub const CHAIN_KINDS: &[&str] = &["binlog", "incr"];

/// Индексы оставляемых и удаляемых снапшотов. `all` — от новых к старым.
pub fn plan(all: &[SnapshotHeader], policy: &PrunePolicy) -> (Vec<usize>, Vec<usize>) {
    let now = chrono::Utc::now();
    let mut per: HashMap<(String, String), usize> = HashMap::new();
    let (mut keep, mut drop) = (Vec::new(), Vec::new());
    // Самая старая оставшаяся полная копия каждого экземпляра — начало цепочек журнала.
    let mut oldest_base: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    let mut chain = Vec::new();
    for (i, h) in all.iter().enumerate() {
        // Время снапшота пишет источник: «будущие» снапшоты не вытесняют настоящие (и сами остаются).
        if h.time > now + chrono::Duration::hours(1) { keep.push(i); continue; }
        if policy.keep_unknown && !policy.per_instance.contains_key(&h.instance) { keep.push(i); continue; }
        if CHAIN_KINDS.contains(&h.kind.as_str()) { chain.push(i); continue; }
        let n = per.entry((h.instance.clone(), h.kind.clone())).or_insert(0);
        let young = policy.keep_days.map(|d| (now - h.time).num_days() < d as i64).unwrap_or(false);
        let keep_n = policy.per_instance.get(&h.instance).copied().unwrap_or(policy.keep_last).max(1);
        if *n < keep_n || young {
            *n += 1;
            keep.push(i);
            let e = oldest_base.entry(h.instance.clone()).or_insert(h.time);
            if h.time < *e { *e = h.time; }
        } else {
            drop.push(i);
        }
    }
    for i in chain {
        let h = &all[i];
        let young = policy.keep_days.map(|d| (now - h.time).num_days() < d as i64).unwrap_or(false);
        match oldest_base.get(&h.instance) {
            Some(t) if h.time >= *t || young => keep.push(i),
            Some(_) => drop.push(i),
            // Полных копий нет вовсе — журнал по счётчику (как прочие виды).
            None => {
                let n = per.entry((h.instance.clone(), h.kind.clone())).or_insert(0);
                if *n < policy.per_instance.get(&h.instance).copied().unwrap_or(policy.keep_last).max(1) || young { *n += 1; keep.push(i); } else { drop.push(i); }
            }
        }
    }
    keep.sort();
    drop.sort();
    (keep, drop)
}

/// Для каждого экземпляра — начало журнала самой старой оставшейся полной копии.
pub fn wal_starts<'a>(keep: impl Iterator<Item = &'a SnapshotHeader>) -> HashMap<String, String> {
    let mut oldest: HashMap<String, String> = HashMap::new();
    for r in keep {
        if let Some(ws) = r.extra.get("wal_start").and_then(|v| v.as_str()) {
            let e = oldest.entry(r.instance.clone()).or_insert_with(|| ws.to_string());
            if ws < e.as_str() { *e = ws.to_string(); }
        }
    }
    oldest
}

/// Сегмент WAL старше начала журнала оставшихся копий (файлы истории линий времени нужны всегда).
pub fn wal_expired(name: &str, start: &str) -> bool {
    if name.ends_with(".history") || name.len() < 24 { return false; }
    name[..24] < start[..start.len().min(24)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::*;

    fn rec(inst: &str, mins_ago: i64) -> SnapshotHeader {
        SnapshotHeader { id: format!("{inst}-{mins_ago}"), time: chrono::Utc::now() - chrono::Duration::minutes(mins_ago), instance: inst.into(), product: "t".into(), kind: "full".into(), host: "h".into(), size: 0, new_bytes: 0, uploaded: 0, chunks: 0, extra: serde_json::json!({}) }
    }

    #[test]
    fn policy() {
        let mut all = vec![rec("a", -600), rec("a", 1), rec("a", 2), rec("a", 3), rec("b", 1), rec("b", 2)];
        all.sort_by_key(|r| std::cmp::Reverse(r.time));
        let mut per = std::collections::HashMap::new();
        per.insert("a".to_string(), 1usize);
        let (keep, drop) = plan(&all, &PrunePolicy { keep_last: 1, keep_days: None, dry_run: true, per_instance: per.clone(), keep_unknown: true });
        let kept: Vec<&str> = keep.iter().map(|&i| all[i].id.as_str()).collect();
        // «будущий» a-(-600) не вытесняет a-1; чужой экземпляр b не трогается.
        assert!(kept.contains(&"a--600") && kept.contains(&"a-1") && kept.contains(&"b-1") && kept.contains(&"b-2"));
        assert_eq!(drop.len(), 2);
        // Журнал хранится, пока хранится полная копия, от которой он начинается.
        let mut all = vec![rec("m", 300), rec("m", 200), rec("m", 100)];
        for (k, m) in [(150, "incr"), (120, "incr"), (90, "incr"), (60, "incr"), (250, "incr")] { let mut r = rec("m", k); r.kind = m.into(); all.push(r); }
        all.sort_by_key(|r| std::cmp::Reverse(r.time));
        let mut per = std::collections::HashMap::new();
        per.insert("m".to_string(), 2usize);
        let (keep, drop) = plan(&all, &PrunePolicy { keep_last: 2, keep_days: None, dry_run: true, per_instance: per, keep_unknown: true });
        let kept: Vec<&str> = keep.iter().map(|&i| all[i].id.as_str()).collect();
        assert!(kept.contains(&"m-150") && kept.contains(&"m-60") && !kept.contains(&"m-250") && !kept.contains(&"m-300"), "{kept:?}");
        assert_eq!(drop.len(), 2);
    }
}
