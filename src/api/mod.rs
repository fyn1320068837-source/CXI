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
    geo_ready: bool,
    geo_status: String,
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
    validation_checked_at: Option<String>,
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
    // Tunnel health and GeoIP readiness are separate signals. The proxy can
    // carry traffic while the GeoIP provider is temporarily unavailable; Web
    // launches are gated separately by InstanceManager until the profile is
    // ready, so a transient GeoIP outage never causes a host-locale fallback.
    let proxy_healthy = state.proxy_manager.is_healthy();
    let geo_ready = geo_profile.is_ready();
    let geo_status = if geo_ready {
        "ready"
    } else if geo_profile.source_ip.is_some() {
        "pending"
    } else {
        "unavailable"
    };

    let validation_for = |kind: InstanceKind| {
        validation_map
            .get(&kind)
            .map(|value| {
                (
                    validation_name(&value.status).to_string(),
                    value.details.clone(),
                    value.checked_at.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    "unknown".to_string(),
                    "validation unavailable".to_string(),
                    None,
                )
            })
    };
    let (code_validation, code_validation_details, code_checked_at) =
        validation_for(InstanceKind::Code);
    let (desktop_validation, desktop_validation_details, desktop_checked_at) =
        validation_for(InstanceKind::Desktop);
    let (vscode_validation, vscode_validation_details, vscode_checked_at) =
        validation_for(InstanceKind::VsCode);
    let (web_validation, web_validation_details, web_checked_at) =
        validation_for(InstanceKind::Web);

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
            validation_checked_at: code_checked_at,
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
            validation_checked_at: desktop_checked_at,
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
            validation_checked_at: vscode_checked_at,
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
            validation_checked_at: web_checked_at,
        },
    ];

    Json(StatusResponse {
        proxy: ProxyStatus {
            healthy: proxy_healthy,
            geo_ready,
            geo_status: geo_status.to_string(),
            exit_ip: geo_profile.source_ip.clone(),
            latency_ms: None,
            geo: if geo_profile.has_geo() {
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
                    tracing::warn!("GeoIP refresh after proxy restart failed: {}", error);
                    match state.proxy_manager.check_exit_ip().await {
                        Ok(_) => state.proxy_manager.mark_healthy(),
                        Err(proxy_error) => {
                            state.proxy_manager.mark_unhealthy();
                            tracing::warn!(
                                "Proxy health check after restart failed: {}",
                                proxy_error
                            );
                        }
                    }
                }
            }
            if state.proxy_manager.is_healthy() {
                let _ = state.tx.send(r#"{"type":"proxy_restarted"}"#.to_string());
                Json(serde_json::json!({
                    "success": true,
                    "message": "sing-box 已重启，新配置生效"
                }))
            } else {
                Json(serde_json::json!({
                    "success": false,
                    "message": "sing-box 已启动，但 fail-closed 尚未解除"
                }))
            }
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
                if !state.proxy_manager.is_healthy() {
                    return Json(serde_json::json!({
                        "healthy": false,
                        "error": "FAIL_CLOSED_NOT_READY",
                        "detail": "PF fail-closed 未安装或未处于放行态"
                    }));
                }
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

#[derive(Debug, Deserialize)]
struct LogQuery {
    limit: Option<usize>,
}

async fn get_logs(
    Query(query): Query<LogQuery>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let limit = query
        .limit
        .unwrap_or(MAX_REQUEST_LOGS)
        .min(MAX_REQUEST_LOGS);
    let mut logs = state.log_buffer.write().await;
    prune_request_logs(&mut logs);
    let entries = logs.iter().take(limit).cloned().collect::<Vec<_>>();
    Json(serde_json::json!({
        "logs": entries,
        "limit": limit,
        "retention_minutes": REQUEST_LOG_RETENTION.num_minutes(),
    }))
}

async fn clear_logs(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let cleared = {
        let mut logs = state.log_buffer.write().await;
        prune_request_logs(&mut logs);
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

async fn get_validation_history(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let allowed = matches!(kind.as_str(), "code" | "desktop" | "vscode" | "web");
    if !allowed {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("Unknown instance kind: {}", kind),
        }));
    }

    match state.storage.recent_validation_evidence(&kind, 50) {
        Ok(evidence) => Json(serde_json::json!({
            "success": true,
            "instance": kind,
            "evidence": evidence,
        })),
        Err(error) => Json(serde_json::json!({
            "success": false,
            "instance": kind,
            "error": error.to_string(),
        })),
    }
}

const SECURITY_EVIDENCE_TTL_SECONDS: i64 = 90;
const SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS: i64 = 30;

fn parse_detail_count(details: &str, key: &str) -> usize {
    let Some(value) = details
        .split_once(&format!("{key}="))
        .map(|(_, value)| value)
    else {
        return 0;
    };
    value
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn parse_checked_pids(details: &str) -> usize {
    details
        .split_once("checked ")
        .and_then(|(_, value)| value.split_once(" pids"))
        .and_then(|(value, _)| value.parse().ok())
        .unwrap_or(0)
}

fn direct_endpoints(details: &str) -> Vec<String> {
    let Some((_, value)) = details.split_once("direct=") else {
        return Vec::new();
    };
    if value.trim_start().starts_with('0') {
        return Vec::new();
    }
    value
        .split(", ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

async fn get_web_security(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let manager = state.instance_manager.read().await;
    let status_map = manager.status_all().await;
    let pid_map = manager.pid_all().await;
    let validation_map = manager.validation_all().await;
    let geo_profile = state.geo_profile.read().await.clone();
    let firewall = state.proxy_manager.firewall_status();
    let proxy_healthy = state.proxy_manager.is_healthy();
    let geo_ready = geo_profile.is_ready();
    let web_running = status_map.get(&InstanceKind::Web).copied().unwrap_or(false);
    let web_pid = pid_map.get(&InstanceKind::Web).copied().flatten();
    let validation = validation_map.get(&InstanceKind::Web);
    let validation_name = validation
        .map(|value| validation_name(&value.status))
        .unwrap_or("unknown");
    let validation_checked_at = validation.and_then(|value| value.checked_at.clone());
    let evidence = state
        .storage
        .recent_validation_evidence("web", 1)
        .ok()
        .and_then(|values| values.into_iter().next());
    let details = evidence
        .as_ref()
        .and_then(|value| value.details.clone())
        .unwrap_or_default();
    let checked_at = evidence
        .as_ref()
        .map(|value| value.timestamp.clone())
        .or(validation_checked_at.clone());
    let age_seconds = checked_at.as_deref().and_then(|timestamp| {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(|value| {
                (Utc::now() - value.with_timezone(&Utc))
                    .num_seconds()
                    .max(0)
            })
    });
    let evidence_matches_validation = match (
        evidence.as_ref().map(|value| value.timestamp.as_str()),
        validation_checked_at.as_deref(),
    ) {
        (Some(evidence_timestamp), Some(validation_timestamp)) => {
            match (
                chrono::DateTime::parse_from_rfc3339(evidence_timestamp),
                chrono::DateTime::parse_from_rfc3339(validation_timestamp),
            ) {
                (Ok(evidence_time), Ok(validation_time)) => {
                    (validation_time - evidence_time).num_seconds().abs()
                        <= SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS
                }
                _ => false,
            }
        }
        _ => false,
    };
    let evidence_fresh = age_seconds
        .map(|age| age <= SECURITY_EVIDENCE_TTL_SECONDS)
        .unwrap_or(false)
        && evidence_matches_validation;
    // Only expose counters from a fresh snapshot. A stopped/restarted Web
    // instance must not inherit the previous process tree's evidence.
    let current_evidence = evidence.as_ref().filter(|_| evidence_fresh);
    let proxied = current_evidence
        .map(|value| value.proxied_count)
        .or_else(|| {
            if evidence_fresh {
                Some(parse_detail_count(&details, "proxied"))
            } else {
                None
            }
        });
    let local = current_evidence.map(|value| value.local_count).or_else(|| {
        if evidence_fresh {
            Some(parse_detail_count(&details, "local"))
        } else {
            None
        }
    });
    let direct = current_evidence
        .map(|value| value.direct_count)
        .or_else(|| {
            if evidence_fresh {
                Some(parse_detail_count(&details, "direct"))
            } else {
                None
            }
        });
    let checked_pids = if evidence_fresh {
        parse_checked_pids(&details)
    } else {
        0
    };
    let direct_items = if evidence_fresh {
        direct_endpoints(&details)
    } else {
        Vec::new()
    };
    let has_direct = direct.map(|value| value > 0).unwrap_or(false);
    let has_proxy = proxied.map(|value| value > 0).unwrap_or(false);
    let firewall_mode = format!("{:?}", firewall.mode).to_lowercase();

    let mut reasons = Vec::new();
    let verdict = if has_direct || validation_name == "fail" {
        reasons.push("检测到公网直连或验证失败".to_string());
        "FAIL"
    } else if !web_running {
        reasons.push("Web 实例未运行，没有新鲜连接样本".to_string());
        "UNKNOWN"
    } else if !proxy_healthy && firewall_mode != "blocked" {
        reasons.push("代理异常但 PF 未处于阻断态".to_string());
        "FAIL"
    } else if !proxy_healthy && firewall_mode == "blocked" {
        reasons.push("代理异常，PF 已阻断实例直连".to_string());
        "BLOCKED"
    } else if !geo_ready || !evidence_fresh || validation_name != "pass" || !has_proxy {
        if !geo_ready {
            reasons.push("GeoIP 位置资料未就绪".to_string());
        }
        if !evidence_fresh {
            reasons.push("网络验证证据不存在或已过期".to_string());
        }
        if !has_proxy {
            reasons.push(if proxied.is_some() {
                "没有观测到代理连接".to_string()
            } else {
                "没有新鲜代理连接样本".to_string()
            });
        }
        "UNKNOWN"
    } else {
        "PASS"
    };
    let label = match verdict {
        "PASS" => "安全 · 进程级观测通过",
        "FAIL" => "风险 · 发现直连或验证失败",
        "BLOCKED" => "已阻断 · 代理故障期间 PF 生效",
        _ => "待确认 · 证据不足或已过期",
    };

    Json(serde_json::json!({
        "success": true,
        "verdict": verdict,
        "label": label,
        "checked_at": checked_at,
        "age_seconds": age_seconds,
        "evidence_ttl_seconds": SECURITY_EVIDENCE_TTL_SECONDS,
        "proxy": {
            "healthy": proxy_healthy,
            "geo_ready": geo_ready,
            "geo_status": if geo_ready { "ready" } else if geo_profile.source_ip.is_some() { "pending" } else { "unavailable" },
            "exit_ip": geo_profile.source_ip,
        },
        "firewall": {
            "installed": firewall.installed,
            "mode": firewall_mode,
        },
        "web": {
            "running": web_running,
            "pid": web_pid,
            "validation": validation_name,
            "validation_checked_at": validation_checked_at,
            "checked_pids": checked_pids,
            "proxied": proxied,
            "local": local,
            "direct": direct,
            "direct_endpoints": direct_items,
        },
        "checks": {
            "proxy_healthy": proxy_healthy,
            "geo_ready": geo_ready,
            "validation_pass": validation_name == "pass" && evidence_fresh,
            "direct_zero": direct == Some(0),
            "udp53": "not_sampled",
            "webrtc_public_candidates": "not_sampled",
            "ipv6_direct": "not_sampled",
        },
        "reasons": reasons,
        "limitations": [
            "结论基于 Web 进程树 lsof 观测，不是全局抓包保证",
            "UDP 53、WebRTC 公网候选和 IPv6-only 需要单独主动测试",
            "请求 URL 日志只能说明访问目标，不能作为直连判定依据",
        ],
    }))
}

async fn launch_instance(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let Some(instance_kind) = InstanceKind::parse(&kind) else {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("Unknown instance kind: {}", kind)
        }));
    };

    if !state.proxy_manager.is_healthy() {
        return Json(serde_json::json!({
            "success": false,
            "message": "代理当前不健康，已阻止实例启动"
        }));
    }
    let firewall = state.proxy_manager.firewall_status();
    if !firewall.installed {
        return Json(serde_json::json!({
            "success": false,
            "message": "PF fail-closed 未安装，已阻止实例启动"
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
    let Some(instance_kind) = InstanceKind::parse(&kind) else {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("Unknown instance kind: {}", kind)
        }));
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
