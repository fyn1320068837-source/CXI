// API Server module - HTTP REST + WebSocket endpoints
use axum::{
    Router,
    routing::{get, post},
    response::{Html, IntoResponse},
    Json,
    extract::ws::{WebSocket, WebSocketUpgrade},
};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use crate::instance::{InstanceManager, InstanceKind};
use crate::storage::Storage;

// Global state for broadcasting updates
#[derive(Clone)]
pub struct AppState {
    pub tx: broadcast::Sender<String>,
    pub instance_manager: Arc<RwLock<InstanceManager>>,
}

pub async fn create_router(storage: Arc<Storage>) -> Router {
    // Create broadcast channel for WebSocket updates
    let (tx, _rx) = broadcast::channel(100);

    // Create instance manager with storage
    let instance_manager = Arc::new(RwLock::new(
        InstanceManager::new().with_storage(storage)
    ));

    let state = AppState { tx, instance_manager };

    Router::new()
        .route("/", get(serve_index))
        .route("/health", get(health_check))
        .route("/api/status", get(get_status))
        .route("/api/proxy/test", post(test_proxy))
        .route("/api/instances/:kind/launch", post(launch_instance))
        .route("/api/instances/:kind/stop", post(stop_instance))
        .route("/ws", get(websocket_handler))
        .nest_service("/static", ServeDir::new("web"))
        .with_state(Arc::new(state))
}

async fn serve_index() -> impl IntoResponse {
    let html = include_str!("../../web/index.html");
    Html(html)
}

async fn health_check() -> &'static str {
    "OK"
}

#[derive(Serialize)]
struct StatusResponse {
    proxy: ProxyStatus,
    instances: Vec<InstanceStatus>,
    firewall: FirewallStatus,
}

#[derive(Serialize)]
struct ProxyStatus {
    healthy: bool,
    exit_ip: Option<String>,
    latency_ms: Option<u64>,
    geo: Option<GeoInfo>,
}

#[derive(Serialize)]
struct GeoInfo {
    country: String,
    city: Option<String>,
}

#[derive(Serialize)]
struct InstanceStatus {
    kind: String,
    status: String,
    validation: String,
}

#[derive(Serialize)]
struct FirewallStatus {
    installed: bool,
    status: String,
}

async fn get_status(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<StatusResponse> {
    let manager = state.instance_manager.read().await;
    let status_map = manager.status_all().await;

    let instances = vec![
        InstanceStatus {
            kind: "code".to_string(),
            status: if status_map.get(&InstanceKind::Code).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            validation: "unknown".to_string(),
        },
        InstanceStatus {
            kind: "desktop".to_string(),
            status: if status_map.get(&InstanceKind::Desktop).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            validation: "unsupported".to_string(),
        },
        InstanceStatus {
            kind: "vscode".to_string(),
            status: if status_map.get(&InstanceKind::VsCode).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            validation: "unknown".to_string(),
        },
        InstanceStatus {
            kind: "web".to_string(),
            status: if status_map.get(&InstanceKind::Web).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            validation: "unknown".to_string(),
        },
    ];

    Json(StatusResponse {
        proxy: ProxyStatus {
            healthy: true,
            exit_ip: Some("68.170.3.93".to_string()),
            latency_ms: Some(150),
            geo: Some(GeoInfo {
                country: "US".to_string(),
                city: Some("New York".to_string()),
            }),
        },
        instances,
        firewall: FirewallStatus {
            installed: false,
            status: "软隔离模式".to_string(),
        },
    })
}

#[derive(Deserialize)]
struct ProxyTestRequest {
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
}

async fn test_proxy(
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
    body: Option<Json<ProxyTestRequest>>,
) -> Json<serde_json::Value> {
    // 实际探测：经 127.0.0.1:7890 请求出口 IP
    let client = match reqwest::Client::builder()
        .proxy(reqwest::Proxy::http("http://127.0.0.1:7890").unwrap())
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Json(serde_json::json!({
            "healthy": false,
            "error": format!("Failed to build client: {}", e)
        })),
    };

    let start = std::time::Instant::now();
    match client.get("https://api.ipify.org").send().await {
        Ok(resp) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            match resp.text().await {
                Ok(ip) => {
                    let ip = ip.trim().to_string();
                    Json(serde_json::json!({
                        "healthy": true,
                        "exit_ip": ip,
                        "latency_ms": latency_ms,
                        "geo": null
                    }))
                }
                Err(e) => Json(serde_json::json!({
                    "healthy": false,
                    "error": format!("Failed to read response: {}", e)
                })),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let reason = if msg.contains("timed out") {
                "PROXY_TIMEOUT"
            } else if msg.contains("auth") || msg.contains("407") {
                "PROXY_AUTH_FAILED"
            } else if msg.contains("connection refused") {
                "PROXY_NOT_RUNNING"
            } else {
                "PROXY_ERROR"
            };
            Json(serde_json::json!({
                "healthy": false,
                "error": reason,
                "detail": msg
            }))
        }
    }
}

async fn launch_instance(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let instance_kind = match kind.as_str() {
        "code" => InstanceKind::Code,
        "desktop" => InstanceKind::Desktop,
        "vscode" => InstanceKind::VsCode,
        "web" => InstanceKind::Web,
        _ => {
            return Json(serde_json::json!({
                "success": false,
                "message": format!("Unknown instance kind: {}", kind)
            }));
        }
    };

    let manager = state.instance_manager.read().await;
    match manager.launch(instance_kind).await {
        Ok(_) => {
            // Broadcast update
            let _ = state.tx.send(format!("Instance {} launched", kind));

            Json(serde_json::json!({
                "success": true,
                "message": format!("{} instance launched successfully", kind)
            }))
        }
        Err(e) => {
            Json(serde_json::json!({
                "success": false,
                "message": format!("Failed to launch {}: {}", kind, e)
            }))
        }
    }
}

async fn stop_instance(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let instance_kind = match kind.as_str() {
        "code" => InstanceKind::Code,
        "desktop" => InstanceKind::Desktop,
        "vscode" => InstanceKind::VsCode,
        "web" => InstanceKind::Web,
        _ => {
            return Json(serde_json::json!({
                "success": false,
                "message": format!("Unknown instance kind: {}", kind)
            }));
        }
    };

    let manager = state.instance_manager.read().await;
    match manager.stop(instance_kind).await {
        Ok(_) => {
            // Broadcast update
            let _ = state.tx.send(format!("Instance {} stopped", kind));

            Json(serde_json::json!({
                "success": true,
                "message": format!("{} instance stopped successfully", kind)
            }))
        }
        Err(e) => {
            Json(serde_json::json!({
                "success": false,
                "message": format!("Failed to stop {}: {}", kind, e)
            }))
        }
    }
}

// WebSocket handler for real-time updates
async fn websocket_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket_connection(socket, state))
}

async fn websocket_connection(socket: WebSocket, state: Arc<AppState>) {
    use axum::extract::ws::Message;
    use futures::stream::StreamExt;
    use futures::sink::SinkExt;

    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // Send initial connection message
    let _ = sender.send(Message::Text(
        serde_json::json!({
            "type": "connected",
            "message": "WebSocket connected"
        }).to_string()
    )).await;

    // Spawn task to send broadcasts to this client
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Spawn task to receive messages from client
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            // Handle incoming messages (ping/pong, etc.)
            if let Message::Close(_) = msg {
                break;
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
}
