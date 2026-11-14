use super::*;

/// Chromium normally keeps every helper below the browser root, but a helper
/// can be briefly reparented during a crash or restart. Match the unique
/// per-launch user-data directory as a second signal so validation still
/// covers that helper while never sweeping in another browser profile.
fn command_uses_session_dir(command: &str, session_dir: &str) -> bool {
    let marker = format!("--user-data-dir={session_dir}");
    command
        .split_whitespace()
        .any(|argument| argument == marker)
}

fn parse_session_processes(output: &str, session_dir: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if !command_uses_session_dir(&command, session_dir) {
                return None;
            }
            pid.trim().parse().ok()
        })
        .collect()
}

pub(super) fn processes_using_session_dir(session_dir: &str) -> Vec<u32> {
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };

    parse_session_processes(&String::from_utf8_lossy(&output.stdout), session_dir)
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

pub(super) struct BrowserProfileSession {
    pub(super) socket: BrowserSocket,
    pub(super) target_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DevToolsVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
    #[serde(rename = "User-Agent")]
    user_agent: Option<String>,
}

pub(super) fn find_free_port() -> Result<u16> {
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

pub(super) async fn apply_browser_profile(
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

const MAX_PENDING_REQUESTS: usize = 2_048;
const PENDING_REQUEST_TTL: Duration = Duration::from_secs(60);

/// Keep a stalled DevTools request from growing memory without allowing log
/// backpressure to interfere with browser networking.
fn prune_pending<K: Eq + Hash + Clone>(pending: &mut HashMap<K, PendingRequest>) {
    let now = Instant::now();
    pending.retain(|_, request| now.duration_since(request.started_at) <= PENDING_REQUEST_TTL);
    while pending.len() > MAX_PENDING_REQUESTS {
        let oldest = pending
            .iter()
            .min_by_key(|(_, request)| request.started_at)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            pending.remove(&key);
        } else {
            break;
        }
    }
}

fn network_failure_reason(params: &serde_json::Value) -> Option<String> {
    params
        .get("errorText")
        .and_then(|value| value.as_str())
        .or_else(|| params.get("blockedReason").and_then(|value| value.as_str()))
        .or_else(|| {
            params
                .get("canceled")
                .and_then(|value| value.as_bool())
                .filter(|v| *v)
                .map(|_| "canceled")
        })
        .map(str::to_string)
}

fn request_outcome(status: u16, failed: bool) -> String {
    if failed {
        "network_error".to_string()
    } else if status >= 400 {
        "http_error".to_string()
    } else if status == 0 {
        "no_response".to_string()
    } else {
        "success".to_string()
    }
}

/// Observe Chromium's Network domain and publish completed requests to the
/// dashboard. The monitor is tied to the browser root PID and exits when the
/// isolated browser is closed.
pub(super) async fn monitor_browser_requests(port: u16, root_pid: u32, sender: RequestLogSender) {
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
pub(super) async fn monitor_browser_targets(
    port: u16,
    root_pid: u32,
    profile: GeoProfile,
    initial_target_id: Option<String>,
    sender: Option<RequestLogSender>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        if !process_exists(root_pid) {
            break;
        }
        monitor_browser_targets_once(
            port,
            root_pid,
            profile.clone(),
            initial_target_id.clone(),
            sender.clone(),
        )
        .await;
        if !process_exists(root_pid) {
            break;
        }
        tracing::warn!(
            port,
            retry_in_ms = backoff.as_millis() as u64,
            "Chrome DevTools target monitor disconnected; retrying"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

pub(super) async fn monitor_browser_targets_once(
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
    prune_pending(pending);
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
            let sanitized_url = sanitize_request_url(url);
            if sanitized_url.is_empty() {
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
                    url: sanitized_url,
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
                let error = if failed {
                    network_failure_reason(&params)
                } else {
                    None
                };
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
                    outcome: request_outcome(request.status, failed),
                    error,
                };
                if sender.try_send(entry).is_err() {
                    tracing::warn!("Dropping Web request log because the log buffer is full");
                }
            }
        }
        _ => {}
    }
}

pub(super) async fn monitor_browser_socket(
    mut socket: BrowserSocket,
    root_pid: u32,
    sender: RequestLogSender,
) {
    let mut pending = HashMap::<String, PendingRequest>::new();
    loop {
        if !process_exists(root_pid) {
            break;
        }

        prune_pending(&mut pending);

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
                let sanitized_url = sanitize_request_url(url);
                if sanitized_url.is_empty() {
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
                        url: sanitized_url,
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
                        outcome: request_outcome(request.status, false),
                        error: None,
                    };
                    match sender.try_send(entry) {
                        Ok(_) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                "Dropping Web request log because the log buffer is full"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
            "Network.loadingFailed" => {
                let Some(request_id) = params.get("requestId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                if let Some(request) = pending.remove(request_id) {
                    let error = network_failure_reason(&params);
                    let entry = RequestLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        instance: "web".to_string(),
                        method: request.method,
                        url: request.url,
                        status: 0,
                        latency_ms: request.started_at.elapsed().as_millis() as u64,
                        size_bytes: 0,
                        outcome: request_outcome(request.status, true),
                        error,
                    };
                    match sender.try_send(entry) {
                        Ok(_) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                "Dropping Web request log because the log buffer is full"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn sanitize_request_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return String::new();
    };
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return String::new();
    }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = rest[..authority_end]
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(&rest[..authority_end]);
    let path_and_query = &rest[authority_end..];
    let path = path_and_query.split(['?', '#']).next().unwrap_or_default();
    let sensitive = ["/auth", "/login", "/oauth", "/token", "/session"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));
    let safe_path = if sensitive {
        "/[redacted]".to_string()
    } else {
        let redacted_path = path
            .split('/')
            .map(|segment| {
                let compact_len = segment
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .count();
                if segment.len() > 48 || compact_len > 24 {
                    "[redacted]"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        if redacted_path.len() > 180 {
            format!("{}…", &redacted_path[..180])
        } else {
            redacted_path
        }
    };
    format!("{scheme}://{authority}{safe_path}")
}

pub(crate) fn language_override_script(language: &str) -> String {
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
mod session_marker_tests {
    use super::{command_uses_session_dir, parse_session_processes, processes_using_session_dir};

    #[test]
    fn marker_matches_only_the_full_session_path() {
        let session = "/tmp/cim-web/sessions/abc";
        assert!(command_uses_session_dir(
            "Chrome --user-data-dir=/tmp/cim-web/sessions/abc --type=renderer",
            session
        ));
        assert!(!command_uses_session_dir(
            "Chrome --user-data-dir=/tmp/cim-web/sessions/abcd --type=renderer",
            session
        ));
        assert!(!command_uses_session_dir(
            "Chrome --user-data-dir=/tmp/other --type=renderer",
            session
        ));
    }

    #[test]
    fn parser_collects_only_processes_with_exact_session_marker() {
        let session = "/tmp/cim-web/sessions/abc";
        let ps = format!(
            " 101 Chrome --user-data-dir={session} --type=renderer\n 102 Chrome --user-data-dir={session}d --type=renderer\n 103 /bin/sh -c --user-data-dir=/tmp/other sleep 30\n"
        );
        assert_eq!(parse_session_processes(&ps, session), vec![101]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_marker_helper_is_detected_and_terminated() {
        use crate::instance::terminate_processes;
        use std::process::Command;
        use uuid::Uuid;

        let session = std::env::temp_dir().join(format!("cim-web-session-{}", Uuid::new_v4()));
        let session = session.to_string_lossy().into_owned();
        let marker_arg = format!("--user-data-dir={session}");
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & wait", &marker_arg])
            .spawn()
            .expect("shell is required for detached marker cleanup test");

        // Give the shell time to create the sleep helper. In the Codex app
        // sandbox `ps` can be denied; that environment cannot observe marker
        // PIDs, so clean up the root and leave the opt-in macOS regression to
        // the real proxy integration script.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let marker_pids = processes_using_session_dir(&session);
        if marker_pids.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        assert!(marker_pids.contains(&child.id()));
        terminate_processes(&marker_pids).await;
        for _ in 0..20 {
            if child.try_wait().expect("poll detached helper").is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("detached marker helper {} survived cleanup", child.id());
    }
}
