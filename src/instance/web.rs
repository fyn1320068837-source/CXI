// Claude Web in Chromium adapter (Instance D)
use super::{
    apply_locale_env, descendant_pids, isolated_command, CapabilityReport, InstanceAdapter,
    ProcessHandle, RequestLogEntry, RequestLogSender,
};
use crate::geoip::{GeoProfile, SharedGeoProfile};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::hash::Hash;
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
    profile_dir: String,
    geo_profile: SharedGeoProfile,
    request_log_tx: Option<RequestLogSender>,
    sessions: Arc<Mutex<HashMap<u32, String>>>,
    /// DevTools ports for the active CIM Web browser roots.  The Web runtime
    /// is single-instance today, but keeping this keyed by PID makes the
    /// ownership explicit and avoids ever routing an auth URL to an unrelated
    /// Chrome profile.
    debug_ports: Arc<Mutex<HashMap<u32, u16>>>,
}

impl WebAdapter {
    pub fn new_with_geo_profile(user_data_dir: String, geo_profile: SharedGeoProfile) -> Self {
        let profile_dir = format!("{user_data_dir}/profile");
        Self {
            user_data_dir,
            profile_dir,
            geo_profile,
            request_log_tx: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            debug_ports: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set_request_log_sender(&mut self, sender: RequestLogSender) {
        self.request_log_tx = Some(sender);
    }

    pub async fn cleanup_orphaned_sessions(&self) -> anyhow::Result<()> {
        lifecycle::cleanup_orphaned_sessions(self).await
    }

    /// Navigate the currently running CIM Web profile to a Claude/Google
    /// authentication URL.  Desktop OAuth is initiated by Electron through
    /// macOS LaunchServices, which otherwise opens the system default browser
    /// outside CIM.  This narrow bridge lets the daemon route that URL into
    /// the already isolated Web profile without copying cookies or tokens.
    pub async fn open_auth_url(&self, raw_url: &str) -> Result<()> {
        let url = validate_auth_url(raw_url)?;

        let port = self
            .debug_ports
            .lock()
            .unwrap()
            .values()
            .next()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("CIM Web is not running"))?;
        cdp::navigate_browser(port, url.as_str()).await
    }
}

fn validate_auth_url(raw_url: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw_url).context("invalid authentication URL")?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("authentication URL must be an HTTPS URL without credentials");
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let allowed = matches!(
        host.as_str(),
        "claude.ai"
            | "www.claude.ai"
            | "accounts.google.com"
            | "accounts.googleusercontent.com"
            | "auth.anthropic.com"
    );
    if !allowed {
        anyhow::bail!("authentication URL host is not allowlisted");
    }
    Ok(url)
}

mod cdp;
mod lifecycle;

#[cfg(test)]
mod tests {
    use super::{
        cdp::{language_override_script, sanitize_request_url},
        validate_auth_url,
    };

    #[test]
    fn request_log_urls_drop_query_values() {
        assert_eq!(
            sanitize_request_url("https://claude.ai/api/messages?token=secret"),
            "https://claude.ai/api/messages"
        );
        assert_eq!(
            sanitize_request_url("https://claude.ai/login"),
            "https://claude.ai/[redacted]"
        );
        assert_eq!(
            sanitize_request_url("https://user:secret@claude.ai/api/messages?token=secret#x"),
            "https://claude.ai/api/messages"
        );
        assert_eq!(
            sanitize_request_url(
                "https://claude.ai/cdn-cgi/challenge-platform/h/g/fo/630164355:1794546913:IV11Q8FZaCUGOkdXEY6HJV7-o7ppiJJ-DpJt1-Wmytc/a"
            ),
            "https://claude.ai/cdn-cgi/challenge-platform/h/g/fo/[redacted]/a"
        );
        assert_eq!(
            sanitize_request_url("https://claude.ai/sso/callback/short-code"),
            "https://claude.ai/[redacted]"
        );
        assert_eq!(sanitize_request_url("file:///tmp/secret"), "");
    }

    #[test]
    fn language_script_sets_primary_and_base_languages() {
        let script = language_override_script("zh-CN");
        assert!(script.contains("zh-CN"));
        assert!(script.contains("zh"));
        assert!(script.contains("spa:locale"));
        assert!(script.contains("zh-Hans"));
        assert!(script.contains("signin-locale"));
        assert!(script.contains("Navigator.prototype"));
    }

    #[test]
    fn auth_bridge_allowlists_https_login_hosts_without_logging_secrets() {
        let url = validate_auth_url(
            "https://accounts.google.com/o/oauth2/auth?state=secret&code_challenge=secret",
        )
        .expect("Google OAuth URL should be accepted");
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert!(validate_auth_url("http://accounts.google.com/login").is_err());
        assert!(validate_auth_url("https://evil.example/login").is_err());
        assert!(validate_auth_url("https://user:pass@claude.ai/login").is_err());
    }
}
