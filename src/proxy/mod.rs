// Proxy Manager - manages sing-box lifecycle and health checks
use crate::firewall::FirewallController;
use crate::geoip::{GeoProfile, GeoProvider, IpApiProvider, SharedGeoProfile};
use anyhow::{Context, Result};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration as StdDuration, Instant};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

const GEO_CACHE_TTL: StdDuration = StdDuration::from_secs(10 * 60);
const GEO_RETRY_INTERVAL: StdDuration = StdDuration::from_secs(30);
// Fail closed on the first failed exit-IP probe. A healthy SOCKS listener is
// not enough: instances must never keep an allowed PF window after the
// upstream proxy stops carrying traffic.
const HEALTH_FAILURE_THRESHOLD: u32 = 1;

#[derive(Clone)]
struct GeoCacheEntry {
    profile: GeoProfile,
    fetched_at: Instant,
}

#[derive(Default)]
struct GeoCache {
    entry: Option<GeoCacheEntry>,
    last_attempt: Option<(IpAddr, Instant)>,
}

pub struct ProxyManager {
    sing_box_process: Arc<Mutex<Option<Child>>>,
    config: ProxyConfig,
    geo_profile: SharedGeoProfile,
    geo_cache: Arc<Mutex<GeoCache>>,
    proxy_healthy: Arc<AtomicBool>,
    firewall: Arc<FirewallController>,
}

impl Clone for ProxyManager {
    fn clone(&self) -> Self {
        Self {
            sing_box_process: self.sing_box_process.clone(),
            config: self.config.clone(),
            geo_profile: self.geo_profile.clone(),
            geo_cache: self.geo_cache.clone(),
            proxy_healthy: self.proxy_healthy.clone(),
            firewall: self.firewall.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ProxyConfig {
    pub binary_path: PathBuf,
    pub config_path: PathBuf,
    pub health_check_interval: u64,
    pub health_check_timeout: u64,
    pub exit_ip_endpoints: Vec<String>,
}

impl ProxyManager {
    pub fn with_geo_profile_and_firewall(
        config: ProxyConfig,
        geo_profile: SharedGeoProfile,
        firewall: Arc<FirewallController>,
    ) -> Self {
        Self {
            sing_box_process: Arc::new(Mutex::new(None)),
            config,
            geo_profile,
            geo_cache: Arc::new(Mutex::new(GeoCache::default())),
            proxy_healthy: Arc::new(AtomicBool::new(false)),
            firewall,
        }
    }

    pub fn firewall_status(&self) -> crate::firewall::FirewallStatus {
        self.firewall.status()
    }
}

mod config;
mod health;
mod lifecycle;

fn parse_exit_ip(value: &str) -> Result<IpAddr> {
    value
        .trim()
        .parse()
        .context("Exit IP response was not a valid IP address")
}

fn reset_profile_if_ip_changed(profile: &mut GeoProfile, ip: &str) {
    if profile.source_ip.as_deref() != Some(ip) {
        *profile = GeoProfile {
            source_ip: Some(ip.to_string()),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..GeoProfile::default()
        };
    }
}

fn extract_toml_val(content: &str, key: &str) -> Option<String> {
    let mut in_proxy = false;
    for line in content.lines() {
        let t = line.trim();
        // 进入/离开 section
        if t.starts_with('[') {
            in_proxy = t == "[proxy]";
            continue;
        }
        if !in_proxy {
            continue;
        }
        // 只在 [proxy] section 里匹配
        let prefix_space = format!("{} =", key);
        let prefix_eq = format!("{}=", key);
        if t.starts_with(&prefix_space) || t.starts_with(&prefix_eq) {
            if let Some((_, val)) = t.split_once('=') {
                let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}

fn get_keychain_password(service: &str, account: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut child = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        loop {
            match child.try_wait().ok()? {
                Some(status) => {
                    if status.success() {
                        let output = child.wait_with_output().ok()?;
                        let pwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !pwd.is_empty() {
                            return Some(pwd);
                        }
                    }
                    return None;
                }
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                None => std::thread::sleep(StdDuration::from_millis(50)),
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn get_existing_upstream_password(config_path: &PathBuf, host: &str) -> Option<String> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;
    config
        .get("outbounds")?
        .as_array()?
        .iter()
        .find(|outbound| {
            outbound.get("tag").and_then(|value| value.as_str()) == Some("upstream")
                && outbound.get("server").and_then(|value| value.as_str()) == Some(host)
        })?
        .get("password")
        .and_then(|value| value.as_str())
        .filter(|password| !password.is_empty())
        .map(ToOwned::to_owned)
}

impl Drop for ProxyManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.sing_box_process) == 1 {
            if let Some(mut child) = self.sing_box_process.lock().unwrap().take() {
                let _ = child.kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_exit_ip, reset_profile_if_ip_changed};
    use crate::geoip::GeoProfile;

    #[test]
    fn accepts_ipv4_and_ipv6_exit_addresses() {
        assert_eq!(
            parse_exit_ip(" 203.0.113.5\n").unwrap().to_string(),
            "203.0.113.5"
        );
        assert_eq!(
            parse_exit_ip("2001:db8::42").unwrap().to_string(),
            "2001:db8::42"
        );
        assert!(parse_exit_ip("not-an-ip").is_err());
    }

    #[test]
    fn changing_exit_ip_discards_stale_geo_but_same_ip_preserves_it() {
        let mut profile = GeoProfile {
            source_ip: Some("203.0.113.5".to_string()),
            country: Some("Example".to_string()),
            timezone: Some("Etc/UTC".to_string()),
            ..GeoProfile::default()
        };
        reset_profile_if_ip_changed(&mut profile, "203.0.113.5");
        assert_eq!(profile.country.as_deref(), Some("Example"));

        reset_profile_if_ip_changed(&mut profile, "2001:db8::42");
        assert_eq!(profile.source_ip.as_deref(), Some("2001:db8::42"));
        assert!(profile.country.is_none());
        assert!(profile.timezone.is_none());
    }
}
