//! Elasticsearch и OpenSearch: штатный Snapshot API.
//!  • Репозиторий fs (каталог из path.repo на этой машине; в кластере — общий для всех узлов): кластер
//!    пишет снимок туда, HopeBackup забирает каталог репозитория в свою копию (неизменные сегменты
//!    дедуплицируются) и оставляет в каталоге только последний снимок.
//!  • Свой репозиторий кластера (S3, HDFS…): HopeBackup только создаёт и учитывает снимки.
//! Восстановление: каталог из копии → отдельный репозиторий только для чтения → _restore выбранных индексов
//! (существующие закрываются или восстанавливаются под другим именем).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use super::appkit::{self as kit};
use super::{cfg_str, BackupCtx, BackupOutcome, LogFn, Plugin, RestoreCtx};

pub struct OpenSearch;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "url": { "type": "string", "title": "Адрес кластера", "default": "https://127.0.0.1:9200" },
        "user": { "type": "string", "title": "Пользователь", "description": "Права: manage snapshots (cluster:admin/snapshot/*, cluster:admin/repository/*), monitor" },
        "password": { "type": "string", "format": "secret", "title": "Пароль" },
        "api_key": { "type": "string", "format": "secret", "title": "API-ключ (Elasticsearch)", "description": "Вместо пользователя: id:api_key в base64" },
        "fingerprint": { "type": "string", "title": "Отпечаток сертификата", "description": "SHA-256 сертификата узла; пусто — системные CA" },
        "insecure": { "type": "boolean", "title": "Не проверять сертификат", "default": false },
        "repo_path": { "type": "string", "title": "Каталог репозитория fs", "default": "/var/backups/opensearch", "description": "Должен быть в path.repo (opensearch.yml / elasticsearch.yml); в кластере — общий для всех узлов" },
        "repository": { "type": "string", "title": "Свой репозиторий кластера", "description": "Имя уже зарегистрированного репозитория (S3 и т. п.): HopeBackup только создаёт снимки, данные остаются в нём" },
        "indices": { "type": "array", "items": { "type": "string" }, "title": "Индексы", "description": "Шаблоны; пусто — все (кроме системных)" },
        "include_global_state": { "type": "boolean", "title": "Глобальное состояние (шаблоны, настройки кластера)", "default": true },
        "config_dirs": { "type": "array", "items": { "type": "string" }, "title": "Конфигурация", "description": "Пусто — /etc/opensearch или /etc/elasticsearch (вместе с keystore)" }
    } })
}

pub fn defaults() -> Value { json!({ "url": "https://127.0.0.1:9200", "repo_path": "/var/backups/opensearch", "include_global_state": true }) }

pub fn restore_schema() -> Value {
    json!({ "type": "object", "properties": {
        "indices": { "type": "array", "items": { "type": "string" }, "title": "Индексы", "description": "Пусто — все из снимка" },
        "rename_prefix": { "type": "string", "title": "Под именем с префиксом", "description": "Например restored_ — восстановить рядом с рабочими" },
        "replace": { "type": "boolean", "title": "Заменить существующие индексы", "default": true, "description": "Существующие закрываются и восстанавливаются из снимка; выключено — восстановление остановится" },
        "include_global_state": { "type": "boolean", "title": "Глобальное состояние", "default": false },
        "restore_config": { "type": "boolean", "title": "Восстановить конфигурацию", "default": false },
        "url": { "type": "string", "title": "Целевой кластер" }
    } })
}

pub fn restore_keys() -> &'static [&'static str] { &["url", "user", "password", "api_key", "fingerprint", "insecure", "repo_path", "repository"] }

struct Api { c: reqwest::Client, url: String, auth: Option<(String, String)>, key: Option<String> }

impl Api {
    fn from(v: &Value) -> Result<Api> {
        let c = kit::http(&cfg_str(v, "fingerprint").unwrap_or_default(), kit::bool_param(v, "insecure", false), 3600)?;
        let url = cfg_str(v, "url").unwrap_or_else(|| "https://127.0.0.1:9200".into()).trim_end_matches('/').to_string();
        Ok(Api { c, url, auth: cfg_str(v, "user").map(|u| (u, cfg_str(v, "password").unwrap_or_default())), key: cfg_str(v, "api_key") })
    }
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut r = self.c.request(method.clone(), format!("{}{path}", self.url));
        if let Some((u, p)) = &self.auth { r = r.basic_auth(u, Some(p)); }
        if let Some(k) = &self.key { r = r.header("Authorization", format!("ApiKey {k}")); }
        if let Some(b) = body { r = r.json(&b); }
        let resp = r.send().await.with_context(|| format!("{} недоступен", self.url))?;
        let st = resp.status();
        let t = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&t).unwrap_or(json!({ "text": t }));
        if !st.is_success() {
            let reason = v["error"]["root_cause"][0]["reason"].as_str().or(v["error"]["reason"].as_str()).map(|s| s.to_string()).unwrap_or_else(|| t.chars().take(300).collect());
            bail!("{method} {path}: {} — {reason}", st.as_u16());
        }
        Ok(v)
    }
    async fn get(&self, p: &str) -> Result<Value> { self.call(reqwest::Method::GET, p, None).await }
    async fn put(&self, p: &str, b: Value) -> Result<Value> { self.call(reqwest::Method::PUT, p, Some(b)).await }
    async fn post(&self, p: &str, b: Value) -> Result<Value> { self.call(reqwest::Method::POST, p, Some(b)).await }
    async fn delete(&self, p: &str) -> Result<Value> { self.call(reqwest::Method::DELETE, p, None).await }
}

const REPO: &str = "hopebackup";

fn flavor(v: &Value) -> String {
    if v["version"]["distribution"] == "opensearch" { format!("OpenSearch {}", v["version"]["number"].as_str().unwrap_or("")) } else { format!("Elasticsearch {}", v["version"]["number"].as_str().unwrap_or("")) }
}

#[async_trait]
impl Plugin for OpenSearch {
    fn id(&self) -> &'static str { "opensearch" }

    async fn discover(&self, cfg: &Value, _log: &LogFn) -> Result<Value> {
        let a = Api::from(cfg)?;
        let v = a.get("/").await?;
        let h = a.get("/_cluster/health").await.unwrap_or_default();
        let repos = a.get("/_snapshot").await.unwrap_or_default();
        Ok(json!({ "product": flavor(&v), "cluster": v["cluster_name"], "status": h["status"], "nodes": h["number_of_nodes"], "repositories": repos.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()) }))
    }

    async fn backup(&self, ctx: &BackupCtx) -> Result<BackupOutcome> {
        let log = &ctx.log;
        let a = Api::from(&ctx.config)?;
        let v = a.get("/").await.context("подключение к кластеру")?;
        let product = flavor(&v);
        let h = a.get("/_cluster/health").await.unwrap_or_default();
        log(&format!("{product} · кластер {} · узлов {} · состояние {}", v["cluster_name"].as_str().unwrap_or(""), h["number_of_nodes"], h["status"].as_str().unwrap_or("?")));
        let managed = cfg_str(&ctx.config, "repository");
        let repo = managed.clone().unwrap_or_else(|| REPO.into());
        let repo_path = cfg_str(&ctx.config, "repo_path").unwrap_or_else(|| "/var/backups/opensearch".into());
        if managed.is_none() {
            if !Path::new(&repo_path).is_dir() { bail!("каталог репозитория {repo_path} не найден на этой машине — создайте его (владелец — пользователь службы), добавьте в path.repo и перезапустите узлы"); }
            a.put(&format!("/_snapshot/{REPO}"), json!({ "type": "fs", "settings": { "location": repo_path, "compress": false } })).await
                .map_err(|e| anyhow!("{e:#}\nрегистрация репозитория fs: каталог {repo_path} должен быть в path.repo на всех узлах"))?;
        }
        let snap = format!("hb-{}-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"), &crate::util::short_id()[..6]);
        let indices = crate::connectors::net::p_list(&ctx.config, "indices");
        let body = json!({ "indices": if indices.is_empty() { "*,-.*".to_string() } else { indices.join(",") }, "ignore_unavailable": true, "include_global_state": kit::bool_param(&ctx.config, "include_global_state", true) });
        log(&format!("снимок {repo}/{snap}"));
        let r = a.put(&format!("/_snapshot/{repo}/{snap}?wait_for_completion=true"), body).await.context("создание снимка")?;
        let s = &r["snapshot"];
        let state = s["state"].as_str().unwrap_or("").to_string();
        if state != "SUCCESS" && state != "PARTIAL" { bail!("снимок завершился с состоянием {state}: {}", s["failures"]); }
        if state == "PARTIAL" { log(&format!("! снимок частичный: {}", s["failures"])); }
        let idx: Vec<String> = s["indices"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|y| y.to_string())).collect();
        log(&format!("  индексов: {}, шардов {}/{}", idx.len(), s["shards"]["successful"], s["shards"]["total"]));
        let mut w = ctx.repo.writer();
        if managed.is_none() {
            kit::tar_paths(&mut w, "repository.tar", &[repo_path.clone()], Default::default(), Default::default(), log).await?;
            // В локальном каталоге — только последний снимок: история — в копиях HopeBackup.
            if let Ok(all) = a.get(&format!("/_snapshot/{REPO}/_all")).await {
                for old in all["snapshots"].as_array().into_iter().flatten().filter_map(|x| x["snapshot"].as_str()).filter(|n| *n != snap) {
                    if let Err(e) = a.delete(&format!("/_snapshot/{REPO}/{old}")).await { log(&format!("  удаление старого снимка {old}: {e:#}")); }
                }
            }
        }
        let mut conf = crate::connectors::net::p_list(&ctx.config, "config_dirs");
        if conf.is_empty() { conf = ["/etc/opensearch", "/etc/elasticsearch"].iter().filter(|p| Path::new(p).is_dir()).map(|s| s.to_string()).collect(); }
        if !conf.is_empty() { kit::tar_paths(&mut w, "config.tar", &conf, Default::default(), Default::default(), log).await?; }
        kit::add_bytes(&mut w, "snapshot.json", serde_json::to_vec(s)?).await?;
        let meta = json!({ "product": product, "snapshot": snap, "repository": repo, "managed": managed.is_some(), "repo_path": repo_path, "indices": idx, "state": state });
        let (header, stats) = w.finish(&ctx.instance, "opensearch", "full", json!({ "snapshot": snap, "indices": idx.len() }), meta).await?;
        Ok(BackupOutcome { header, stats })
    }

    async fn restore(&self, ctx: &RestoreCtx) -> Result<Value> {
        let log = &ctx.log;
        let (_, m) = ctx.repo.manifest(&ctx.snapshot_id).await?;
        let a = Api::from(&ctx.params)?;
        let snap = m.meta["snapshot"].as_str().unwrap_or("").to_string();
        let mut repo = m.meta["repository"].as_str().unwrap_or(REPO).to_string();
        let mut tmp_dir = None;
        if m.meta["managed"] != true {
            // Каталог из копии — рядом с рабочим репозиторием (внутри path.repo), отдельным репозиторием только для чтения.
            let base = cfg_str(&ctx.params, "repo_path").or_else(|| m.meta["repo_path"].as_str().map(|s| s.to_string())).unwrap_or_else(|| "/var/backups/opensearch".into());
            let dir = format!("{}/hopebackup-restore-{}", base.trim_end_matches('/'), crate::util::short_id());
            let orig = m.meta["repo_path"].as_str().unwrap_or("").trim_start_matches('/').to_string();
            std::fs::create_dir_all(&dir)?;
            let owner = std::fs::metadata(&base).ok().map(|x| { use std::os::unix::fs::MetadataExt; x.uid() }).and_then(crate::util::user_name);
            kit::untar(&ctx.repo, kit::stream(&m, "repository.tar")?, Path::new("/"), kit::Untar { relocate: vec![(orig, dir.trim_start_matches('/').to_string())], owner: owner.clone(), ..Default::default() }, log).await?;
            if let Some(o) = owner.filter(|o| o != "root") { kit::chown_to(&[dir.clone()], &o, log)?; }
            repo = format!("hopebackup-restore-{}", crate::util::short_id());
            a.put(&format!("/_snapshot/{repo}"), json!({ "type": "fs", "settings": { "location": dir, "readonly": true } })).await.context("регистрация каталога из копии как репозитория")?;
            tmp_dir = Some(dir);
        }
        let r = async {
            let info = a.get(&format!("/_snapshot/{repo}/{snap}")).await?;
            let all: Vec<String> = info["snapshots"][0]["indices"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
            let want = crate::connectors::net::p_list(&ctx.params, "indices");
            let sel: Vec<String> = if want.is_empty() { all.clone() } else { all.iter().filter(|i| want.iter().any(|w| glob(w, i))).cloned().collect() };
            if sel.is_empty() { bail!("в снимке нет подходящих индексов (есть: {})", all.join(", ")); }
            let prefix = cfg_str(&ctx.params, "rename_prefix");
            let mut body = json!({ "indices": sel.join(","), "include_global_state": kit::bool_param(&ctx.params, "include_global_state", false), "ignore_unavailable": true });
            if let Some(p) = &prefix { body["rename_pattern"] = json!("(.+)"); body["rename_replacement"] = json!(format!("{p}$1")); }
            else {
                // Существующие индексы восстанавливаются только закрытыми.
                let existing: Vec<String> = sel.iter().filter(|_| true).cloned().collect();
                let mut open = Vec::new();
                for i in &existing { if a.get(&format!("/{i}/_settings")).await.is_ok() { open.push(i.clone()); } }
                if !open.is_empty() {
                    if !kit::bool_param(&ctx.params, "replace", true) { bail!("индексы уже есть: {} — включите замену или задайте префикс", open.join(", ")); }
                    a.post(&format!("/{}/_close", open.join(",")), json!({})).await.context("закрытие существующих индексов")?;
                    log(&format!("закрыты для замены: {}", open.join(", ")));
                }
            }
            a.post(&format!("/_snapshot/{repo}/{snap}/_restore?wait_for_completion=true"), body).await.context("восстановление снимка")?;
            let names: Vec<String> = sel.iter().map(|i| format!("{}{i}", prefix.clone().unwrap_or_default())).collect();
            let h = a.get(&format!("/_cluster/health/{}?wait_for_status=yellow&timeout=120s", names.join(","))).await.unwrap_or_default();
            log(&format!("восстановлено индексов: {} · состояние {}", names.len(), h["status"].as_str().unwrap_or("?")));
            Ok::<_, anyhow::Error>(json!({ "indices": names, "status": h["status"] }))
        }.await;
        if tmp_dir.is_some() {
            let _ = a.delete(&format!("/_snapshot/{repo}")).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Some(d) = &tmp_dir { let _ = std::fs::remove_dir_all(d); }
        }
        let mut res = r?;
        if kit::bool_param(&ctx.params, "restore_config", false) {
            if let Ok(s) = kit::stream(&m, "config.tar") { kit::untar(&ctx.repo, s, Path::new("/"), Default::default(), log).await?; res["config"] = json!(true); }
        }
        Ok(res)
    }
}

fn glob(p: &str, s: &str) -> bool { match p.strip_suffix('*') { Some(pre) => s.starts_with(pre), None => p == s } }
