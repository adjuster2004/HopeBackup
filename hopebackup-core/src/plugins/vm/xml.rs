//! Небольшой разбор XML (ответы SOAP vSphere, XML-RPC XAPI и OpenNebula, описания libvirt) и
//! XML-RPC: запрос из значений JSON, ответ — в JSON.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

#[derive(Debug, Default, Clone)]
pub struct Node {
    /// Имя без префикса пространства имён.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, n: &str) -> Option<&Node> { self.children.iter().find(|c| c.name == n) }
    pub fn all(&self, n: &str) -> Vec<&Node> { self.children.iter().filter(|c| c.name == n).collect() }
    /// Текст дочернего элемента (пусто, если нет).
    pub fn t(&self, n: &str) -> String { self.child(n).map(|c| c.text.trim().to_string()).unwrap_or_default() }
    pub fn attr(&self, n: &str) -> Option<&str> { self.attrs.iter().find(|(k, _)| k == n || k.rsplit(':').next() == Some(n)).map(|(_, v)| v.as_str()) }
    /// Первый потомок с именем на любой глубине.
    pub fn find(&self, n: &str) -> Option<&Node> {
        for c in &self.children { if c.name == n { return Some(c); } if let Some(x) = c.find(n) { return Some(x); } }
        None
    }
    /// Все потомки с именем на любой глубине.
    pub fn find_all<'a>(&'a self, n: &str, out: &mut Vec<&'a Node>) {
        for c in &self.children { if c.name == n { out.push(c); } c.find_all(n, out); }
    }
    pub fn path(&self, p: &[&str]) -> Option<&Node> { let mut cur = self; for s in p { cur = cur.child(s)?; } Some(cur) }
}

pub fn unescape(s: &str) -> String {
    if !s.contains('&') { return s.to_string(); }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(j) = tail.find(';') else { out.push_str(tail); return out };
        let ent = &tail[1..j];
        match ent {
            "lt" => out.push('<'), "gt" => out.push('>'), "amp" => out.push('&'), "quot" => out.push('"'), "apos" => out.push('\''),
            e if e.starts_with("#x") => { if let Some(c) = u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32) { out.push(c); } }
            e if e.starts_with('#') => { if let Some(c) = e[1..].parse::<u32>().ok().and_then(char::from_u32) { out.push(c); } }
            _ => out.push_str(&tail[..=j]),
        }
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    out
}

pub fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

fn local(n: &str) -> String { n.rsplit(':').next().unwrap_or(n).to_string() }

/// Разобрать документ; корень — служебный узел с пустым именем.
pub fn parse(s: &str) -> Result<Node> {
    let mut stack: Vec<Node> = vec![Node::default()];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if s[i..].starts_with("<!--") { i = s[i..].find("-->").map(|x| i + x + 3).ok_or_else(|| anyhow!("XML: незакрытый комментарий"))?; continue; }
            if s[i..].starts_with("<![CDATA[") {
                let e = s[i..].find("]]>").map(|x| i + x).ok_or_else(|| anyhow!("XML: незакрытый CDATA"))?;
                stack.last_mut().unwrap().text.push_str(&s[i + 9..e]);
                i = e + 3; continue;
            }
            if s[i..].starts_with("<?") || s[i..].starts_with("<!") { i = s[i..].find('>').map(|x| i + x + 1).ok_or_else(|| anyhow!("XML: незакрытая инструкция"))?; continue; }
            // Конец тега с учётом кавычек в атрибутах.
            let mut j = i + 1;
            let mut q: Option<u8> = None;
            while j < b.len() { match (q, b[j]) { (None, b'"') | (None, b'\'') => q = Some(b[j]), (Some(c), x) if x == c => q = None, (None, b'>') => break, _ => {} } j += 1; }
            if j >= b.len() { bail!("XML: незакрытый тег"); }
            let tag = &s[i + 1..j];
            if let Some(name) = tag.strip_prefix('/') {
                let n = stack.pop().ok_or_else(|| anyhow!("XML: лишний закрывающий тег"))?;
                if n.name != local(name.trim()) { bail!("XML: ожидался </{}>, а встретился </{}>", n.name, name.trim()); }
                stack.last_mut().ok_or_else(|| anyhow!("XML: лишний закрывающий тег"))?.children.push(n);
            } else {
                let self_close = tag.ends_with('/');
                let tag = tag.trim_end_matches('/');
                let (name, rest) = tag.split_once(|c: char| c.is_whitespace()).unwrap_or((tag, ""));
                let mut node = Node { name: local(name), ..Default::default() };
                let mut r = rest.trim();
                while let Some(eq) = r.find('=') {
                    let k = r[..eq].trim().to_string();
                    let v = r[eq + 1..].trim_start();
                    let Some(qc) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
                    let Some(end) = v[1..].find(qc) else { break };
                    node.attrs.push((k, unescape(&v[1..1 + end])));
                    r = v[1 + end + 1..].trim_start();
                }
                if self_close { stack.last_mut().unwrap().children.push(node); } else { stack.push(node); }
            }
            i = j + 1;
        } else {
            let e = s[i..].find('<').map(|x| i + x).unwrap_or(b.len());
            stack.last_mut().unwrap().text.push_str(&unescape(&s[i..e]));
            i = e;
        }
    }
    if stack.len() != 1 { bail!("XML: незакрытый элемент <{}>", stack.last().map(|n| n.name.clone()).unwrap_or_default()); }
    Ok(stack.pop().unwrap())
}

// ---------------------------------------------------------------- XML-RPC

pub fn rpc_encode(v: &Value) -> String {
    match v {
        Value::Null => "<value><nil/></value>".into(),
        Value::Bool(b) => format!("<value><boolean>{}</boolean></value>", if *b { 1 } else { 0 }),
        Value::Number(n) => if n.is_i64() { format!("<value><int>{n}</int></value>") } else { format!("<value><double>{n}</double></value>") },
        Value::String(s) => format!("<value><string>{}</string></value>", esc(s)),
        Value::Array(a) => format!("<value><array><data>{}</data></array></value>", a.iter().map(rpc_encode).collect::<String>()),
        Value::Object(o) => format!("<value><struct>{}</struct></value>", o.iter().map(|(k, x)| format!("<member><name>{}</name>{}</member>", esc(k), rpc_encode(x))).collect::<String>()),
    }
}

pub fn rpc_request(method: &str, params: &[Value]) -> String {
    format!("<?xml version=\"1.0\"?><methodCall><methodName>{}</methodName><params>{}</params></methodCall>", esc(method), params.iter().map(|p| format!("<param>{}</param>", rpc_encode(p))).collect::<String>())
}

pub fn rpc_value(v: &Node) -> Value {
    match v.children.first() {
        None => Value::String(v.text.clone()),
        Some(c) => match c.name.as_str() {
            "string" => Value::String(c.text.clone()),
            "int" | "i4" | "i8" => c.text.trim().parse::<i64>().map(Value::from).unwrap_or(Value::String(c.text.clone())),
            "double" => c.text.trim().parse::<f64>().map(|x| json!(x)).unwrap_or(Value::Null),
            "boolean" => Value::Bool(c.text.trim() == "1"),
            "nil" => Value::Null,
            "array" => Value::Array(c.child("data").map(|d| d.all("value").into_iter().map(rpc_value).collect()).unwrap_or_default()),
            "struct" => {
                let mut m = Map::new();
                for mem in c.all("member") { if let Some(val) = mem.child("value") { m.insert(mem.t("name"), rpc_value(val)); } }
                Value::Object(m)
            }
            _ => Value::String(c.text.clone()),
        },
    }
}

/// Ответ XML-RPC → значение (fault — ошибка).
pub fn rpc_response(body: &str) -> Result<Value> {
    let d = parse(body)?;
    let r = d.child("methodResponse").ok_or_else(|| anyhow!("ответ не XML-RPC: {}", body.chars().take(200).collect::<String>()))?;
    if let Some(f) = r.child("fault") { let v = f.child("value").map(rpc_value).unwrap_or_default(); bail!("XML-RPC: {} {}", v["faultCode"], v["faultString"].as_str().unwrap_or("")); }
    Ok(r.path(&["params", "param", "value"]).map(rpc_value).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses() {
        let x = r#"<?xml version="1.0"?><soapenv:Envelope xmlns:soapenv="x"><soapenv:Body><RetrievePropertiesExResponse><returnval><obj type="VirtualMachine">vm-12</obj><propSet><name>name</name><val xsi:type="xsd:string">web &amp; db</val></propSet></returnval></RetrievePropertiesExResponse></soapenv:Body></soapenv:Envelope>"#;
        let d = parse(x).unwrap();
        let rv = d.find("returnval").unwrap();
        assert_eq!(rv.child("obj").unwrap().attr("type"), Some("VirtualMachine"));
        assert_eq!(rv.t("obj"), "vm-12");
        assert_eq!(rv.find("val").unwrap().text, "web & db");
        let req = rpc_request("VM.snapshot", &[json!("OpaqueRef:s"), json!("hb <1>")]);
        assert!(req.contains("<string>hb &lt;1&gt;</string>"));
        let resp = r#"<methodResponse><params><param><value><struct><member><name>Status</name><value>Success</value></member><member><name>Value</name><value><array><data><value><i4>7</i4></value><value><boolean>1</boolean></value></data></array></value></member></struct></value></param></params></methodResponse>"#;
        let v = rpc_response(resp).unwrap();
        assert_eq!(v["Value"], json!([7, true]));
        assert!(parse("<a><b></a>").is_err());
        let sc = parse("<domain><devices><disk type='file' device=\"disk\"><source file='/var/lib/a b.qcow2'/></disk></devices></domain>").unwrap();
        assert_eq!(sc.find("source").unwrap().attr("file"), Some("/var/lib/a b.qcow2"));
    }
}
