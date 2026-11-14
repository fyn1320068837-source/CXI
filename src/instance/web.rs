// Claude Web in Chromium adapter (Instance D)
use super::{
    apply_locale_env, descendant_pids, isolated_command, terminate_process_tree, CapabilityReport,
    InstanceAdapter, ProcessHandle, RequestLogEntry, RequestLogSender,
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

mod cdp;
mod lifecycle;

#[cfg(test)]
mod tests {
    use super::cdp::{language_override_script, sanitize_request_url};

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
        assert_eq!(sanitize_request_url("file:///tmp/secret"), "");
    }

    #[test]
    fn language_script_sets_primary_and_base_languages() {
        let script = language_override_script("zh-CN");
        assert!(script.contains("zh-CN"));
        assert!(script.contains("zh"));
        assert!(script.contains("Navigator.prototype"));
    }
}
