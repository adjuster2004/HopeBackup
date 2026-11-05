//! Исключения: что не копировать. Общий список сервера (настройки) + список экземпляра.
//!
//! Шаблоны:
//! * `/var/log`, `/home/*/Downloads` — от корня: путь и всё внутри;
//! * `*.log`, `node_modules`, `.cache` — имя файла или каталога на любой глубине;
//! * `cache/tmp`, `**/target/debug` — часть пути на любой глубине.
//!
//! `*` — любые символы, кроме `/`; `**` — любое число каталогов; `?` — один символ.

use serde_json::Value;

#[derive(Clone, Debug)]
enum Pat {
    Anchored(String),
    Anywhere(String),
    Name(String),
}

#[derive(Clone, Debug, Default)]
pub struct Excludes {
    pats: Vec<Pat>,
    /// Файлы больше этого размера не копируются.
    max_size: Option<u64>,
    raw: Vec<String>,
}

/// Исключения по умолчанию для нового сервера управления.
pub fn defaults() -> Vec<String> {
    [
        "/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/var/cache", "/var/lib/apt/lists", "/var/lib/dnf",
        "/var/log/journal", "/swapfile", "/swap.img", "lost+found", ".cache", "*.swp", "*.tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Excludes {
    pub fn new(list: &[String], max_size: Option<u64>) -> Excludes {
        let mut e = Excludes { max_size, ..Default::default() };
        for p in list {
            let p = p.trim();
            if p.is_empty() || p.starts_with('#') { continue; }
            e.raw.push(p.to_string());
            let p = p.trim_end_matches('/');
            if p.is_empty() { continue; }
            if p.starts_with('/') {
                e.pats.push(Pat::Anchored(p.to_string()));
            } else if p.contains('/') {
                e.pats.push(Pat::Anywhere(p.trim_start_matches("**/").to_string()));
            } else {
                e.pats.push(Pat::Name(p.to_string()));
            }
        }
        e
    }

    /// Из конфигурации экземпляра: `exclude`, `global_exclude` (подставляет сервер, если не выключено
    /// `use_global_exclude`), `max_file_mb`.
    pub fn from_config(cfg: &Value) -> Excludes {
        let mut list = crate::connectors::net::p_list(cfg, "exclude");
        if cfg.get("use_global_exclude").and_then(|v| v.as_bool()).unwrap_or(true) {
            list.extend(crate::connectors::net::p_list(cfg, "global_exclude"));
        }
        let mb = cfg.get("max_file_mb").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).filter(|m| *m > 0);
        Excludes::new(&list, mb.map(|m| m * 1024 * 1024))
    }

    pub fn is_empty(&self) -> bool { self.pats.is_empty() && self.max_size.is_none() }
    pub fn list(&self) -> &[String] { &self.raw }

    /// Путь (абсолютный) исключён сам или через один из родительских каталогов.
    pub fn excluded(&self, path: &str) -> bool { self.excluded_under(path, "") }

    /// То же, но каталоги выше `top` (явно выбранного пути) и сам `top` не проверяются: если
    /// администратор указал /tmp/app, общее исключение /tmp его не отменяет.
    pub fn excluded_under(&self, path: &str, top: &str) -> bool {
        if self.pats.is_empty() { return false; }
        let path = path.trim_end_matches('/');
        let top_len = top.trim_end_matches('/').len();
        let mut ends: Vec<usize> = path.match_indices('/').map(|(i, _)| i).filter(|&i| i > 0 && i > top_len).collect();
        if path.len() > top_len || top_len == 0 { ends.push(path.len()); }
        for &end in &ends {
            let prefix = &path[..end];
            let name = prefix.rsplit('/').next().unwrap_or(prefix);
            for p in &self.pats {
                let hit = match p {
                    Pat::Anchored(g) => glob(g.as_bytes(), prefix.as_bytes()),
                    Pat::Name(g) => glob(g.as_bytes(), name.as_bytes()),
                    Pat::Anywhere(g) => {
                        // Совпадение с хвостом префикса, начинающимся с границы каталога.
                        let rel = prefix.trim_start_matches('/');
                        std::iter::once(0).chain(rel.match_indices('/').map(|(i, _)| i + 1)).any(|s| glob(g.as_bytes(), rel[s..].as_bytes()))
                    }
                };
                if hit { return true; }
            }
        }
        false
    }

    pub fn too_big(&self, size: u64) -> bool { self.max_size.map(|m| size > m).unwrap_or(false) }
}

/// Сопоставление с шаблоном: `*` (кроме `/`), `**` (включая `/`), `?`.
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() { return s.is_empty(); }
    if p.starts_with(b"**") {
        let mut rest = &p[2..];
        // «**/» может совпасть и с пустым префиксом.
        if rest.first() == Some(&b'/') {
            let r2 = &rest[1..];
            if glob(r2, s) { return true; }
            rest = r2;
            for i in 0..s.len() { if s[i] == b'/' && glob(rest, &s[i + 1..]) { return true; } }
            return false;
        }
        for i in 0..=s.len() { if glob(rest, &s[i..]) { return true; } }
        return false;
    }
    match p[0] {
        b'*' => {
            for i in 0..=s.len() {
                if glob(&p[1..], &s[i..]) { return true; }
                if i < s.len() && s[i] == b'/' { break; }
            }
            false
        }
        b'?' => !s.is_empty() && s[0] != b'/' && glob(&p[1..], &s[1..]),
        c => !s.is_empty() && s[0] == c && glob(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let e = Excludes::new(&["/var/log".into(), "*.log".into(), "node_modules".into(), "/home/*/Downloads".into(), "build/tmp".into(), "/cache/".into(), "srv/cache".into()], None);
        assert!(e.excluded("/var/log") && e.excluded("/var/log/nginx/access.1"));
        assert!(!e.excluded("/var/logs") && !e.excluded("/var/lib"));
        assert!(e.excluded("/opt/app/x.log") && !e.excluded("/opt/app/x.logx"));
        assert!(e.excluded("/srv/web/node_modules/react/index.js"));
        assert!(e.excluded("/home/ivan/Downloads/a.iso") && !e.excluded("/home/ivan/Documents/a"));
        assert!(e.excluded("/opt/x/build/tmp/1") && !e.excluded("/opt/x/build/out"));
        assert!(e.excluded("/srv/cache/1") && !e.excluded("/srv/cached"));
        // «/cache/» — от корня (как «/cache»), а не любой каталог cache.
        assert!(e.excluded("/cache/x") && !e.excluded("/opt/cache/x"));
        assert!(!e.excluded_under("/var/log/app", "/var/log/app") && e.excluded_under("/var/log/app/x.log", "/var/log/app") && !e.excluded_under("/var/log/app/x", "/var/log/app"));
        assert!(glob(b"**/a/*.c", b"x/y/a/b.c") && glob(b"**/a/*.c", b"a/b.c") && !glob(b"*.c", b"x/b.c"));
        let cfg = serde_json::json!({ "exclude": ["*.iso"], "global_exclude": ["/tmp"], "max_file_mb": 5 });
        let e = Excludes::from_config(&cfg);
        assert!(e.excluded("/tmp/x") && e.excluded("/a.iso") && e.too_big(6 * 1024 * 1024) && !e.too_big(1024));
        let cfg = serde_json::json!({ "exclude": ["*.iso"], "global_exclude": ["/tmp"], "use_global_exclude": false });
        assert!(!Excludes::from_config(&cfg).excluded("/tmp/x"));
    }
}
