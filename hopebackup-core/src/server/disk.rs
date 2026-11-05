//! Площадка «диск сервера управления»: каталог на сервере (большой диск, примонтированный
//! администратором, например /mnt/data) обслуживается встроенным узлом хранения по адресу
//! `https://<сервер>:4443/node/<площадка>/…`. Агенты пишут туда через порт сервера с тем же
//! сертификатом; токены — как у отдельного узла (агенты — append, очистка — admin).

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tower::ServiceExt;

use super::Server;

impl Server {
    fn disk_router(&self, id: &str) -> Result<axum::Router, String> {
        if let Some(r) = self.disk.read().get(id) { return Ok(r.clone()); }
        // Один экземпляр узла (и его защита pack-файлов от очистки) на площадку: строим под блокировкой.
        let mut g = self.disk.write();
        if let Some(r) = g.get(id) { return Ok(r.clone()); }
        let path = self.store.read(|st| st.sites.iter().find(|s| s.id == id && s.kind == "disk").and_then(|s| s.disk.as_ref().map(|d| d.path.clone()))).ok_or("площадка не найдена")?;
        let p = std::path::PathBuf::from(&path);
        let cfg = crate::node::load_config(&p).map_err(|e| { tracing::warn!("площадка {id} ({path}): {e:#}"); "каталог площадки недоступен на этом сервере".to_string() })?;
        let r = crate::node::router(p, cfg);
        g.insert(id.to_string(), r.clone());
        Ok(r)
    }

    /// Забыть встроенный узел (площадка удалена).
    pub fn disk_forget(&self, id: &str) { self.disk.write().remove(id); }
}

/// Сервис для `nest_service("/node", …)`: путь приходит без префикса `/node`.
pub fn service(s: Arc<Server>) -> impl tower::Service<Request, Response = Response, Error = std::convert::Infallible, Future = impl Send> + Clone + Send + 'static {
    tower::service_fn(move |req: Request| {
        let s = s.clone();
        async move { Ok::<_, std::convert::Infallible>(dispatch(s, req).await) }
    })
}

pub async fn dispatch(s: Arc<Server>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let rest = path.trim_start_matches('/');
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let router = match s.disk_router(id) {
        Ok(r) => r,
        Err(e) => return (StatusCode::NOT_FOUND, e).into_response(),
    };
    let pq = format!("/{tail}{}", req.uri().query().map(|q| format!("?{q}")).unwrap_or_default());
    let (mut parts, body) = req.into_parts();
    parts.uri = match pq.parse() { Ok(u) => u, Err(_) => return StatusCode::BAD_REQUEST.into_response() };
    match router.oneshot(Request::from_parts(parts, body)).await {
        Ok(r) => r.into_response(),
        Err(e) => match e {},
    }
}

/// Место на диске площадки: (всего, занято).
pub fn usage(path: &str) -> (u64, u64) { crate::plugins::host::inventory::statvfs(path) }
