// API Server module - HTTP REST + WebSocket endpoints
use crate::geoip::{GeoProfile, SharedGeoProfile};
use crate::instance::{InstanceKind, InstanceManager, RequestLogEntry, ValidationStatus};
use crate::proxy::ProxyManager;
use crate::storage::Storage;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    extract::Query,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::services::ServeDir;

// Global state for broadcasting updates
#[derive(Clone)]
pub struct AppState {
    pub tx: broadcast::Sender<String>,
    pub instance_manager: Arc<RwLock<InstanceManager>>,
    pub storage: Arc<Storage>,
    pub log_buffer: Arc<RwLock<Vec<LogEntry>>>,
    pub geo_profile: SharedGeoProfile,
    pub proxy_manager: Arc<ProxyManager>,
}

pub type LogEntry = RequestLogEntry;

const MAX_REQUEST_LOGS: usize = 2_000;
const REQUEST_LOG_RETENTION: ChronoDuration = ChronoDuration::minutes(30);

fn prune_request_logs(logs: &mut Vec<LogEntry>) {
    let cutoff = Utc::now() - REQUEST_LOG_RETENTION;
    logs.retain(|entry| {
        DateTime::parse_from_rfc3339(&entry.timestamp)
            .map(|timestamp| timestamp.with_timezone(&Utc) >= cutoff)
            .unwrap_or(true)
    });
    logs.truncate(MAX_REQUEST_LOGS);
}

pub async fn create_router(
    storage: Arc<Storage>,
    geo_profile: SharedGeoProfile,
    proxy_manager: Arc<ProxyManager>,
    allow_lan: bool,
) -> Router {
    let (tx, _rx) = broadcast::channel(100);
    let log_buffer: Arc<RwLock<Vec<LogEntry>>> = Arc::new(RwLock::new(Vec::new()));
    let (request_log_tx, mut request_log_rx) = mpsc::channel::<RequestLogEntry>(1024);

    // Keep the in-memory log bounded and fan each browser request out to the
    // dashboard over the existing WebSocket connection.
    let log_buffer_task = log_buffer.clone();
    let tx_task = tx.clone();
    tokio::spawn(async move {
        while let Some(entry) = request_log_rx.recv().await {
            {
                let mut logs = log_buffer_task.write().await;
                prune_request_logs(&mut logs);
                logs.insert(0, entry.clone());
                prune_request_logs(&mut logs);
            }
            let _ = tx_task.send(
                serde_json::json!({
                    "type": "request_log",
                    "data": entry,
                })
                .to_string(),
            );
        }
    });

    let instance_manager = Arc::new(RwLock::new(
        InstanceManager::with_geo_profile(geo_profile.clone())
            .with_allow_lan(allow_lan)
            .with_request_log_sender(request_log_tx)
            .with_storage(storage.clone()),
    ));

    let state = AppState {
        tx,
        instance_manager,
        storage,
        log_buffer,
        geo_profile,
        proxy_manager,
    };

    Router::new()
        .route("/", get(serve_index))
        .route("/health", get(health_check))
        .route("/api/status", get(get_status))
        .route("/api/proxy", get(get_proxy_config))
        .route("/api/proxy", post(save_proxy_config))
        .route("/api/proxy/test", post(test_proxy))
        .route("/api/proxy/restart", post(restart_proxy))
        .route("/api/instances/:kind/launch", post(launch_instance))
        .route("/api/instances/:kind/stop", post(stop_instance))
        .route("/api/logs", get(get_logs).delete(clear_logs))
        .route("/api/validation/:kind", get(get_validation_history))
        .route("/api/security/web", get(get_web_security))
        .route("/ws", get(websocket_handler))
        .nest_service("/static", ServeDir::new("web"))
        .with_state(Arc::new(state))
}

include!("status.rs");
include!("proxy.rs");
include!("logs.rs");
include!("security.rs");
include!("instances.rs");
include!("websocket.rs");
