//! Исполнитель заданий: одинаковый для агента, сервера и `restore-exec` на цели восстановления.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::connectors::TunnelManager;
use crate::plugins::{self, BackupCtx, LogFn, RestoreCtx};
use crate::proto::JobSpec;
use crate::store::{backend_for, Repo};

pub async fn run_job(spec: &JobSpec, tunnels: &TunnelManager, log: LogFn) -> Result<Value> {
    match spec.kind.as_str() {
        "discover" => {
            let inst = spec.instance.as_ref().ok_or_else(|| anyhow!("нет экземпляра"))?;
            let p = plugins::get(&inst.plugin).ok_or_else(|| anyhow!("неизвестный плагин {}", inst.plugin))?;
            let mut cfg = inst.config.clone();
            if let Some(o) = cfg.as_object_mut() { o.insert("_instance".into(), json!(inst.id)); }
            p.discover(&cfg, &log).await
        }
        "action" => {
            let inst = spec.instance.as_ref().ok_or_else(|| anyhow!("нет экземпляра"))?;
            let p = plugins::get(&inst.plugin).ok_or_else(|| anyhow!("неизвестный плагин {}", inst.plugin))?;
            // Параметры задания (переезд: платформа цели и её состояние) — поверх параметров экземпляра.
            let mut cfg = inst.config.clone();
            if let (Some(c), Some(p)) = (cfg.as_object_mut(), spec.params.as_object()) { for (k, v) in p { c.insert(k.clone(), v.clone()); } }
            if !cfg.is_object() && spec.params.is_object() { cfg = spec.params.clone(); }
            p.action(&spec.mode, &cfg, &inst.id, &log).await
        }
        "init_repo" => {
            let site = spec.site.as_ref().ok_or_else(|| anyhow!("нет площадки"))?;
            let remote = tunnels.connect(site, &*log).await?;
            let repo = Repo::init(backend_for(&remote)?, &site.password).await?;
            log(&format!("репозиторий «{}» создан на площадке «{}» (id {})", site.remote.repo, site.name, repo.cfg.id));
            Ok(json!({ "repo_id": repo.cfg.id }))
        }
        "backup" => {
            let inst = spec.instance.as_ref().ok_or_else(|| anyhow!("нет экземпляра"))?;
            let site = spec.site.as_ref().ok_or_else(|| anyhow!("у экземпляра не задана площадка"))?;
            let p = plugins::get(&inst.plugin).ok_or_else(|| anyhow!("неизвестный плагин {}", inst.plugin))?;
            let remote = tunnels.connect(site, &*log).await?;
            let repo = Arc::new(Repo::open(backend_for(&remote)?, &site.password).await?.with_level(site.compression).with_job(&spec.id));
            let started = std::time::Instant::now();
            let ctx = BackupCtx { repo: repo.clone(), instance: inst.id.clone(), config: inst.config.clone(), kind: spec.mode.clone(), log: log.clone(), job_id: spec.id.clone() };
            let out = p.backup(&ctx).await;
            repo.backend.release().await;
            let out = out?;
            let s = &out.stats;
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let saving = if s.uploaded > 0 { format!("экономия ×{:.1}", s.bytes_read as f64 / s.uploaded as f64) } else { "все блоки уже были на площадке".to_string() };
            log(&format!(
                "готово за {:.0} с: прочитано {}, новых {}, по каналу {} ({saving}), снапшот {}",
                secs,
                crate::util::human_bytes(s.bytes_read),
                crate::util::human_bytes(s.new_bytes),
                crate::util::human_bytes(s.uploaded),
                out.header.id
            ));
            Ok(json!({ "snapshot": out.header, "stats": out.stats, "seconds": secs }))
        }
        "restore" => {
            let inst = spec.instance.as_ref().ok_or_else(|| anyhow!("нет экземпляра"))?;
            let site = spec.site.as_ref().ok_or_else(|| anyhow!("нет площадки"))?;
            let p = plugins::get(&inst.plugin).ok_or_else(|| anyhow!("неизвестный плагин {}", inst.plugin))?;
            let snap = spec.snapshot_id.clone().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("не выбрана точка восстановления"))?;
            let remote = tunnels.connect(site, &*log).await?;
            let repo = Arc::new(Repo::open(backend_for(&remote)?, &site.password).await?);
            let mut params: Value = spec.params.clone();
            // Параметры экземпляра (проверка, доступ к хранилищу и т. п.) — по умолчанию для восстановления.
            if !params.is_object() { params = json!({}); }
            if let (Some(pm), Some(cm)) = (params.as_object_mut(), inst.config.as_object()) {
                for k in plugins::restore_defaults(&inst.plugin).iter().copied() {
                    // Пустое поле мастера — «как у экземпляра».
                    let empty = pm.get(k).map(|v| v.is_null() || v.as_str() == Some("")).unwrap_or(true);
                    if empty { if let Some(v) = cm.get(k) { pm.insert(k.into(), v.clone()); } }
                }
            }
            let ctx = RestoreCtx { repo, instance: inst.id.clone(), snapshot_id: snap, params, log: log.clone(), job_id: spec.id.clone(), remote };
            p.restore(&ctx).await
        }
        k => bail!("неизвестный вид задания {k}"),
    }
}
