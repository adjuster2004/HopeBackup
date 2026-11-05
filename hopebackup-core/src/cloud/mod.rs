//! Облака и объектные хранилища: подпись AWS SigV4, клиент S3, разбор XML-ответов.

pub mod drive;
pub mod gdrive;
pub mod oauth;
pub mod objstore;
pub mod s3;
pub mod sigv4;
pub mod webdav;
pub mod yadisk;

/// Значение первого тега `<tag>…</tag>` в XML (с раскрытием сущностей).
pub fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(xml_unescape(&xml[s..e]))
}

/// Все блоки `<tag>…</tag>` (вложенные теги внутри блока не разбираются).
pub fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

pub fn xml_unescape(s: &str) -> String {
    if !s.contains('&') { return s.to_string(); }
    let mut o = String::with_capacity(s.len());
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c != '&' { o.push(c); continue; }
        let rest = &s[i..];
        let Some(end) = rest.find(';') else { o.push(c); continue };
        let ent = &rest[1..end];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match rep {
            Some(r) => { o.push(r); for _ in 0..end { it.next(); } }
            None => o.push(c),
        }
    }
    o
}

pub fn xml_escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml() {
        let x = "<R><IsTruncated>true</IsTruncated><Contents><Key>a&amp;b/c&#x20;d</Key><Size>5</Size></Contents><Contents><Key>z</Key><Size>7</Size></Contents></R>";
        assert_eq!(xml_tag(x, "IsTruncated").as_deref(), Some("true"));
        let b = xml_blocks(x, "Contents");
        assert_eq!(b.len(), 2);
        assert_eq!(xml_tag(b[0], "Key").as_deref(), Some("a&b/c d"));
        assert_eq!(xml_tag(b[1], "Size").as_deref(), Some("7"));
    }
}
