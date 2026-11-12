// Claude Web in Chromium adapter (Instance D)
use super::{
    apply_locale_env, descendant_pids, isolated_command, CapabilityReport, InstanceAdapter,
    ProcessHandle, RequestLogEntry, RequestLogSender,
};
use crate::geoip::{GeoProfile, SharedGeoProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type BrowserSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct WebAdapter {
    user_data_dir: String,
    geo_profile: SharedGeoProfile,
    request_log_tx: Option<RequestLogSender>,
    sessions: Arc<Mutex<HashMap<u32, String>>>,
}

impl WebAdapter {
    pub fn new_with_geo_profile(user_data_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self {
            user_data_dir,
            geo_profile,
            request_log_tx: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set_request_log_sender(&mut self, sender: RequestLogSender) {
        self.request_log_tx = Some(sender);
    }
}

#[async_trait]
impl InstanceAdapter for WebAdapter {
    async fn detect(&self) -> CapabilityReport {
        // Try to find Chromium or Chrome
        let browsers = vec![
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ];

        for path in browsers {
            if std::path::Path::new(path).exists() {
                return CapabilityReport {
                    installed: true,
                    version: None,
                    path: Some(path.to_string()),
                };
            }
        }

        CapabilityReport {
            installed: false,
            version: None,
            path: None,
        }
    }

    async fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.user_data_dir)?;
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        let report = self.detect().await;
        let browser_path = report
            .path
            .ok_or_else(|| anyhow::anyhow!("No browser found"))?;
        let profile = self.geo_profile.read().await.clone();
        let debug_port = find_free_port()?;
        let session_dir = format!("{}/sessions/{}", self.user_data_dir, Uuid::new_v4());
        std::fs::create_dir_all(&session_dir)?;

        let mut args = vec![
            format!("--user-data-dir={}", session_dir),
            "--proxy-server=socks5://127.0.0.1:7891".to_string(),
            "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".to_string(),
            "--force-webrtc-ip-handling-policy=default_public_interface_only".to_string(),
            "--disable-background-networking".to_string(),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            format!("--remote-debugging-port={}", debug_port),
            "--remote-allow-origins=*".to_string(),
        ];
        if let Some(language) = profile.language.as_deref() {
            args.push(format!("--lang={}", language));
        }
        // Do not navigate before DevTools has installed the locale, timezone,
        // geolocation and Accept-Language overrides. Otherwise the first
        // document is fetched with the host locale and only a later refresh
        // reflects the proxy's GeoIP profile.
        args.push(if profile.is_resolved() {
            "about:blank".to_string()
        } else {
            "https://claude.ai".to_string()
        });

        let mut command = isolated_command(&browser_path);
        command.args(&args);
        apply_locale_env(&mut command, &profile);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&session_dir);
                return Err(error.into());
            }
        };
        let root_pid = child.id();

        if profile.is_resolved() {
            let capture_requests = self.request_log_tx.is_some();
            let browser_session =
                match apply_browser_profile(debug_port, &profile, capture_requests).await {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::error!(
                            port = debug_port,
                            error = %error,
                            chain = %format!("{:#}", error),
                            "Failed to apply browser locale/timezone profile"
                        );
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(root_pid as i32),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                        let _ = std::fs::remove_dir_all(&session_dir);
                        return Err(
                            error.context("Failed to apply browser locale/timezone profile")
                        );
                    }
                };

            let initial_target_id = browser_session.target_id.clone();

            // Keep the same CDP connection that installed the overrides. A
            // second debugger client can race the profile connection on some
            // Chrome builds, so request events are consumed from this socket
            // after the first navigation instead of attaching another client.
            if let Some(sender) = self.request_log_tx.clone() {
                tokio::spawn(monitor_browser_socket(
                    browser_session.socket,
                    root_pid,
                    sender.clone(),
                ));
            }
            tokio::spawn(monitor_browser_targets(
                debug_port,
                root_pid,
                profile,
                initial_target_id,
                self.request_log_tx.clone(),
            ));
        } else if let Some(sender) = self.request_log_tx.clone() {
            // Without GeoIP data Chrome starts directly on Claude, so the
            // monitor attaches independently because there is no profile
            // connection to share.
            tokio::spawn(monitor_browser_requests(debug_port, root_pid, sender));
        }

        self.sessions.lock().unwrap().insert(root_pid, session_dir);

        Ok(ProcessHandle { root_pid })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let pid = Pid::from_raw(handle.root_pid as i32);
        let _ = kill(pid, Signal::SIGTERM);

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        let _ = kill(pid, Signal::SIGKILL);
        if let Some(session_dir) = self.sessions.lock().unwrap().remove(&handle.root_pid) {
            let _ = std::fs::remove_dir_all(session_dir);
        }
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        descendant_pids(handle.root_pid)
    }
}

#[derive(Debug, Deserialize, Clone)]
struct DevToolsTarget {
    #[serde(rename = "id")]
    target_id: Option<String>,
    #[serde(rename = "type")]
    target_type: Option<String>,
    url: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
}

struct BrowserProfileSession {
    socket: BrowserSocket,
    target_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DevToolsVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
    #[serde(rename = "User-Agent")]
    user_agent: Option<String>,
}

fn find_free_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("Failed to reserve a Chrome DevTools port")?;
    Ok(listener.local_addr()?.port())
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn apply_browser_profile(
    port: u16,
    profile: &GeoProfile,
    capture_requests: bool,
) -> Result<BrowserProfileSession> {
    tracing::debug!(port, "Applying browser locale/timezone profile");
    // Chrome DevTools is a loopback endpoint. Do not let HTTP(S)_PROXY from
    // the host redirect these control requests through another proxy.
    let client = reqwest::Client::builder().no_proxy().build()?;
    let endpoint = format!("http://127.0.0.1:{}/json/list", port);
    let target = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(response) = client.get(&endpoint).send().await {
                if let Ok(targets) = response.json::<Vec<DevToolsTarget>>().await {
                    let page_target = targets.iter().find(|target| {
                        target.target_type.as_deref() == Some("page")
                            && target.websocket_url.is_some()
                            && target
                                .url
                                .as_deref()
                                .map(|url| url.starts_with("https://claude.ai"))
                                .unwrap_or(false)
                    });
                    if let Some(target) = page_target {
                        break target.clone();
                    }
                    // The initial target is about:blank. Configure it before
                    // the first real navigation so no request leaks the host
                    // language or timezone.
                    if let Some(target) = targets.into_iter().find(|target| {
                        target.target_type.as_deref() == Some("page")
                            && target.websocket_url.is_some()
                    }) {
                        break target;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("Timed out waiting for Chrome DevTools")?;

    let target_id = target.target_id.clone();
    let websocket_url = target
        .websocket_url
        .clone()
        .context("Chrome did not expose a page DevTools endpoint")?;

    let version_endpoint = format!("http://127.0.0.1:{}/json/version", port);
    let version = match client.get(version_endpoint).send().await {
        Ok(response) => response.json::<DevToolsVersion>().await.ok(),
        Err(_) => None,
    };

    // Grant geolocation permission for the isolated Claude origin when the
    // GeoIP provider returned coordinates. The page-level override below then
    // supplies the coordinates without changing the host's real location.
    if profile.latitude.is_some() && profile.longitude.is_some() {
        if let Some(browser_ws) = version
            .as_ref()
            .and_then(|value| value.websocket_url.clone())
        {
            if let Ok((mut browser_socket, _)) = connect_async(browser_ws).await {
                let _ = send_cdp_command(
                    &mut browser_socket,
                    1,
                    "Browser.grantPermissions",
                    json!({
                        "origin": "https://claude.ai",
                        "permissions": ["geolocation"]
                    }),
                )
                .await;
            }
        }
    }

    let (mut socket, _) = connect_async(websocket_url)
        .await
        .context("Failed to connect to Chrome DevTools")?;

    let mut command_id = 1u64;

    if capture_requests {
        send_cdp_command(&mut socket, command_id, "Network.enable", json!({})).await?;
        command_id += 1;
    }

    if let Some(language) = profile.language.as_deref() {
        let language_script = language_override_script(language);
        send_cdp_command(
            &mut socket,
            command_id,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": language_script}),
        )
        .await?;
        command_id += 1;
    }

    if let (Some(language), Some(user_agent)) = (
        profile.language.as_deref(),
        version
            .as_ref()
            .and_then(|value| value.user_agent.as_deref()),
    ) {
        send_cdp_command(
            &mut socket,
            command_id,
            "Network.setUserAgentOverride",
            json!({"userAgent": user_agent, "acceptLanguage": language}),
        )
        .await?;
        command_id += 1;
    }

    if let Some(timezone) = profile.timezone.as_deref() {
        send_cdp_command(
            &mut socket,
            command_id,
            "Emulation.setTimezoneOverride",
            json!({"timezoneId": timezone}),
        )
        .await?;
        command_id += 1;
    }
    if let Some(language) = profile.language.as_deref() {
        send_cdp_command(
            &mut socket,
            command_id,
            "Emulation.setLocaleOverride",
            json!({"locale": language}),
        )
        .await?;
        command_id += 1;
    }
    if let (Some(latitude), Some(longitude)) = (profile.latitude, profile.longitude) {
        send_cdp_command(
            &mut socket,
            command_id,
            "Emulation.setGeolocationOverride",
            json!({"latitude": latitude, "longitude": longitude, "accuracy": 100.0}),
        )
        .await?;
        command_id += 1;
    }

    send_cdp_command(
        &mut socket,
        command_id,
        "Page.navigate",
        json!({"url": "https://claude.ai"}),
    )
    .await?;

    tracing::debug!(port, "Browser locale/timezone profile applied");
    Ok(BrowserProfileSession { socket, target_id })
}

#[derive(Debug)]
struct PendingRequest {
    started_at: Instant,
    method: String,
    url: String,
    status: u16,
}

/// Observe Chromium's Network domain and publish completed requests to the
/// dashboard. The monitor is tied to the browser root PID and exits when the
/// isolated browser is closed.
async fn monitor_browser_requests(port: u16, root_pid: u32, sender: RequestLogSender) {
    // The DevTools discovery endpoint is local to this browser session and
    // must bypass any inherited HTTP proxy environment variables.
    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(port, "Could not create local DevTools client: {}", error);
            return;
        }
    };
    let endpoint = format!("http://127.0.0.1:{}/json/list", port);
    let deadline = Instant::now() + Duration::from_secs(15);
    let target = loop {
        if !process_exists(root_pid) || Instant::now() >= deadline {
            return;
        }
        if let Ok(response) = client.get(&endpoint).send().await {
            if let Ok(targets) = response.json::<Vec<DevToolsTarget>>().await {
                if let Some(target) = targets.into_iter().find(|target| {
                    target.target_type.as_deref() == Some("page") && target.websocket_url.is_some()
                }) {
                    break target;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let Some(websocket_url) = target.websocket_url else {
        return;
    };
    let Ok((mut socket, _)) = connect_async(websocket_url).await else {
        tracing::debug!(port, "Could not attach browser request monitor");
        return;
    };

    if send_cdp_command(&mut socket, 1, "Network.enable", json!({}))
        .await
        .is_err()
    {
        return;
    }
    monitor_browser_socket(socket, root_pid, sender).await;
}

/// Keep page-scoped CDP overrides in sync with new tabs and popup targets.
/// Chrome applies `Emulation.*` and `Network.setUserAgentOverride` to a single
/// target, so the initial page connection is not enough when the user opens a
/// second page in the same isolated browser window.
async fn monitor_browser_targets(
    port: u16,
    root_pid: u32,
    profile: GeoProfile,
    initial_target_id: Option<String>,
    sender: Option<RequestLogSender>,
) {
    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(port, "Could not create local DevTools client: {}", error);
            return;
        }
    };
    let version_endpoint = format!("http://127.0.0.1:{}/json/version", port);
    let deadline = Instant::now() + Duration::from_secs(15);
    let version = loop {
        if !process_exists(root_pid) || Instant::now() >= deadline {
            return;
        }
        if let Ok(response) = client.get(&version_endpoint).send().await {
            if let Ok(version) = response.json::<DevToolsVersion>().await {
                break version;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let Some(browser_ws) = version.websocket_url else {
        tracing::debug!(port, "Chrome did not expose a browser DevTools endpoint");
        return;
    };
    let Ok((mut socket, _)) = connect_async(browser_ws).await else {
        tracing::debug!(port, "Could not attach Chrome target manager");
        return;
    };

    let mut next_command_id = 1u64;
    let auto_attach_id = next_command_id;
    if send_target_command(
        &mut socket,
        &mut next_command_id,
        None,
        "Target.setAutoAttach",
        json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true
        }),
    )
    .await
    .is_err()
    {
        return;
    }

    let mut pending = HashMap::<(String, String), PendingRequest>::new();
    loop {
        if !process_exists(root_pid) {
            break;
        }

        let message = match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => continue,
        };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };

        if value.get("id").and_then(|value| value.as_u64()) == Some(auto_attach_id) {
            if let Some(error) = value.get("error") {
                tracing::debug!(port, "Chrome target auto-attach failed: {}", error);
                break;
            }
            continue;
        }

        let Some(method) = value.get("method").and_then(|value| value.as_str()) else {
            continue;
        };
        if method == "Target.attachedToTarget" {
            let params = value.get("params").cloned().unwrap_or_default();
            let Some(session_id) = params.get("sessionId").and_then(|value| value.as_str()) else {
                continue;
            };
            let target_info = params.get("targetInfo").cloned().unwrap_or_default();
            let target_id = target_info.get("targetId").and_then(|value| value.as_str());
            let target_type = target_info
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            tracing::debug!(
                port,
                target_id = ?target_id,
                target_type,
                "Chrome target attached"
            );

            let is_initial_page = target_type == "page"
                && target_id.is_some()
                && target_id == initial_target_id.as_deref();
            if target_type == "page" && !is_initial_page {
                if let Err(error) = configure_attached_page(
                    &mut socket,
                    &mut next_command_id,
                    session_id,
                    &profile,
                    version.user_agent.as_deref(),
                    sender.is_some(),
                )
                .await
                {
                    tracing::debug!(port, "Could not configure new Chrome page: {}", error);
                } else {
                    tracing::debug!(port, target_id = ?target_id, "Configured new Chrome page");
                }
            } else {
                // Non-page targets (iframes, workers, extensions) must not
                // remain paused when auto-attach is enabled.
                let _ = send_target_command(
                    &mut socket,
                    &mut next_command_id,
                    Some(session_id),
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                )
                .await;
            }
            continue;
        }

        if method.starts_with("Network.") {
            if let (Some(sender), Some(session_id)) = (
                sender.as_ref(),
                value.get("sessionId").and_then(|value| value.as_str()),
            ) {
                record_network_event(
                    session_id,
                    method,
                    value.get("params").cloned().unwrap_or_default(),
                    &mut pending,
                    sender,
                )
                .await;
            }
        }
    }
}

async fn configure_attached_page(
    socket: &mut BrowserSocket,
    next_command_id: &mut u64,
    session_id: &str,
    profile: &GeoProfile,
    user_agent: Option<&str>,
    capture_requests: bool,
) -> Result<()> {
    if capture_requests {
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Network.enable",
            json!({}),
        )
        .await?;
    }
    if let Some(language) = profile.language.as_deref() {
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": language_override_script(language)}),
        )
        .await?;
        if let Some(user_agent) = user_agent {
            send_target_command(
                socket,
                next_command_id,
                Some(session_id),
                "Network.setUserAgentOverride",
                json!({"userAgent": user_agent, "acceptLanguage": language}),
            )
            .await?;
        }
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Emulation.setLocaleOverride",
            json!({"locale": language}),
        )
        .await?;
    }
    if let Some(timezone) = profile.timezone.as_deref() {
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Emulation.setTimezoneOverride",
            json!({"timezoneId": timezone}),
        )
        .await?;
    }
    if let (Some(latitude), Some(longitude)) = (profile.latitude, profile.longitude) {
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Emulation.setGeolocationOverride",
            json!({"latitude": latitude, "longitude": longitude, "accuracy": 100.0}),
        )
        .await?;
    }
    send_target_command(
        socket,
        next_command_id,
        Some(session_id),
        "Runtime.runIfWaitingForDebugger",
        json!({}),
    )
    .await?;
    Ok(())
}

async fn send_target_command(
    socket: &mut BrowserSocket,
    next_command_id: &mut u64,
    session_id: Option<&str>,
    method: &str,
    params: serde_json::Value,
) -> Result<()> {
    let id = *next_command_id;
    *next_command_id += 1;
    let mut command = json!({"id": id, "method": method, "params": params});
    if let Some(session_id) = session_id {
        command["sessionId"] = json!(session_id);
    }
    socket
        .send(Message::Text(command.to_string()))
        .await
        .with_context(|| format!("Failed to send Chrome target command {}", method))?;
    Ok(())
}

async fn record_network_event(
    session_id: &str,
    method: &str,
    params: serde_json::Value,
    pending: &mut HashMap<(String, String), PendingRequest>,
    sender: &RequestLogSender,
) {
    let Some(request_id) = params.get("requestId").and_then(|value| value.as_str()) else {
        return;
    };
    let key = (session_id.to_string(), request_id.to_string());
    match method {
        "Network.requestWillBeSent" => {
            let request = params.get("request").cloned().unwrap_or_default();
            let Some(url) = request.get("url").and_then(|value| value.as_str()) else {
                return;
            };
            if url.starts_with("data:") || url.starts_with("blob:") {
                return;
            }
            pending.insert(
                key,
                PendingRequest {
                    started_at: Instant::now(),
                    method: request
                        .get("method")
                        .and_then(|value| value.as_str())
                        .unwrap_or("GET")
                        .to_string(),
                    url: sanitize_request_url(url),
                    status: 0,
                },
            );
        }
        "Network.responseReceived" => {
            if let Some(request) = pending.get_mut(&key) {
                request.status = params
                    .get("response")
                    .and_then(|response| response.get("status"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
                    .min(u16::MAX as u64) as u16;
            }
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            if let Some(request) = pending.remove(&key) {
                let failed = method == "Network.loadingFailed";
                let entry = RequestLogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    instance: "web".to_string(),
                    method: request.method,
                    url: request.url,
                    status: if failed { 0 } else { request.status },
                    latency_ms: request.started_at.elapsed().as_millis() as u64,
                    size_bytes: if failed {
                        0
                    } else {
                        params
                            .get("encodedDataLength")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0)
                    },
                };
                let _ = sender.send(entry).await;
            }
        }
        _ => {}
    }
}

async fn monitor_browser_socket(
    mut socket: BrowserSocket,
    root_pid: u32,
    sender: RequestLogSender,
) {
    let mut pending = HashMap::<String, PendingRequest>::new();
    loop {
        if !process_exists(root_pid) {
            break;
        }

        let message = match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => continue,
        };
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(method) = value.get("method").and_then(|value| value.as_str()) else {
            continue;
        };
        let params = value.get("params").cloned().unwrap_or_default();

        match method {
            "Network.requestWillBeSent" => {
                let Some(request_id) = params.get("requestId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                let request = params.get("request").cloned().unwrap_or_default();
                let Some(url) = request.get("url").and_then(|value| value.as_str()) else {
                    continue;
                };
                if url.starts_with("data:") || url.starts_with("blob:") {
                    continue;
                }
                pending.insert(
                    request_id.to_string(),
                    PendingRequest {
                        started_at: Instant::now(),
                        method: request
                            .get("method")
                            .and_then(|value| value.as_str())
                            .unwrap_or("GET")
                            .to_string(),
                        url: sanitize_request_url(url),
                        status: 0,
                    },
                );
            }
            "Network.responseReceived" => {
                let Some(request_id) = params.get("requestId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                if let Some(request) = pending.get_mut(request_id) {
                    request.status = params
                        .get("response")
                        .and_then(|response| response.get("status"))
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                        .min(u16::MAX as u64) as u16;
                }
            }
            "Network.loadingFinished" => {
                let Some(request_id) = params.get("requestId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                if let Some(request) = pending.remove(request_id) {
                    let entry = RequestLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        instance: "web".to_string(),
                        method: request.method,
                        url: request.url,
                        status: request.status,
                        latency_ms: request.started_at.elapsed().as_millis() as u64,
                        size_bytes: params
                            .get("encodedDataLength")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0),
                    };
                    if sender.send(entry).await.is_err() {
                        break;
                    }
                }
            }
            "Network.loadingFailed" => {
                let Some(request_id) = params.get("requestId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                if let Some(request) = pending.remove(request_id) {
                    let entry = RequestLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        instance: "web".to_string(),
                        method: request.method,
                        url: request.url,
                        status: 0,
                        latency_ms: request.started_at.elapsed().as_millis() as u64,
                        size_bytes: 0,
                    };
                    if sender.send(entry).await.is_err() {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

fn sanitize_request_url(url: &str) -> String {
    url.split_once('?')
        .map(|(base, _)| format!("{}?…", base))
        .unwrap_or_else(|| url.to_string())
}

fn language_override_script(language: &str) -> String {
    let base = language.split('-').next().unwrap_or(language);
    format!(
        "Object.defineProperty(Navigator.prototype, 'language', {{get: () => {:?}}}); \
         Object.defineProperty(Navigator.prototype, 'languages', {{get: () => [{:?}, {:?}]}});",
        language, language, base
    )
}

async fn send_cdp_command<S>(
    socket: &mut WebSocketStream<S>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tracing::debug!(id, method, "Sending Chrome DevTools command");
    socket
        .send(Message::Text(
            json!({"id": id, "method": method, "params": params}).to_string(),
        ))
        .await
        .context("Failed to send Chrome DevTools command")?;

    // A navigation through a high-latency SOCKS5 exit can take several
    // seconds before Chrome emits the Page.navigate reply. The old 2-second
    // limit made a healthy browser look like a failed launch and produced a
    // synthetic ~2000 ms request error in the dashboard. Keep enough time for
    // the proxy round trip while still bounding a dead DevTools connection.
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                            if let Some(error) = value.get("error") {
                                anyhow::bail!(
                                    "Chrome DevTools command {} failed: {}",
                                    method,
                                    error
                                );
                            }
                            tracing::debug!(id, method, "Chrome DevTools command completed");
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                            if let Some(error) = value.get("error") {
                                anyhow::bail!(
                                    "Chrome DevTools command {} failed: {}",
                                    method,
                                    error
                                );
                            }
                            tracing::debug!(id, method, "Chrome DevTools command completed");
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    anyhow::bail!(
                        "Chrome DevTools connection closed before command {} completed: {:?}",
                        method,
                        frame
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "Chrome DevTools receive failed for {}: {}",
                        method,
                        error
                    ));
                }
            }
        }
        anyhow::bail!(
            "Chrome DevTools connection ended before command {} completed",
            method
        )
    })
    .await
    .context("Timed out waiting for Chrome DevTools response")??;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{language_override_script, sanitize_request_url};

    #[test]
    fn request_log_urls_drop_query_values() {
        assert_eq!(
            sanitize_request_url("https://claude.ai/api/messages?token=secret"),
            "https://claude.ai/api/messages?…"
        );
        assert_eq!(
            sanitize_request_url("https://claude.ai/login"),
            "https://claude.ai/login"
        );
    }

    #[test]
    fn language_script_sets_primary_and_base_languages() {
        let script = language_override_script("zh-CN");
        assert!(script.contains("zh-CN"));
        assert!(script.contains("zh"));
        assert!(script.contains("Navigator.prototype"));
    }
}
