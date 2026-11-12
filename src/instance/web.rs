// Claude Web in Chromium adapter (Instance D)
use super::{
    apply_locale_env, descendant_pids, isolated_command, CapabilityReport, InstanceAdapter,
    ProcessHandle,
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
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use uuid::Uuid;

pub struct WebAdapter {
    user_data_dir: String,
    geo_profile: SharedGeoProfile,
    sessions: Arc<Mutex<HashMap<u32, String>>>,
}

impl WebAdapter {
    pub fn new_with_geo_profile(user_data_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self {
            user_data_dir,
            geo_profile,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
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
        args.push("https://claude.ai".to_string());

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
            if let Err(error) = apply_browser_profile(debug_port, &profile).await {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(root_pid as i32),
                    nix::sys::signal::Signal::SIGTERM,
                );
                let _ = std::fs::remove_dir_all(&session_dir);
                return Err(error.context("Failed to apply browser locale/timezone profile"));
            }

            // The Claude login/challenge flow can replace the document after
            // the first pass. Reapply once after that navigation settles so
            // navigator.language and the emulation overrides belong to the
            // final Claude page target.
            let delayed_profile = profile.clone();
            tokio::spawn(async move {
                for attempt in 1..=3 {
                    tokio::time::sleep(Duration::from_secs(if attempt == 1 { 3 } else { 2 })).await;
                    if !process_exists(root_pid) {
                        break;
                    }
                    match apply_browser_profile(debug_port, &delayed_profile).await {
                        Ok(()) => {
                            tracing::info!(
                                port = debug_port,
                                attempt,
                                "Browser profile reapplied after navigation"
                            );
                            break;
                        }
                        Err(error) => {
                            tracing::warn!(
                                port = debug_port,
                                attempt,
                                "Delayed browser profile reapply failed: {}",
                                error
                            );
                        }
                    }
                }
            });
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
    #[serde(rename = "type")]
    target_type: Option<String>,
    url: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
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

async fn apply_browser_profile(port: u16, profile: &GeoProfile) -> Result<()> {
    tracing::debug!(port, "Applying browser locale/timezone profile");
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{}/json/list", port);
    let target = tokio::time::timeout(Duration::from_secs(8), async {
        let started = Instant::now();
        loop {
            if let Ok(response) = client.get(&endpoint).send().await {
                if let Ok(targets) = response.json::<Vec<DevToolsTarget>>().await {
                    let claude_target = targets.iter().find(|target| {
                        target.target_type.as_deref() == Some("page")
                            && target.websocket_url.is_some()
                            && target
                                .url
                                .as_deref()
                                .map(|url| url.starts_with("https://claude.ai"))
                                .unwrap_or(false)
                    });
                    if let Some(target) = claude_target {
                        break target.clone();
                    }
                    // During hCaptcha/login transitions Chrome can briefly
                    // expose a page target with an empty URL. After the first
                    // few seconds it is safer to use that sole page target
                    // than to abandon the locale/geo profile entirely.
                    if started.elapsed() >= Duration::from_secs(4) {
                        if let Some(target) = targets.into_iter().find(|target| {
                            target.target_type.as_deref() == Some("page")
                                && target.websocket_url.is_some()
                        }) {
                            break target;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("Timed out waiting for Chrome DevTools")?;

    let websocket_url = target
        .websocket_url
        .context("Chrome did not expose a page DevTools endpoint")?;

    // Claude's login page can immediately create challenge frames and perform
    // a redirect. Wait for that first navigation to settle before applying
    // page-scoped overrides, otherwise the next document can reset them.
    tokio::time::sleep(Duration::from_secs(2)).await;

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

    if profile.language.is_some() {
        send_cdp_command(
            &mut socket,
            command_id,
            "Page.reload",
            json!({"ignoreCache": false}),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(language) = profile.language.as_deref() {
            if let Err(error) = send_cdp_command(
                &mut socket,
                command_id + 1,
                "Runtime.evaluate",
                json!({"expression": language_override_script(language)}),
            )
            .await
            {
                tracing::debug!(port, "Runtime language reapply deferred: {}", error);
            }
        }
    }

    tracing::debug!(port, "Browser locale/timezone profile applied");
    Ok(())
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
    socket
        .send(Message::Text(
            json!({"id": id, "method": method, "params": params}).to_string(),
        ))
        .await
        .context("Failed to send Chrome DevTools command")?;

    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(message) = socket.next().await {
            if let Ok(Message::Text(text)) = message {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                        if let Some(error) = value.get("error") {
                            anyhow::bail!("Chrome DevTools command {} failed: {}", method, error);
                        }
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Timed out waiting for Chrome DevTools response")??;

    Ok(())
}
