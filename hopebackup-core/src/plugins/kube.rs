//! Kubernetes (minikube, kubeadm, k3s, managed): объекты API — каждый тип ресурса отдельным потоком JSON
//! (kubectl get -A -o json), снимок etcd (etcdctl snapshot save — на узле управления kubeadm), данные
//! томов hostPath/local (каталог на этой машине или в контейнере узла minikube), каталог /etc/kubernetes.
//!
//! Восстановление объектов: очистка служебных полей (uid, resourceVersion, status, clusterIP…), без
//! объектов, которыми управляют контроллеры (ownerReferences), и системных; применение по порядку
//! зависимостей (Namespace → CRD → хранилища → RBAC → конфигурация → сервисы → рабочие нагрузки).
//! Снимок etcd восстанавливается только файлом (etcdutl snapshot restore — вручную на узле управления).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

use super::appkit::{self as kit, Cmd, SecretFile};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct Kube;

const SYSTEM_NS: &[&str] = &["kube-system", "kube-public", "kube-node-lease"];

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "kubeconfig": { "type": "string", "title": "Файл kubeconfig", "description": "Пусто — /etc/kubernetes/admin.conf, /etc/rancher/k3s/k3s.yaml или ~/.kube/config" },
        "kubeconfig_content": { "type": "string", "format": "secret-textarea", "title": "Содержимое kubeconfig", "description": "Вместо файла (хранится как секрет)" },
        "context": { "type": "string", "title": "Контекст kubeconfig" },
        "namespaces": { "type": "array", "items": { "type": "string" }, "title": "Пространства имён", "description": "Пусто — все, кроме системных" },
        "include_system": { "type": "boolean", "title": "Системные пространства (kube-system…)", "default": false },
        "exclude_resources": { "type": "array", "items": { "type": "string" }, "title": "Кроме ресурсов", "description": "Например secrets, configmaps" },
        "pv_data": { "type": "boolean", "title": "Данные томов hostPath / local", "default": true },
        "node_container": { "type": "string", "title": "Контейнер узла", "description": "minikube (драйвер docker): тома hostPath — внутри контейнера узла, обычно «minikube»" },
        "etcd": { "type": "string", "title": "Снимок etcd", "enum": ["auto", "on", "off"], "default": "auto", "description": "auto — если на этой машине есть etcdctl и сертификаты kubeadm" },
        "etcd_endpoints": { "type": "string", "title": "Адрес etcd", "default": "https://127.0.0.1:2379" },
        "etcd_cacert": { "type": "string", "title": "CA etcd", "default": "/etc/kubernetes/pki/etcd/ca.crt" },
        "etcd_cert": { "type": "string", "title": "Сертификат клиента etcd", "default": "/etc/kubernetes/pki/etcd/server.crt" },
        "etcd_key": { "type": "string", "title": "Ключ клиента etcd", "default": "/etc/kubernetes/pki/etcd/server.key" }
    } })
}

pub fn defaults() -> Value { json!({ "include_system": false, "pv_data": true, "etcd": "auto", "etcd_endpoints": "https://127.0.0.1:2379" }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "namespaces": { "type": "array", "items": { "type": "string" }, "title": "Пространства имён", "description": "Пусто — все из копии" },
        "kinds": { "type": "array", "items": { "type": "string" }, "title": "Только ресурсы", "description": "Например deployments.apps, configmaps; пусто — все" },
        "pv_data": { "type": "boolean", "title": "Вернуть данные томов hostPath / local", "default": true },
        "dry_run": { "type": "boolean", "title": "Проверка без изменений (kubectl apply --dry-run=server)", "default": false },
        "target_dir": { "type": "string", "title": "Только файлы в каталог", "description": "Объекты (YAML/JSON), снимок etcd, данные томов — кластер не трогать" },
        "kubeconfig": { "type": "string", "title": "Файл kubeconfig целевого кластера" },
        "context": { "type": "string", "title": "Контекст" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["kubeconfig", "kubeconfig_content", "context", "node_container"] }

/// Доступ к кластеру: kubeconfig из файла или содержимого (временный файл 0600).
struct K { cfg: String, context: Option<String>, _tmp: Option<SecretFile> }

impl K {
    fn from(v: &Value) -> Result<K> {
        if let Some(c) = cfg_str(v, "kubeconfig_content") {
            let f = SecretFile::new(&c, None)?;
            return Ok(K { cfg: f.path(), context: cfg_str(v, "context"), _tmp: Some(f) });
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let cfg = cfg_str(v, "kubeconfig").or_else(|| ["/etc/kubernetes/admin.conf".to_string(), "/etc/rancher/k3s/k3s.yaml".into(), format!("{home}/.kube/config")].into_iter().find(|p| Path::new(p).exists()))
            .ok_or_else(|| anyhow!("нет kubeconfig: укажите файл или содержимое"))?;
        Ok(K { cfg, context: cfg_str(v, "context"), _tmp: None })
    }
    fn cmd(&self, args: &[&str]) -> Cmd {
        let mut c = Cmd::new("kubectl").env("KUBECONFIG", &self.cfg);
        if let Some(x) = &self.context { c = c.arg(format!("--context={x}")); }
        c.args(args.iter().copied())
    }
    async fn json(&self, args: &[&str], log: &LogFn) -> Result<Value> {
        let o = kit::output(&self.cmd(args), log).await?;
        serde_json::from_str(&o).context("ответ kubectl не JSON")
    }
}

/// Ресурсы, которые не копируются: события, производные и служебные объекты, состояние узлов.
fn skip_resource(r: &str) -> bool {
    let base = r.split('.').next().unwrap_or(r);
    ["events", "endpoints", "endpointslices", "controllerrevisions", "leases", "nodes", "apiservices", "csinodes", "volumeattachments", "componentstatuses",
        "certificatesigningrequests", "flowschemas", "prioritylevelconfigurations", "bindings", "tokenreviews", "selfsubjectaccessreviews", "selfsubjectrulesreviews",
        "selfsubjectreviews", "subjectaccessreviews", "localsubjectaccessreviews", "pods", "replicasets", "csistoragecapacities"].contains(&base)
        || r.ends_with(".metrics.k8s.io") || r.ends_with("events.k8s.io")
}

/// Порядок применения: от того, на что ссылаются, к тому, что ссылается.
fn priority(kind: &str) -> u8 {
    match kind {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        "StorageClass" | "PriorityClass" | "IngressClass" | "RuntimeClass" => 2,
        "PersistentVolume" => 3,
        "PersistentVolumeClaim" => 4,
        "ServiceAccount" => 5,
        "ClusterRole" | "Role" => 6,
        "ClusterRoleBinding" | "RoleBinding" => 7,
        "Secret" | "ConfigMap" | "LimitRange" | "ResourceQuota" => 8,
        "Service" => 9,
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" | "Pod" | "ReplicaSet" => 11,
        "Ingress" | "HorizontalPodAutoscaler" | "PodDisruptionBudget" | "NetworkPolicy" => 12,
        "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration" => 13,
        _ => 10,
    }
}

/// Объект, пригодный для apply в новый или тот же кластер; None — не восстанавливать.
pub fn clean(mut o: Value) -> Option<Value> {
    let kind = o["kind"].as_str().unwrap_or("").to_string();
    let md = o.get("metadata")?.clone();
    let name = md["name"].as_str().unwrap_or("").to_string();
    let ns = md["namespace"].as_str().unwrap_or("").to_string();
    if md.get("ownerReferences").and_then(|x| x.as_array()).map(|a| !a.is_empty()).unwrap_or(false) { return None; }
    if kind == "Namespace" && (SYSTEM_NS.contains(&name.as_str()) || name == "default") { return None; }
    if (kind == "ServiceAccount" && name == "default") || (kind == "ConfigMap" && name == "kube-root-ca.crt") { return None; }
    if kind == "Secret" && o["type"] == "kubernetes.io/service-account-token" { return None; }
    if kind == "Service" && name == "kubernetes" && ns == "default" { return None; }
    if (kind == "ClusterRole" || kind == "ClusterRoleBinding" || kind == "Role" || kind == "RoleBinding") && (name.starts_with("system:") || md["labels"]["kubernetes.io/bootstrapping"].is_string()) { return None; }
    if kind == "StorageClass" && md["annotations"]["storageclass.kubernetes.io/is-default-class"].is_string() && md["labels"]["addonmanager.kubernetes.io/mode"].is_string() { return None; }
    let m = o["metadata"].as_object_mut()?;
    for k in ["uid", "resourceVersion", "creationTimestamp", "generation", "managedFields", "selfLink", "deletionTimestamp", "deletionGracePeriodSeconds"] { m.remove(k); }
    if let Some(a) = m.get_mut("annotations").and_then(|a| a.as_object_mut()) {
        for k in ["kubectl.kubernetes.io/last-applied-configuration", "deployment.kubernetes.io/revision", "pv.kubernetes.io/bind-completed", "pv.kubernetes.io/bound-by-controller", "pv.kubernetes.io/provisioned-by"] { a.remove(k); }
    }
    if let Some(x) = o.as_object_mut() { x.remove("status"); }
    match kind.as_str() {
        "Service" => {
            if let Some(s) = o["spec"].as_object_mut() {
                if s.get("clusterIP").and_then(|x| x.as_str()) != Some("None") { s.remove("clusterIP"); s.remove("clusterIPs"); }
                s.remove("healthCheckNodePort");
            }
        }
        "PersistentVolume" => { if let Some(c) = o["spec"]["claimRef"].as_object_mut() { c.remove("uid"); c.remove("resourceVersion"); } }
        "Job" => {
            if let Some(s) = o["spec"].as_object_mut() { s.remove("selector"); }
            if let Some(l) = o["spec"]["template"]["metadata"]["labels"].as_object_mut() { for k in ["controller-uid", "batch.kubernetes.io/controller-uid", "job-name", "batch.kubernetes.io/job-name"] { l.remove(k); } }
        }
        "Pod" => { if let Some(s) = o["spec"].as_object_mut() { s.remove("nodeName"); } }
        _ => {}
    }
    Some(o)
}

#[async_trait]
impl Plugin for Kube {
    fn id(&self) -> &'static str { "kubernetes" }

    async fn discover(&self, cfg: &Value, log: &LogFn) -> Result<Value> {
        let k = K::from(cfg)?;
        let v = k.json(&["version", "-o", "json"], log).await.unwrap_or_default();
        let ns = k.json(&["get", "namespaces", "-o", "json"], log).await?;
        Ok(json!({ "server": v["serverVersion"]["gitVersion"], "namespaces": ns["items"].as_array().into_iter().flatten().map(|n| n["metadata"]["name"].clone()).collect::<Vec<_>>() }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let k = K::from(&ctx.config)?;
        let ver = k.json(&["version", "-o", "json"], log).await.context("подключение к кластеру (kubectl version)")?;
        let server = ver["serverVersion"]["gitVersion"].as_str().unwrap_or("").to_string();
        let want_ns = crate::connectors::net::p_list(&ctx.config, "namespaces");
        let sys = kit::bool_param(&ctx.config, "include_system", false);
        let excl = crate::connectors::net::p_list(&ctx.config, "exclude_resources");
        let ns_ok = |ns: &str| (want_ns.is_empty() || want_ns.iter().any(|w| w == ns)) && (sys || !SYSTEM_NS.contains(&ns));
        log(&format!("Kubernetes {server} · kubeconfig {}", k.cfg));
        let mut w = ctx.repo.writer();
        let mut rmeta = Vec::new();
        let mut objects = 0usize;
        for namespaced in [true, false] {
            let list = kit::output(&k.cmd(&["api-resources", "--verbs=list", &format!("--namespaced={namespaced}"), "-o", "name"]), log).await?;
            for r in list.lines().map(str::trim).filter(|r| !r.is_empty()) {
                if skip_resource(r) || excl.iter().any(|e| e == r || r.split('.').next() == Some(e.as_str())) { continue; }
                let v = match k.json(&["get", r, if namespaced { "-A" } else { "--ignore-not-found" }, "-o", "json"], log).await { Ok(v) => v, Err(e) => { log(&format!("  {r}: {e:#} — пропущен")); continue } };
                let items: Vec<Value> = v["items"].as_array().cloned().unwrap_or_default().into_iter()
                    .filter(|o| !namespaced || ns_ok(o["metadata"]["namespace"].as_str().unwrap_or("")))
                    .filter(|o| r != "namespaces" || ns_ok(o["metadata"]["name"].as_str().unwrap_or("")))
                    .collect();
                if items.is_empty() { continue; }
                objects += items.len();
                kit::add_bytes(&mut w, &format!("resources/{r}.json"), serde_json::to_vec(&json!({ "apiVersion": "v1", "kind": "List", "items": items }))?).await?;
                rmeta.push(json!({ "resource": r, "namespaced": namespaced, "count": items.len() }));
            }
        }
        log(&format!("объектов: {objects} в {} типах ресурсов", rmeta.len()));
        // Данные томов hostPath/local.
        let mut pv_meta = Vec::new();
        if kit::bool_param(&ctx.config, "pv_data", true) {
            let pvs = k.json(&["get", "persistentvolumes", "-o", "json"], log).await.unwrap_or_default();
            let node = cfg_str(&ctx.config, "node_container");
            for pv in pvs["items"].as_array().into_iter().flatten() {
                let name = pv["metadata"]["name"].as_str().unwrap_or("").to_string();
                let Some(path) = pv["spec"]["hostPath"]["path"].as_str().or(pv["spec"]["local"]["path"].as_str()).map(|s| s.to_string()) else { continue };
                let claim_ns = pv["spec"]["claimRef"]["namespace"].as_str().unwrap_or("");
                if !claim_ns.is_empty() && !ns_ok(claim_ns) { continue; }
                let sname = format!("pv/{name}.tar");
                let r = match &node {
                    Some(n) => kit::stream_cmd(&mut w, &sname, &Cmd::new("tar").args(["-C", &path, "-cf", "-", "."]).container(Some(n.clone())), log).await.map(|_| ()),
                    None if Path::new(&path).is_dir() => kit::tar_paths(&mut w, &sname, &[path.clone()], Default::default(), Default::default(), log).await.map(|_| ()),
                    None => { log(&format!("  том {name}: {path} не на этой машине — данные не скопированы (укажите контейнер узла или копируйте на узле)")); continue }
                };
                if let Err(e) = r { log(&format!("  том {name}: {e:#}")); continue; }
                pv_meta.push(json!({ "name": name, "path": path, "node_container": node, "claim": format!("{claim_ns}/{}", pv["spec"]["claimRef"]["name"].as_str().unwrap_or("")) }));
            }
        }
        // Снимок etcd (kubeadm).
        let mut etcd = false;
        let mode = cfg_str(&ctx.config, "etcd").unwrap_or_else(|| "auto".into());
        let ca = cfg_str(&ctx.config, "etcd_cacert").unwrap_or_else(|| "/etc/kubernetes/pki/etcd/ca.crt".into());
        let endpoints = cfg_str(&ctx.config, "etcd_endpoints").unwrap_or_else(|| "https://127.0.0.1:2379".into());
        let tls = endpoints.starts_with("https");
        if mode == "on" || (mode == "auto" && kit::which("etcdctl").is_some() && (!tls || Path::new(&ca).exists())) {
            let wd = kit::WorkDir::new(None, None)?;
            let f = wd.join("etcd.db");
            let mut c = Cmd::new("etcdctl").env("ETCDCTL_API", "3").arg(format!("--endpoints={endpoints}"));
            if tls {
                c = c.arg(format!("--cacert={ca}")).arg(format!("--cert={}", cfg_str(&ctx.config, "etcd_cert").unwrap_or_else(|| "/etc/kubernetes/pki/etcd/server.crt".into())))
                    .arg(format!("--key={}", cfg_str(&ctx.config, "etcd_key").unwrap_or_else(|| "/etc/kubernetes/pki/etcd/server.key".into())));
            }
            kit::run(&c.args(["snapshot", "save", &f.to_string_lossy()]), log).await.context("etcdctl snapshot save")?;
            let e = w.add_stream("etcd.db", std::fs::File::open(&f)?, None).await?;
            log(&format!("  etcd.db: {}", crate::util::human_bytes(e.size)));
            etcd = true;
        } else if mode == "on" { bail!("нет etcdctl"); }
        if Path::new("/etc/kubernetes/pki").is_dir() { kit::tar_paths(&mut w, "etc-kubernetes.tar", &["/etc/kubernetes".to_string()], Default::default(), Default::default(), log).await?; }
        let meta = json!({ "server": server, "resources": rmeta, "objects": objects, "pv": pv_meta, "etcd": etcd });
        let (header, stats) = w.finish(&ctx.instance, "kubernetes", "full", json!({ "server": server, "objects": objects }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let want_ns = crate::connectors::net::p_list(&ctx.params, "namespaces");
        let kinds = crate::connectors::net::p_list(&ctx.params, "kinds");
        let mut objs: Vec<Value> = Vec::new();
        for s in m.streams.iter().filter(|s| s.name.starts_with("resources/")) {
            let res = s.name.trim_start_matches("resources/").trim_end_matches(".json");
            if !kinds.is_empty() && !kinds.iter().any(|k| k == res || res.split('.').next() == Some(k.as_str())) { continue; }
            let v: Value = serde_json::from_slice(&kit::read_small(&ctx.repo, s).await?)?;
            for o in v["items"].as_array().cloned().unwrap_or_default() {
                let ns = o["metadata"]["namespace"].as_str().unwrap_or("");
                let name = o["metadata"]["name"].as_str().unwrap_or("");
                if !want_ns.is_empty() && !(want_ns.iter().any(|w| w == ns) || (o["kind"] == "Namespace" && want_ns.iter().any(|w| w == name)) || (ns.is_empty() && o["kind"] != "Namespace")) { continue; }
                if let Some(c) = clean(o) { objs.push(c); }
            }
        }
        objs.sort_by_key(|o| priority(o["kind"].as_str().unwrap_or("")));
        if let Some(t) = cfg_str(&ctx.params, "target_dir") {
            std::fs::create_dir_all(&t)?;
            std::fs::write(Path::new(&t).join("objects.json"), serde_json::to_vec_pretty(&json!({ "apiVersion": "v1", "kind": "List", "items": objs }))?)?;
            if let Ok(s) = kit::stream(&m, "etcd.db") { kit::to_file(&ctx.repo, s, &Path::new(&t).join("etcd.db")).await?; }
            for s in m.streams.iter().filter(|s| s.name.starts_with("pv/") || s.name == "etc-kubernetes.tar") { kit::untar(&ctx.repo, s, &Path::new(&t).join(s.name.trim_end_matches(".tar")), Default::default(), log).await?; }
            log(&format!("объектов: {} → {t}/objects.json{}", objs.len(), if m.meta["etcd"] == true { "; снимок etcd → etcd.db (etcdutl snapshot restore на узле управления)" } else { "" }));
            return Ok(json!({ "target_dir": t, "objects": objs.len() }));
        }
        let k = K::from(&ctx.params)?;
        let dry = kit::bool_param(&ctx.params, "dry_run", false);
        // Данные томов — до рабочих нагрузок.
        let mut pv_done = Vec::new();
        let (first, rest): (Vec<Value>, Vec<Value>) = objs.into_iter().partition(|o| priority(o["kind"].as_str().unwrap_or("")) <= 4);
        let apply = |batch: Vec<Value>| { let k = &k; async move {
            if batch.is_empty() { return Ok::<usize, anyhow::Error>(0); }
            let n = batch.len();
            let body = serde_json::to_vec(&json!({ "apiVersion": "v1", "kind": "List", "items": batch }))?;
            let mut c = k.cmd(&["apply", "-f", "-"]);
            if dry { c = c.arg("--dry-run=server"); }
            kit::feed_read(Box::new(std::io::Cursor::new(body)), "объекты", &c, log).await?;
            Ok(n)
        } };
        let mut applied = apply(first).await.context("kubectl apply (пространства имён, CRD, тома)")?;
        if !dry && kit::bool_param(&ctx.params, "pv_data", true) {
            for p in m.meta["pv"].as_array().cloned().unwrap_or_default() {
                let name = p["name"].as_str().unwrap_or("");
                let path = kit::safe_abs(p["path"].as_str().unwrap_or(""))?;
                let Ok(s) = kit::stream(&m, &format!("pv/{name}.tar")) else { continue };
                match p["node_container"].as_str().or(cfg_str(&ctx.params, "node_container").as_deref()) {
                    Some(n) => {
                        let n = n.to_string();
                        kit::output(&Cmd::new("mkdir").args(["-p", &path]).container(Some(n.clone())), log).await?;
                        let strip = path.trim_matches('/').split('/').count().to_string();
                        kit::feed(&ctx.repo, s, &Cmd::new("tar").args(["-C", &path, "-xf", "-", "--strip-components", &strip]).container(Some(n)), log).await?;
                    }
                    None => { std::fs::create_dir_all(&path)?; kit::untar(&ctx.repo, s, Path::new("/"), Default::default(), log).await?; }
                }
                pv_done.push(name.to_string());
            }
        }
        // CRD должны зарегистрироваться до своих объектов.
        if !dry { let _ = kit::output(&k.cmd(&["wait", "--for=condition=Established", "crd", "--all", "--timeout=60s"]), log).await; }
        let mut by_pri: std::collections::BTreeMap<u8, Vec<Value>> = Default::default();
        for o in rest { by_pri.entry(priority(o["kind"].as_str().unwrap_or(""))).or_default().push(o); }
        for (_, b) in by_pri { applied += apply(b).await.context("kubectl apply")?; }
        log(&format!("применено объектов: {applied}{}", if dry { " (проверка, без изменений)" } else { "" }));
        Ok(json!({ "applied": applied, "pv_data": pv_done, "dry_run": dry, "etcd_in_backup": m.meta["etcd"] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleans() {
        let svc = json!({ "kind": "Service", "metadata": { "name": "web", "namespace": "shop", "uid": "u", "resourceVersion": "1", "annotations": { "kubectl.kubernetes.io/last-applied-configuration": "{}" } }, "spec": { "clusterIP": "10.0.0.5", "clusterIPs": ["10.0.0.5"], "ports": [] }, "status": {} });
        let c = clean(svc).unwrap();
        assert!(c["metadata"]["uid"].is_null() && c["spec"]["clusterIP"].is_null() && c["status"].is_null());
        assert!(c["metadata"]["annotations"].as_object().unwrap().is_empty());
        let owned = json!({ "kind": "Pod", "metadata": { "name": "p", "ownerReferences": [{ "kind": "ReplicaSet" }] } });
        assert!(clean(owned).is_none());
        assert!(clean(json!({ "kind": "Secret", "type": "kubernetes.io/service-account-token", "metadata": { "name": "t" } })).is_none());
        assert!(skip_resource("events.events.k8s.io") && skip_resource("pods") && !skip_resource("deployments.apps"));
        assert!(priority("Namespace") < priority("Deployment"));
    }
}
