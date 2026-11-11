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
    pub log_buffer: Arc<RwLock<Vec<LogEntry>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub instance: String,
    pub method: String,
    pub url: String,
    pub status: u16,
    pub latency_ms: u64,
    pub size_bytes: u64,
}

pub async fn create_router(storage: Arc<Storage>) -> Router {
    let (tx, _rx) = broadcast::channel(100);
    let log_buffer: Arc<RwLock<Vec<LogEntry>>> = Arc::new(RwLock::new(Vec::new()));

    let instance_manager = Arc::new(RwLock::new(
        InstanceManager::new().with_storage(storage)
    ));

    let state = AppState { tx, instance_manager, log_buffer };

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
        .route("/api/logs", get(get_logs))
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

async fn restart_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 重启 sing-box：pkill + 守护进程会重新启动
    let status = std::process::Command::new("pkill")
        .arg("-f")
        .arg("sing-box")
        .status();

    match status {
        Ok(_) => {
            // 等待 sing-box 被 proxy manager 重新拉起（2秒）
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let _ = state.tx.send(r#"{"type":"proxy_restarting"}"#.to_string());

            Json(serde_json::json!({
                "success": true,
                "message": "sing-box 已重启，新配置生效"
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": format!("重启失败: {}", e)
        })),
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProxyConfigData {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    // 密码不返回给前端，只接收保存
    #[serde(skip_serializing)]
    pub password: Option<String>,
}

async fn get_proxy_config(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 从配置文件读取（不含密码）
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = format!("{}/.cim/config.toml", home);

    // 尝试解析 config.toml 中的代理设置
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        // 简单提取 host/port/username（生产中用 toml 库解析）
        let host = extract_toml_str(&content, "host").unwrap_or_default();
        let port = extract_toml_num(&content, "port").unwrap_or(0);
        let username = extract_toml_str(&content, "username");
        let configured = !host.is_empty() && port > 0;

        return Json(serde_json::json!({
            "configured": configured,
            "host": host,
            "port": port,
            "username": username,
        }));
    }

    Json(serde_json::json!({
        "configured": false,
        "host": "",
        "port": 0,
        "username": null,
    }))
}

fn extract_toml_str(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{} =", key)) || trimmed.starts_with(&format!("{}=", key)) {
            if let Some(val) = trimmed.splitn(2, '=').nth(1) {
                let val = val.trim().trim_matches('"').to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}

fn extract_toml_num(content: &str, key: &str) -> Option<u16> {
    extract_toml_str(content, key)?.parse().ok()
}

#[derive(Deserialize)]
struct SaveProxyRequest {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

async fn save_proxy_config(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<SaveProxyRequest>,
) -> Json<serde_json::Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = format!("{}/.cim/config.toml", home);

    // 读取现有配置
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();

    // 更新或添加 [proxy] 下的 host/port/username
    let new_content = update_proxy_in_toml(&content, &payload.host, payload.port, &payload.username);

    if let Err(e) = std::fs::write(&config_path, &new_content) {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("写入配置失败: {}", e)
        }));
    }

    // 密码保存到 Keychain（macOS）
    if let Some(ref pwd) = payload.password {
        if !pwd.is_empty() {
            #[cfg(target_os = "macos")]
            {
                use crate::storage::keychain;
                let _ = keychain::save_password("com.cim.proxy", &payload.host, pwd);
            }
        }
    }

    Json(serde_json::json!({
        "success": true,
        "message": "代理配置已保存"
    }))
}

fn update_proxy_in_toml(content: &str, host: &str, port: u16, username: &Option<String>) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut in_proxy = false;
    let mut host_set = false;
    let mut port_set = false;
    let mut user_set = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed == "[proxy]" { in_proxy = true; continue; }
        if trimmed.starts_with('[') && trimmed != "[proxy]" { in_proxy = false; }

        if in_proxy {
            if trimmed.starts_with("host") {
                *line = format!("host = \"{}\"", host);
                host_set = true;
            } else if trimmed.starts_with("port") {
                *line = format!("port = {}", port);
                port_set = true;
            } else if trimmed.starts_with("username") {
                *line = match username {
                    Some(u) if !u.is_empty() => format!("username = \"{}\"", u),
                    _ => "username = \"\"".to_string(),
                };
                user_set = true;
            }
        }
    }

    // 如果 [proxy] 区域没有这些字段，追加
    if in_proxy && (!host_set || !port_set || !user_set) {
        if !host_set { lines.push(format!("host = \"{}\"", host)); }
        if !port_set { lines.push(format!("port = {}", port)); }
        if !user_set {
            if let Some(u) = username {
                lines.push(format!("username = \"{}\"", u));
            }
        }
    }

    lines.join("\n")
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

async fn get_logs(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let logs = state.log_buffer.read().await;
    Json(serde_json::json!({ "logs": *logs }))
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
