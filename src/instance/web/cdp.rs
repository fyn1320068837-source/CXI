use super::*;

/// Chromium normally keeps every helper below the browser root, but a helper
/// can be briefly reparented during a crash or restart. Match the unique
/// exact user-data directory or CIM session marker so validation and cleanup
/// still cover that helper while never sweeping in another browser profile.
fn command_uses_session_dir(command: &str, session_dir: &str) -> bool {
    let marker = format!("--user-data-dir={session_dir}");
    command
        .split_whitespace()
        .any(|argument| argument == marker)
}

fn command_uses_session_marker(command: &str, session_dir: &str) -> bool {
    let marker = format!("--cim-session-dir={session_dir}");
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
            if !command_uses_session_dir(&command, session_dir)
                && !command_uses_session_marker(&command, session_dir)
            {
                return None;
            }
            pid.trim().parse().ok()
        })
        .collect()
}

pub(super) fn processes_using_session_dir(session_dir: &str) -> Vec<u32> {
    processes_using_session_dir_checked(session_dir).unwrap_or_default()
}

pub(super) fn processes_using_session_marker(session_dir: &str) -> Vec<u32> {
    processes_using_session_marker_checked(session_dir).unwrap_or_default()
}

fn ps_process_output() -> Result<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .context("failed to run ps while scanning Chromium processes")?;
    if !output.status.success() {
        anyhow::bail!(
            "ps failed while scanning Chromium processes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn processes_using_session_dir_checked(session_dir: &str) -> Result<Vec<u32>> {
    Ok(parse_session_processes(&ps_process_output()?, session_dir))
}

pub(super) fn processes_using_session_marker_checked(session_dir: &str) -> Result<Vec<u32>> {
    Ok(ps_process_output()?
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if !command_uses_session_marker(&command, session_dir) {
                return None;
            }
            pid.trim().parse().ok()
        })
        .collect())
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

/// Open a new tab in the CIM-owned Chromium profile through its loopback
/// DevTools endpoint. Chromium versions differ in whether they expose
/// `/json/new` (and in which HTTP method they accept), so fall back to a
/// page-scoped `Page.navigate` command when the discovery endpoint rejects the
/// request. The endpoint is only bound to 127.0.0.1 and the caller validates
/// the destination first.
pub(super) async fn navigate_browser(port: u16, url: &str) -> Result<()> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    // Chrome's `/json/new` endpoint treats the whole query string as the
    // destination URL.  Passing `?url=...` returns 200 but silently opens an
    // about:blank tab on current Chromium builds.
    let endpoint = reqwest::Url::parse(&format!(
        "http://127.0.0.1:{port}/json/new?{}",
        encode_query_component(url)
    ))
    .context("invalid CIM Web DevTools endpoint")?;

    match client.put(endpoint).send().await {
        Ok(response) if response.status().is_success() => {
            let target = response.json::<DevToolsTarget>().await.ok();
            if target
                .as_ref()
                .and_then(|target| target.url.as_deref())
                .filter(|target_url| !target_url.is_empty() && *target_url != "about:blank")
                .is_some()
            {
                return Ok(());
            }
            tracing::debug!(
                port,
                "Chromium created a blank target; falling back to Page.navigate"
            );
        }
        Ok(response) => {
            // A number of bundled Chromium builds return 404/405 for
            // `/json/new` even though page-level CDP navigation is enabled.
            // Keep this detail local: the user-facing API should not expose a
            // browser version or the OAuth URL.
            tracing::debug!(port, status = %response.status(), "Chromium rejected /json/new; falling back to Page.navigate");
        }
        Err(error) => {
            tracing::debug!(port, error = %error, "Chromium /json/new request failed; falling back to Page.navigate");
        }
    }

    let list_endpoint = format!("http://127.0.0.1:{port}/json/list");
    let targets = client
        .get(list_endpoint)
        .send()
        .await
        .context("failed to list CIM Web DevTools targets")?
        .error_for_status()
        .context("CIM Web DevTools refused target discovery")?
        .json::<Vec<DevToolsTarget>>()
        .await
        .context("invalid CIM Web DevTools target list")?;
    let target = targets
        .into_iter()
        .find(|target| {
            target.target_type.as_deref() == Some("page") && target.websocket_url.is_some()
        })
        .context("CIM Web has no page target for authentication")?;
    let websocket_url = target
        .websocket_url
        .context("CIM Web page target has no DevTools endpoint")?;
    let (mut socket, _) = connect_async(websocket_url)
        .await
        .context("failed to connect to CIM Web page DevTools")?;
    send_cdp_command(&mut socket, 1, "Page.navigate", json!({"url": url}))
        .await
        .context("CIM Web refused the authentication navigation")?;
    Ok(())
}

fn encode_query_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
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
    site: WebSite,
) -> Result<BrowserProfileSession> {
    tracing::debug!(port, "Applying browser locale/timezone profile");
    // Chrome DevTools is a loopback endpoint. Do not let HTTP(S)_PROXY from
    // the host redirect these control requests through another proxy.
    let client = reqwest::Client::builder().no_proxy().build()?;
    let endpoint = format!("http://127.0.0.1:{}/json/list", port);
    // A persistent profile may need to replay migrations, restore cookies and
    // start Chrome's helper tree before the first page target appears. Keep
    // the browser launch transactional, but allow that cold-start window.
    let target = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(response) = client.get(&endpoint).send().await {
                if let Ok(targets) = response.json::<Vec<DevToolsTarget>>().await {
                    let page_target = targets.iter().find(|target| {
                        target.target_type.as_deref() == Some("page")
                            && target.websocket_url.is_some()
                            && target
                                .url
                                .as_deref()
                                .map(|url| {
                                    reqwest::Url::parse(url)
                                        .map(|url| {
                                            url.origin().ascii_serialization() == site.origin()
                                        })
                                        .unwrap_or(false)
                                })
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
                        "origin": site.origin(),
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
        let language_script = site.language_script(language);
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

    if let Some(language) = profile
        .language
        .as_deref()
        .filter(|_| site == WebSite::Claude)
    {
        // Keep Claude's own persisted locale aligned with the current exit
        // profile. The page may have been restored from a previous Chrome
        // session, so the new-document hook alone is not sufficient here.
        send_cdp_command(
            &mut socket,
            command_id,
            "Network.setCookie",
            json!({
                "name": "signin-locale",
                "value": language,
                "url": "https://claude.ai/",
                "path": "/",
                "secure": true,
                "sameSite": "Lax"
            }),
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
        json!({"url": site.entry_url()}),
    )
    .await?;
    command_id += 1;

    if let Some(language) = profile
        .language
        .as_deref()
        .filter(|_| site == WebSite::Claude)
    {
        // Give the restored target a short chance to become the Claude
        // document, then update its storage and reload once. This fixes a
        // persisted site preference before Claude's app selects its UI copy.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let locale_script = site.language_script(language);
        send_cdp_command(
            &mut socket,
            command_id,
            "Runtime.evaluate",
            json!({"expression": locale_script, "returnByValue": true}),
        )
        .await?;
        command_id += 1;
        send_cdp_command(&mut socket, command_id, "Page.reload", json!({})).await?;
        command_id += 1;
        tokio::time::sleep(Duration::from_millis(500)).await;
        send_cdp_command(
            &mut socket,
            command_id,
            "Runtime.evaluate",
            json!({"expression": site.language_script(language), "returnByValue": true}),
        )
        .await?;
    }

    tracing::debug!(port, "Browser locale/timezone profile applied");
    Ok(BrowserProfileSession { socket, target_id })
}

#[derive(Debug)]
struct PendingRequest {
    instance: String,
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
pub(super) async fn monitor_browser_requests(
    port: u16,
    root_pid: u32,
    sender: RequestLogSender,
    site: WebSite,
) {
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
    monitor_browser_socket(socket, root_pid, sender, site).await;
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
    site: WebSite,
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
            site,
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
    site: WebSite,
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
                    site,
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
                    site,
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
    _site: WebSite,
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
        send_target_command(
            socket,
            next_command_id,
            Some(session_id),
            "Runtime.evaluate",
            json!({
                "expression": language_override_script(language),
                "returnByValue": true
            }),
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
    site: WebSite,
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
                    instance: request_owner(&params, site),
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
                    instance: request.instance,
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
    site: WebSite,
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
                        instance: request_owner(&params, site),
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
                        instance: request.instance,
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
                        instance: request.instance,
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
    let sensitive = [
        "/auth",
        "/login",
        "/oauth",
        "/token",
        "/session",
        "/sso",
        "/authorize",
        "/callback",
        "/signin",
        "/sign-in",
    ]
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
    let app_locale = match language {
        value if value.eq_ignore_ascii_case("zh-CN") => "zh-Hans",
        value if value.eq_ignore_ascii_case("zh-TW") => "zh-Hant",
        value if value.eq_ignore_ascii_case("ja-JP") => "ja",
        value if value.eq_ignore_ascii_case("ko-KR") => "ko",
        _ => language,
    };
    format!(
        "(function() {{ \
            Object.defineProperty(Navigator.prototype, 'language', {{configurable: true, get: () => {:?}}}); \
            Object.defineProperty(Navigator.prototype, 'languages', {{configurable: true, get: () => [{:?}, {:?}]}}); \
            try {{ \
                if (location.origin === 'https://claude.ai' || location.origin === 'https://www.claude.ai') {{ \
                    localStorage.setItem('spa:locale', {:?}); \
                    localStorage.setItem('spa:locale-source', 'dynamic'); \
                    document.cookie = 'signin-locale=' + encodeURIComponent({:?}) + '; Path=/; Secure; SameSite=Lax'; \
                }} \
            }} catch (_) {{}} \
        }})();",
        language, language, base, app_locale, language
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
    use super::{
        command_uses_session_dir, command_uses_session_marker, encode_query_component,
        parse_session_processes, processes_using_session_dir,
    };

    #[test]
    fn devtools_new_target_uses_raw_encoded_query() {
        assert_eq!(
            encode_query_component("https://accounts.google.com/o/oauth2?x=1&y=2"),
            "https%3A%2F%2Faccounts.google.com%2Fo%2Foauth2%3Fx%3D1%26y%3D2"
        );
    }

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
        assert!(command_uses_session_marker(
            "Chrome --user-data-dir=/tmp/cim-web/profile --cim-session-dir=/tmp/cim-web/sessions/abc --type=renderer",
            session
        ));
        assert!(!command_uses_session_marker(
            "Chrome --cim-session-dir=/tmp/cim-web/sessions/abcd --type=renderer",
            session
        ));
    }

    #[test]
    fn parser_collects_only_processes_with_exact_session_marker() {
        let session = "/tmp/cim-web/sessions/abc";
        let ps = format!(
            " 101 Chrome --user-data-dir={session} --type=renderer\n 102 Chrome --user-data-dir={session}d --type=renderer\n 103 Chrome --user-data-dir=/tmp/cim-web/profile --cim-session-dir={session} --type=renderer\n 104 /bin/sh -c --user-data-dir=/tmp/other sleep 30\n"
        );
        assert_eq!(parse_session_processes(&ps, session), vec![101, 103]);
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

#[cfg(test)]
mod gemini_tests {
    use super::*;

    #[tokio::test]
    async fn initial_cdp_overrides_precede_product_navigation_without_cross_site_cookies() {
        use axum::{
            extract::ws::{Message as AxumMessage, WebSocketUpgrade},
            routing::get,
            Json, Router,
        };
        for site in [WebSite::Claude, WebSite::Gemini] {
            let commands =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let captured = commands.clone();
            let router = Router::new()
                .route("/json/list", get(move || async move { Json(json!([{"id":"initial", "type":"page", "url":"about:blank", "webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/page")}])) }))
                .route("/json/version", get(|| async { Json(json!({"User-Agent":"TestBrowser"})) }))
                .route("/page", get(move |ws: WebSocketUpgrade| {
                    let captured = captured.clone();
                    async move { ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumMessage::Text(text))) = socket.recv().await {
                            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                            let id = value["id"].clone();
                            captured.lock().unwrap().push(value);
                            if socket.send(AxumMessage::Text(json!({"id":id,"result":{}}).to_string())).await.is_err() { break; }
                        }
                    }) }
                }));
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let profile = GeoProfile {
                source_ip: Some("203.0.113.5".into()),
                language: Some("en-US".into()),
                locale: Some("en_US.UTF-8".into()),
                timezone: Some("America/New_York".into()),
                ..GeoProfile::default()
            };
            let session = apply_browser_profile(port, &profile, true, site)
                .await
                .unwrap();
            drop(session);
            let commands = commands.lock().unwrap().clone();
            let navigation = commands
                .iter()
                .position(|c| c["method"] == "Page.navigate")
                .unwrap();
            assert_eq!(commands[navigation]["params"]["url"], site.entry_url());
            for method in [
                "Network.enable",
                "Page.addScriptToEvaluateOnNewDocument",
                "Network.setUserAgentOverride",
                "Emulation.setTimezoneOverride",
                "Emulation.setLocaleOverride",
            ] {
                assert!(
                    commands[..navigation].iter().any(|c| c["method"] == method),
                    "missing {method}"
                );
            }
            let cookies = commands
                .iter()
                .filter(|c| c["method"] == "Network.setCookie")
                .count();
            assert_eq!(cookies, usize::from(site == WebSite::Claude));
            if site == WebSite::Gemini {
                assert!(!serde_json::to_string(&commands)
                    .unwrap()
                    .contains("claude.ai"));
                assert!(!commands.iter().any(|c| c["method"] == "Page.reload"));
            }
            server.abort();
            let _ = server.await;
        }
    }

    #[test]
    fn profile_process_matching_does_not_cross_products() {
        let claude = "/fixture/.cim/instances/web/profile";
        let gemini = "/fixture/.cim/instances/gemini-web/profile";
        assert!(!command_uses_session_dir(
            &format!("Chrome --user-data-dir={claude}"),
            gemini
        ));
        assert!(!command_uses_session_dir(
            &format!("Chrome --user-data-dir={gemini}"),
            claude
        ));
    }

    #[tokio::test]
    async fn request_logs_are_bound_to_the_product_and_strip_query_secrets() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        for site in [WebSite::Claude, WebSite::Gemini] {
            let mut pending = HashMap::new();
            record_network_event("session", "Network.requestWillBeSent", json!({"requestId":"1", "request":{"url":format!("{}/app?token=secret",site.origin()),"method":"GET"}}), &mut pending, &tx, site).await;
            record_network_event(
                "session",
                "Network.responseReceived",
                json!({"requestId":"1", "response":{"status":200}}),
                &mut pending,
                &tx,
                site,
            )
            .await;
            record_network_event(
                "session",
                "Network.loadingFinished",
                json!({"requestId":"1"}),
                &mut pending,
                &tx,
                site,
            )
            .await;
            let entry = rx.recv().await.unwrap();
            assert_eq!(entry.instance, site.log_key());
            assert_eq!(entry.status, 200);
            assert!(!entry.url.contains("secret"));
        }
    }
}

pub(super) async fn tab_exists(port: u16, id: &str) -> Result<bool> {
    let targets = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<DevToolsTarget>>()
        .await?;
    Ok(targets.iter().any(|t| t.target_id.as_deref() == Some(id)))
}

pub(super) async fn tab_action(port: u16, action: &str, id: &str) -> Result<()> {
    anyhow::ensure!(matches!(action, "activate" | "close"), "invalid tab action");
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?
        .get(format!(
            "http://127.0.0.1:{port}/json/{action}/{}",
            encode_query_component(id)
        ))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

pub(super) async fn open_site_tab(
    port: u16,
    site: WebSite,
    profile: &GeoProfile,
    root_pid: u32,
) -> Result<String> {
    open_url_tab(port, site.entry_url(), profile, root_pid).await
}

/// Create a blank tab in the existing window, install the same environment,
/// then navigate. Never fall back to navigating an arbitrary existing tab.
pub(super) async fn open_url_tab(
    port: u16,
    url: &str,
    profile: &GeoProfile,
    root_pid: u32,
) -> Result<String> {
    anyhow::ensure!(profile.is_ready(), "GeoIP profile is not ready");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let target = client
        .put(format!("http://127.0.0.1:{port}/json/new?about:blank"))
        .send()
        .await?
        .error_for_status()?
        .json::<DevToolsTarget>()
        .await?;
    let id = target.target_id.context("created tab has no id")?;
    let setup = async {
        let (mut socket, _) = connect_async(target.websocket_url.context("created tab has no debugger")?).await?;
        let version = client.get(format!("http://127.0.0.1:{port}/json/version")).send().await?.error_for_status()?.json::<DevToolsVersion>().await?;
        let language = profile.language.as_deref().context("language missing")?;
        let timezone = profile.timezone.as_deref().context("timezone missing")?;
        for (i, (method, params)) in [
            ("Page.enable", json!({})),
            ("Page.addScriptToEvaluateOnNewDocument", json!({"source": language_override_script(language)})),
            ("Network.setUserAgentOverride", json!({"userAgent":version.user_agent.context("browser user agent missing")?,"acceptLanguage":language})),
            ("Emulation.setLocaleOverride", json!({"locale":language})),
            ("Emulation.setTimezoneOverride", json!({"timezoneId":timezone})),
            ("Emulation.setGeolocationOverride", json!({"latitude":profile.latitude,"longitude":profile.longitude,"accuracy":100})),
            ("Page.navigate", json!({"url":url})),
        ].into_iter().enumerate() {
            if let Err(error) = send_cdp_command(&mut socket, i as u64 + 1, method, params).await {
                // Chrome rejects a second debugger's locale/timezone override.
                // Accept only this specific conflict, after proving the live value.
                let property = match method {
                    "Emulation.setLocaleOverride" => Some(("locale", language)),
                    "Emulation.setTimezoneOverride" => Some(("timeZone", timezone)),
                    _ => None,
                };
                if let Some((property, expected)) = property.filter(|_| error.to_string().contains("already in effect")) {
                    verify_runtime_value(&mut socket, 100 + i as u64, property, expected).await?;
                } else { return Err(error); }
            }
        }
        // Keep environment overrides attached. The browser target monitor owns logs.
        tokio::spawn(async move {
            while process_exists(root_pid) {
                match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
                    Ok(Some(Ok(_))) | Err(_) => {},
                    _ => break,
                }
            }
        });
        Ok::<(), anyhow::Error>(())
    }.await;
    if let Err(error) = setup {
        tracing::error!(error = %format!("{error:#}"), "Shared browser tab setup failed");
        let close = tab_action(port, "close", &id).await;
        return Err(error.context(format!("new tab setup failed; tab cleanup: {close:?}")));
    }
    tab_action(port, "activate", &id).await?;
    Ok(id)
}

// Attribute subresources to their document, not to the site that launched Chrome.
fn request_owner(params: &serde_json::Value, fallback: WebSite) -> String {
    let url = params
        .get("documentURL")
        .and_then(|v| v.as_str())
        .or_else(|| params.pointer("/request/url").and_then(|v| v.as_str()));
    match url
        .and_then(|u| reqwest::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned))
        .as_deref()
    {
        Some("gemini.google.com") => "gemini-web".into(),
        Some("claude.ai" | "www.claude.ai") => "web".into(),
        // Auth pages and unrelated tabs cannot reliably be attributed by hostname.
        Some(_) => "browser".into(),
        None => fallback.log_key().into(),
    }
}

#[cfg(test)]
mod shared_log_tests {
    use super::*;
    #[test]
    fn shared_requests_follow_document_origin_not_first_launched_site() {
        assert_eq!(
            request_owner(
                &json!({"documentURL":"https://gemini.google.com/app", "request":{"url":"https://www.gstatic.com/script.js"}}),
                WebSite::Claude
            ),
            "gemini-web"
        );
        assert_eq!(
            request_owner(
                &json!({"documentURL":"https://claude.ai/new", "request":{"url":"https://www.gstatic.com/script.js"}}),
                WebSite::Gemini
            ),
            "web"
        );
        assert_eq!(
            request_owner(
                &json!({"documentURL":"https://accounts.google.com/", "request":{"url":"https://www.gstatic.com/script.js"}}),
                WebSite::Claude
            ),
            "browser"
        );
    }
}

async fn verify_runtime_value(
    socket: &mut BrowserSocket,
    id: u64,
    property: &str,
    expected: &str,
) -> Result<()> {
    let expression = format!(
        "Intl.DateTimeFormat().resolvedOptions()[{}]",
        serde_json::to_string(property)?
    );
    socket.send(Message::Text(json!({"id":id,"method":"Runtime.evaluate","params":{"expression":expression,"returnByValue":true}}).to_string())).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = socket.next().await {
            let message = message?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text)?;
            if value.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            anyhow::ensure!(
                value
                    .pointer("/result/result/value")
                    .and_then(|v| v.as_str())
                    == Some(expected),
                "shared browser {property} does not match proxy profile"
            );
            return Ok::<_, anyhow::Error>(());
        }
        anyhow::bail!("DevTools disconnected during environment verification")
    })
    .await
    .context("timed out verifying shared browser environment")?
}

#[cfg(test)]
mod shared_environment_tests {
    use super::*;
    #[tokio::test]
    async fn existing_environment_must_match_before_navigation() {
        for (actual, pass) in [("en-US", true), ("zh-CN", false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request = socket.next().await.unwrap().unwrap();
                let value: serde_json::Value =
                    serde_json::from_str(request.to_text().unwrap()).unwrap();
                socket.send(Message::Text(json!({"id":value["id"],"result":{"result":{"type":"string","value":actual}}}).to_string())).await.unwrap();
            });
            let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{port}"))
                .await
                .unwrap();
            assert_eq!(
                verify_runtime_value(&mut socket, 100, "locale", "en-US")
                    .await
                    .is_ok(),
                pass
            );
            server.await.unwrap();
        }
    }
}
