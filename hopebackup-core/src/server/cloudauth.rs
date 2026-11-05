//! Вход в облачные диски (OAuth по коду устройства) и продление токенов Яндекс Диска.

use chrono::Utc;
use serde_json::{json, Value};
use std::sync::Arc;

use super::Server;
use crate::cloud::oauth::{self, DeviceStart, Poll, Tokens};

pub struct Flow {
    pub provider: String,
    pub client_id: String,
    pub endpoint: String,
    pub status: String,
    pub error: String,
    pub tokens: Option<Tokens>,
    pub created: chrono::DateTime<Utc>,
}

impl Server {
    /// Не больше 20 незавершённых входов одновременно.
    pub fn oauth_busy(&self) -> bool {
        let now = Utc::now();
        let mut m = self.oauth.lock();
        m.retain(|_, f| (now - f.created).num_minutes() < 30);
        m.values().filter(|f| f.status == "pending").count() >= 20
    }

    pub fn oauth_begin(self: &Arc<Self>, flow: &str, provider: &str, endpoint: &str, client_id: &str, client_secret: &str, d: &DeviceStart) {
        let now = Utc::now();
        {
            let mut m = self.oauth.lock();
            m.retain(|_, f| (now - f.created).num_minutes() < 30);
            m.insert(flow.to_string(), Flow { provider: provider.into(), client_id: client_id.trim().into(), endpoint: endpoint.trim().into(), status: "pending".into(), error: String::new(), tokens: None, created: now });
        }
        let (s, flow, provider, endpoint, cid, secret, code) = (self.clone(), flow.to_string(), provider.to_string(), endpoint.to_string(), client_id.to_string(), client_secret.to_string(), d.device_code.clone());
        let (mut interval, deadline) = (d.interval, std::time::Instant::now() + std::time::Duration::from_secs(d.expires_in.min(1800)));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                if std::time::Instant::now() > deadline { s.oauth_set(&flow, "error", "время на подтверждение истекло", None); return; }
                if !s.oauth.lock().contains_key(&flow) { return; }
                match oauth::device_poll(&provider, &endpoint, &cid, &secret, &code).await {
                    Ok(Poll::Pending) => {}
                    Ok(Poll::SlowDown) => interval += 5,
                    Ok(Poll::Done(t)) => { s.oauth_set(&flow, "done", "", Some(t)); return; }
                    Err(e) => { s.oauth_set(&flow, "error", &format!("{e:#}"), None); return; }
                }
            }
        });
    }

    fn oauth_set(&self, flow: &str, status: &str, err: &str, t: Option<Tokens>) {
        if let Some(f) = self.oauth.lock().get_mut(flow) {
            f.status = status.into();
            f.error = err.into();
            if t.is_some() { f.tokens = t; }
        }
    }

    pub fn oauth_status(&self, flow: &str) -> Option<Value> {
        self.oauth.lock().get(flow).map(|f| json!({ "status": f.status, "error": f.error, "provider": f.provider }))
    }

    /// Забрать токены завершённого входа (один раз) — только для того же облака, приложения и адреса.
    pub fn oauth_take(&self, flow: &str, provider: &str, client_id: &str, endpoint: &str) -> Option<Tokens> {
        let mut m = self.oauth.lock();
        let ok = m.get(flow).map(|f| f.status == "done" && f.provider == provider && f.client_id == client_id.trim() && f.endpoint == endpoint.trim()).unwrap_or(false);
        if ok { m.remove(flow).and_then(|f| f.tokens) } else { None }
    }
}

/// Токен Яндекса живёт год: за 30 дней до конца сервер продлевает его токеном обновления.
pub async fn token_loop(s: Arc<Server>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        if !s.is_active() { continue; }
        let soon = Utc::now().timestamp() + 30 * 86400;
        let due: Vec<(String, crate::cloud::drive::CloudAccess)> = s.store.read(|st| {
            st.sites.iter().filter_map(|x| x.cloud.as_ref().filter(|c| c.provider == "yadisk" && !c.refresh_token.is_empty() && c.token_expires > 0 && c.token_expires < soon).map(|c| (x.id.clone(), c.clone()))).collect()
        });
        for (id, c) in due {
            let url = oauth::endpoints("yadisk", &c.endpoint).map(|e| e.token).unwrap_or_default();
            match oauth::refresh("yadisk", &url, &c.client_id, &c.client_secret, &c.refresh_token).await {
                Ok((t, exp)) => {
                    let _ = s.store.update(|st| {
                        if let Some(cc) = st.sites.iter_mut().find(|x| x.id == id).and_then(|x| x.cloud.as_mut()) {
                            cc.access_token = t.access_token.clone();
                            if !t.refresh_token.is_empty() { cc.refresh_token = t.refresh_token.clone(); }
                            cc.token_expires = exp;
                        }
                        Ok(())
                    });
                    s.audit("system", "cloud_token", format!("площадка {id}: токен Яндекс Диска продлён"));
                }
                Err(e) => tracing::warn!("площадка {id}: продление токена Яндекс Диска: {e:#}"),
            }
        }
    }
}
