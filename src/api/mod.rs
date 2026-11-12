// API Server module - HTTP REST + WebSocket endpoints
use crate::geoip::{GeoProfile, SharedGeoProfile};
use crate::instance::{InstanceKind, InstanceManager, RequestLogEntry, ValidationStatus};
use crate::proxy::ProxyManager;
use crate::storage::Storage;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::services::ServeDir;

// Global state for broadcasting updates
#[derive(Clone)]
pub struct AppState {
    pub tx: broadcast::Sender<String>,
    pub instance_manager: Arc<RwLock<InstanceManager>>,
    pub log_buffer: Arc<RwLock<Vec<LogEntry>>>,
    pub geo_profile: SharedGeoProfile,
    pub proxy_manager: Arc<ProxyManager>,
}

pub type LogEntry = RequestLogEntry;

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
                logs.insert(0, entry.clone());
                logs.truncate(2_000);
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
            .with_storage(storage),
    ));

    let state = AppState {
        tx,
        instance_manager,
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
    source_ip: Option<String>,
    country_code: Option<String>,
    country: String,
    city: Option<String>,
    timezone: Option<String>,
    language: Option<String>,
    locale: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

fn geo_info_from_profile(profile: &GeoProfile) -> GeoInfo {
    GeoInfo {
        source_ip: profile.source_ip.clone(),
        country_code: profile.country_code.clone(),
        country: profile.country.clone().unwrap_or_default(),
        city: profile.city.clone(),
        timezone: profile.timezone.clone(),
        language: profile.language.clone(),
        locale: profile.locale.clone(),
        latitude: profile.latitude,
        longitude: profile.longitude,
    }
}

#[derive(Serialize)]
struct InstanceStatus {
    kind: String,
    status: String,
    pid: Option<u32>,
    validation: String,
    validation_details: String,
}

#[derive(Serialize)]
struct FirewallStatus {
    installed: bool,
    mode: String,
    status: String,
    anchor: String,
    isolation_group: String,
    last_error: Option<String>,
}

fn validation_name(status: &ValidationStatus) -> &'static str {
    match status {
        ValidationStatus::Pass => "pass",
        ValidationStatus::Fail => "fail",
        ValidationStatus::Unknown => "unknown",
        ValidationStatus::Unsupported => "unsupported",
    }
}

async fn get_status(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<StatusResponse> {
    let manager = state.instance_manager.read().await;
    let status_map = manager.status_all().await;
    let pid_map = manager.pid_all().await;
    let validation_map = manager.validation_all().await;
    let geo_profile = state.geo_profile.read().await.clone();
    let firewall = state.proxy_manager.firewall_status();
    let proxy_healthy = state.proxy_manager.is_healthy() && geo_profile.is_resolved();

    let validation_for = |kind: InstanceKind| {
        validation_map
            .get(&kind)
            .map(|value| {
                (
                    validation_name(&value.status).to_string(),
                    value.details.clone(),
                )
            })
            .unwrap_or_else(|| ("unknown".to_string(), "validation unavailable".to_string()))
    };
    let (code_validation, code_validation_details) = validation_for(InstanceKind::Code);
    let (desktop_validation, desktop_validation_details) = validation_for(InstanceKind::Desktop);
    let (vscode_validation, vscode_validation_details) = validation_for(InstanceKind::VsCode);
    let (web_validation, web_validation_details) = validation_for(InstanceKind::Web);

    let instances = vec![
        InstanceStatus {
            kind: "code".to_string(),
            status: if status_map
                .get(&InstanceKind::Code)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Code).copied().flatten(),
            validation: code_validation,
            validation_details: code_validation_details,
        },
        InstanceStatus {
            kind: "desktop".to_string(),
            status: if status_map
                .get(&InstanceKind::Desktop)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Desktop).copied().flatten(),
            validation: desktop_validation,
            validation_details: desktop_validation_details,
        },
        InstanceStatus {
            kind: "vscode".to_string(),
            status: if status_map
                .get(&InstanceKind::VsCode)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::VsCode).copied().flatten(),
            validation: vscode_validation,
            validation_details: vscode_validation_details,
        },
        InstanceStatus {
            kind: "web".to_string(),
            status: if status_map.get(&InstanceKind::Web).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Web).copied().flatten(),
            validation: web_validation,
            validation_details: web_validation_details,
        },
    ];

    Json(StatusResponse {
        proxy: ProxyStatus {
            healthy: proxy_healthy,
            exit_ip: geo_profile.source_ip.clone(),
            latency_ms: None,
            geo: if proxy_healthy && geo_profile.has_geo() {
                Some(geo_info_from_profile(&geo_profile))
            } else {
                None
            },
        },
        instances,
        firewall: FirewallStatus {
            installed: firewall.installed,
            mode: format!("{:?}", firewall.mode).to_lowercase(),
            status: firewall.status,
            anchor: firewall.anchor,
            isolation_group: firewall.isolation_group,
            last_error: firewall.last_error,
        },
    })
}

async fn restart_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let _ = state.tx.send(r#"{"type":"proxy_restarting"}"#.to_string());

    match state.proxy_manager.restart().await {
        Ok(()) => {
            match state.proxy_manager.refresh_geo_profile().await {
                Ok(_) => state.proxy_manager.mark_healthy(),
                Err(error) => {
                    state.proxy_manager.mark_unhealthy();
                    tracing::warn!("GeoIP refresh after proxy restart failed: {}", error);
                }
            }
            let _ = state.tx.send(r#"{"type":"proxy_restarted"}"#.to_string());
            Json(serde_json::json!({
                "success": true,
                "message": "sing-box 已重启，新配置生效"
            }))
        }
        Err(error) => Json(serde_json::json!({
            "success": false,
            "message": format!("重启失败: {}", error)
        })),
    }
}

async fn get_proxy_config(
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
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
            if let Some((_, val)) = trimmed.split_once('=') {
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
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<SaveProxyRequest>,
) -> Json<serde_json::Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = format!("{}/.cim/config.toml", home);

    // 读取现有配置
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();

    // 更新或添加 [proxy] 下的 host/port/username
    let new_content =
        update_proxy_in_toml(&content, &payload.host, payload.port, &payload.username);

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
        if trimmed == "[proxy]" {
            in_proxy = true;
            continue;
        }
        if trimmed.starts_with('[') && trimmed != "[proxy]" {
            in_proxy = false;
        }

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
        if !host_set {
            lines.push(format!("host = \"{}\"", host));
        }
        if !port_set {
            lines.push(format!("port = {}", port));
        }
        if !user_set {
            if let Some(u) = username {
                lines.push(format!("username = \"{}\"", u));
            }
        }
    }

    lines.join("\n")
}

async fn test_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let start = std::time::Instant::now();

    // 用 curl + socks5，和健康检查保持一致，避免 HTTP 代理走直连
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-x",
            "socks5://127.0.0.1:7891",
            "--max-time",
            "8",
            "https://ipv4.icanhazip.com",
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let latency_ms = start.elapsed().as_millis() as u64;
            if !ip.is_empty() {
                let parsed_ip = match ip.parse::<std::net::IpAddr>() {
                    Ok(parsed_ip) => parsed_ip,
                    Err(error) => {
                        return Json(serde_json::json!({
                            "healthy": false,
                            "error": "PROXY_INVALID_EXIT_IP",
                            "detail": error.to_string()
                        }));
                    }
                };
                state.proxy_manager.mark_healthy();
                let geo = match state.proxy_manager.refresh_geo_profile_for_ip(&ip).await {
                    Ok(profile) => Some(geo_info_from_profile(&profile)),
                    Err(error) => {
                        tracing::warn!("GeoIP resolution failed for {}: {}", parsed_ip, error);
                        None
                    }
                };
                Json(serde_json::json!({
                    "healthy": true,
                    "exit_ip": ip,
                    "latency_ms": latency_ms,
                    "geo": geo
                }))
            } else {
                Json(serde_json::json!({
                    "healthy": false,
                    "error": "PROXY_NO_RESPONSE"
                }))
            }
        }
        Ok(out) => {
            state.proxy_manager.mark_unhealthy();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let reason = if stderr.contains("timed out") || stderr.contains("Operation timed out") {
                "PROXY_TIMEOUT"
            } else if stderr.contains("Authentication") || stderr.contains("407") {
                "PROXY_AUTH_FAILED"
            } else if stderr.contains("Connection refused") {
                "PROXY_NOT_RUNNING"
            } else {
                "PROXY_ERROR"
            };
            Json(serde_json::json!({
                "healthy": false,
                "error": reason,
                "detail": stderr
            }))
        }
        Err(e) => {
            state.proxy_manager.mark_unhealthy();
            Json(serde_json::json!({
                "healthy": false,
                "error": "CURL_NOT_FOUND",
                "detail": e.to_string()
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

async fn clear_logs(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let cleared = {
        let mut logs = state.log_buffer.write().await;
        let count = logs.len();
        logs.clear();
        count
    };

    // Keep every open dashboard in sync with the server-side buffer.
    let _ = state.tx.send(
        serde_json::json!({
            "type": "logs_cleared",
            "count": cleared,
        })
        .to_string(),
    );

    Json(serde_json::json!({
        "success": true,
        "cleared": cleared,
    }))
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

    if !state.proxy_manager.is_healthy() {
        return Json(serde_json::json!({
            "success": false,
            "message": "代理当前不健康，已阻止实例启动"
        }));
    }

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
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": format!("Failed to launch {}: {}", kind, e)
        })),
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
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": format!("Failed to stop {}: {}", kind, e)
        })),
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
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;

    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // Send initial connection message
    let _ = sender
        .send(Message::Text(
            serde_json::json!({
                "type": "connected",
                "message": "WebSocket connected"
            })
            .to_string(),
        ))
        .await;

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
