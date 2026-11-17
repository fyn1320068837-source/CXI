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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum WebSite {
    Claude,
    Gemini,
}

impl WebSite {
    fn origin(self) -> &'static str {
        match self {
            Self::Claude => "https://claude.ai",
            Self::Gemini => "https://gemini.google.com",
        }
    }
    fn entry_url(self) -> &'static str {
        match self {
            Self::Claude => "https://claude.ai",
            Self::Gemini => "https://gemini.google.com/app",
        }
    }
    fn log_key(self) -> &'static str {
        match self {
            Self::Claude => "web",
            Self::Gemini => "gemini-web",
        }
    }
    fn language_script(self, language: &str) -> String {
        if self == Self::Claude {
            return cdp::language_override_script(language);
        }
        let base = language.split('-').next().unwrap_or(language);
        format!("Object.defineProperty(Navigator.prototype, 'language', {{configurable: true, get: () => {:?}}}); Object.defineProperty(Navigator.prototype, 'languages', {{configurable: true, get: () => [{:?}, {:?}]}});", language, language, base)
    }
}

pub struct WebAdapter {
    site: WebSite,
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
    initial_target: Arc<Mutex<Option<String>>>,
}

impl WebAdapter {
    pub fn new_with_geo_profile(user_data_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self::for_site(user_data_dir, geo_profile, WebSite::Claude)
    }

    pub fn new_gemini(user_data_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self::for_site(user_data_dir, geo_profile, WebSite::Gemini)
    }

    fn for_site(user_data_dir: String, geo_profile: SharedGeoProfile, site: WebSite) -> Self {
        let profile_dir = format!("{user_data_dir}/profile");
        Self {
            site,
            user_data_dir,
            profile_dir,
            geo_profile,
            request_log_tx: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            debug_ports: Arc::new(Mutex::new(HashMap::new())),
            initial_target: Arc::new(Mutex::new(None)),
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
        anyhow::ensure!(
            self.site == WebSite::Claude,
            "Desktop authentication handoff is only supported by Claude Web"
        );
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
pub(crate) mod shared;

#[cfg(test)]
mod tests {
    use super::{
        cdp::{language_override_script, sanitize_request_url},
        validate_auth_url,
    };

    #[test]
    fn gemini_uses_its_own_entry_and_generic_language_overrides() {
        let geo = crate::geoip::shared_profile();
        let claude = super::WebAdapter::new_with_geo_profile("/fixture/web".into(), geo.clone());
        let gemini = super::WebAdapter::new_gemini("/fixture/gemini-web".into(), geo);
        assert_eq!(claude.profile_dir, "/fixture/web/profile");
        assert_eq!(gemini.profile_dir, "/fixture/gemini-web/profile");
        assert_eq!(gemini.site.entry_url(), "https://gemini.google.com/app");
        assert_eq!(gemini.site.log_key(), "gemini-web");
        let script = gemini.site.language_script("en-US");
        assert!(script.contains("Navigator.prototype"));
        assert!(!script.contains("claude.ai"));
        assert!(!script.contains("localStorage"));
        assert!(!script.contains("cookie"));
        assert!(claude
            .site
            .language_script("en-US")
            .contains("signin-locale"));
    }

    #[tokio::test]
    async fn gemini_cannot_receive_desktop_auth_handoff() {
        let adapter = super::WebAdapter::new_gemini(
            "/fixture/gemini-web".into(),
            crate::geoip::shared_profile(),
        );
        assert!(adapter
            .open_auth_url("https://claude.ai/login")
            .await
            .unwrap_err()
            .to_string()
            .contains("only supported by Claude Web"));
    }

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
