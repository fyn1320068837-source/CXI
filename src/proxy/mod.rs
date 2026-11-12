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
const HEALTH_FAILURE_THRESHOLD: u32 = 3;

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

    /// Start sing-box process
    pub async fn start(&self) -> Result<()> {
        self.mark_unhealthy();
        {
            let mut process = self.sing_box_process.lock().unwrap();
            if let Some(child) = process.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        warn!("sing-box already running");
                        return Ok(());
                    }
                    Ok(Some(_)) => {
                        *process = None;
                    }
                    Err(error) => {
                        warn!("Could not check existing sing-box status: {}", error);
                        *process = None;
                    }
                }
            }
        }

        info!("Starting sing-box from {:?}", self.config.binary_path);

        // Generate config
        self.generate_config().await?;

        // Start sing-box, capture stderr for diagnostics
        let child = Command::new(&self.config.binary_path)
            .arg("run")
            .arg("-c")
            .arg(&self.config.config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start sing-box")?;

        info!("sing-box started with PID: {}", child.id());
        *self.sing_box_process.lock().unwrap() = Some(child);

        // Wait longer for sing-box to initialize with upstream proxy
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify it's still alive
        let mut process = self.sing_box_process.lock().unwrap();
        if let Some(child) = process.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    *process = None;
                    anyhow::bail!("sing-box exited immediately with status: {}", status);
                }
                Ok(None) => {
                    info!("sing-box is running and stable");
                    // The process is stable, but the proxy is not considered
                    // healthy until an exit-IP probe succeeds.
                }
                Err(e) => {
                    warn!("Could not check sing-box status: {}", e);
                }
            }
        }

        Ok(())
    }

    /// Stop sing-box process
    pub async fn stop(&self) -> Result<()> {
        self.mark_unhealthy();
        let child = self.sing_box_process.lock().unwrap().take();
        if let Some(mut child) = child {
            info!("Stopping sing-box PID: {}", child.id());

            // Try graceful shutdown first
            let _ = child.kill();

            // Wait for process to exit
            match tokio::time::timeout(Duration::from_secs(5), async {
                tokio::task::spawn_blocking(move || child.wait()).await
            })
            .await
            {
                Ok(_) => info!("sing-box stopped gracefully"),
                Err(_) => warn!("sing-box did not stop in time"),
            }
        }

        self.clear_geo_profile().await;

        Ok(())
    }

    /// Check the process that this manager started, without matching unrelated
    /// sing-box processes on the host.
    pub fn is_running(&self) -> bool {
        let mut process = self.sing_box_process.lock().unwrap();
        if let Some(child) = process.as_mut() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    warn!("sing-box exited with status: {}", status);
                    *process = None;
                    self.mark_unhealthy();
                    self.clear_geo_profile_sync();
                    false
                }
                Err(error) => {
                    error!("Error checking sing-box status: {}", error);
                    self.mark_unhealthy();
                    self.clear_geo_profile_sync();
                    false
                }
            }
        } else {
            false
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.proxy_healthy.load(Ordering::Acquire) && self.is_running()
    }

    pub fn mark_healthy(&self) {
        self.proxy_healthy.store(true, Ordering::Release);
        if let Err(error) = self.firewall.set_allowed() {
            warn!(
                "Failed to clear PF fail-closed rules after proxy recovery: {}",
                error
            );
        }
    }

    pub fn mark_unhealthy(&self) {
        self.proxy_healthy.store(false, Ordering::Release);
        if let Err(error) = self.firewall.set_blocked() {
            warn!("Failed to enable PF fail-closed rules: {}", error);
        }
    }

    pub async fn restart(&self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }

    /// Check exit IP through proxy using curl (more reliable for SOCKS5)
    pub async fn check_exit_ip(&self) -> Result<String> {
        let output = tokio::process::Command::new("curl")
            .args([
                "-s",
                "-x",
                "socks5://127.0.0.1:7891",
                "--max-time",
                &self.config.health_check_timeout.to_string(),
                "https://ipv4.icanhazip.com",
            ])
            .output()
            .await
            .context("Failed to run curl")?;

        if output.status.success() {
            let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !ip.is_empty() {
                info!("Exit IP: {}", ip);
                return Ok(ip);
            }
        }

        // Fallback endpoints
        for endpoint in &self.config.exit_ip_endpoints {
            let result = tokio::process::Command::new("curl")
                .args([
                    "-s",
                    "-x",
                    "socks5://127.0.0.1:7891",
                    "--max-time",
                    &self.config.health_check_timeout.to_string(),
                    endpoint.as_str(),
                ])
                .output()
                .await;

            if let Ok(out) = result {
                if out.status.success() {
                    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !ip.is_empty() && ip.parse::<IpAddr>().is_ok() {
                        info!("Exit IP: {}", ip);
                        return Ok(ip);
                    }
                }
            }
        }

        anyhow::bail!("All exit IP endpoints failed")
    }

    /// Resolve the current exit IP and update the shared locale profile.
    pub async fn refresh_geo_profile(&self) -> Result<GeoProfile> {
        let exit_ip = self.check_exit_ip().await?;
        self.refresh_geo_profile_for_ip(&exit_ip).await
    }

    pub async fn refresh_geo_profile_for_ip(&self, exit_ip: &str) -> Result<GeoProfile> {
        let ip = parse_exit_ip(exit_ip)?;
        let ip_text = ip.to_string();
        let now = Instant::now();

        let cached_profile = {
            let cache = self.geo_cache.lock().unwrap();
            cache.entry.as_ref().and_then(|entry| {
                if entry.profile.source_ip.as_deref() == Some(ip_text.as_str())
                    && now.duration_since(entry.fetched_at) < GEO_CACHE_TTL
                {
                    Some(entry.profile.clone())
                } else {
                    None
                }
            })
        };
        if let Some(profile) = cached_profile {
            *self.geo_profile.write().await = profile.clone();
            return Ok(profile);
        }

        let rate_limited = {
            let cache = self.geo_cache.lock().unwrap();
            cache
                .last_attempt
                .map(|(last_ip, last_attempt)| {
                    last_ip == ip && now.duration_since(last_attempt) < GEO_RETRY_INTERVAL
                })
                .unwrap_or(false)
        };
        if rate_limited {
            let current = self.geo_profile.read().await.clone();
            if current.source_ip.as_deref() == Some(ip_text.as_str()) && current.has_geo() {
                return Ok(current);
            }
            anyhow::bail!("GeoIP lookup is rate limited for {}", ip);
        }

        self.geo_cache.lock().unwrap().last_attempt = Some((ip, now));

        {
            let mut profile = self.geo_profile.write().await;
            reset_profile_if_ip_changed(&mut profile, &ip_text);
        }

        let provider = IpApiProvider::new()?;
        match provider.resolve(ip).await {
            Ok(record) => {
                let profile = GeoProfile::from_record(ip, record);
                self.geo_cache.lock().unwrap().entry = Some(GeoCacheEntry {
                    profile: profile.clone(),
                    fetched_at: Instant::now(),
                });
                *self.geo_profile.write().await = profile.clone();
                Ok(profile)
            }
            Err(error) => {
                // Keep a previously resolved profile for the same exit IP so
                // a transient GeoIP outage does not reset new instances to
                // the host locale. A changed IP was already reset above.
                warn!("GeoIP resolution failed for {}: {}", ip, error);
                Err(error)
            }
        }
    }

    async fn clear_geo_profile(&self) {
        *self.geo_profile.write().await = GeoProfile::default();
    }

    fn clear_geo_profile_sync(&self) {
        if let Ok(mut profile) = self.geo_profile.try_write() {
            *profile = GeoProfile::default();
        }
    }

    /// Health check loop
    pub async fn health_check_loop(&self) {
        let mut check_interval = interval(Duration::from_secs(self.config.health_check_interval));
        let mut consecutive_failures = 0;

        loop {
            check_interval.tick().await;

            // Check if process is still running
            if !self.is_running() {
                self.mark_unhealthy();
                self.clear_geo_profile().await;
                error!("sing-box process died, triggering fail-closed");
                continue;
            }

            // Check connectivity
            match self.check_exit_ip().await {
                Ok(ip) => {
                    self.mark_healthy();
                    info!("Health check passed, exit IP: {}", ip);
                    if let Err(error) = self.refresh_geo_profile_for_ip(&ip).await {
                        warn!("GeoIP resolution failed for {}: {}", ip, error);
                    }
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!("Health check failed ({}): {}", consecutive_failures, e);

                    if consecutive_failures >= HEALTH_FAILURE_THRESHOLD {
                        self.mark_unhealthy();
                        self.clear_geo_profile().await;
                        error!(
                            "Health check failed {} times, triggering fail-closed",
                            HEALTH_FAILURE_THRESHOLD
                        );
                        consecutive_failures = 0;
                    }
                }
            }
        }
    }

    /// Generate sing-box config with real upstream proxy
    async fn generate_config(&self) -> Result<()> {
        use std::fs;

        // 读取 config.toml 中的代理设置
        let home = std::env::var("HOME").unwrap_or_default();
        let toml_path = format!("{}/.cim/config.toml", home);
        let toml_content = fs::read_to_string(&toml_path).unwrap_or_default();

        let upstream_host = extract_toml_val(&toml_content, "host").unwrap_or_default();
        let upstream_port: u16 = extract_toml_val(&toml_content, "port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let upstream_user = extract_toml_val(&toml_content, "username").unwrap_or_default();

        // Prefer the already generated 0600 sing-box config. Reading Keychain
        // through `/usr/bin/security` can trigger a macOS authorization prompt
        // on every launchd restart when the binary is rebuilt or unsigned. The
        // config is written with 0600 permissions below, so Keychain is only
        // needed for the first migration or when the config has no password.
        let upstream_pass =
            get_existing_upstream_password(&self.config.config_path, &upstream_host)
                .or_else(|| get_keychain_password("com.cim.proxy", &upstream_host))
                .unwrap_or_default();

        // 决定出站类型
        let outbound = if !upstream_host.is_empty() && upstream_port > 0 {
            info!(
                "Configuring sing-box upstream: socks5://{}:{}",
                upstream_host, upstream_port
            );
            if !upstream_user.is_empty() {
                serde_json::json!({
                    "type": "socks",
                    "tag": "upstream",
                    "server": upstream_host,
                    "server_port": upstream_port,
                    "username": upstream_user,
                    "password": upstream_pass,
                    "version": "5"
                })
            } else {
                serde_json::json!({
                    "type": "socks",
                    "tag": "upstream",
                    "server": upstream_host,
                    "server_port": upstream_port,
                    "version": "5"
                })
            }
        } else {
            anyhow::bail!(
                "No upstream SOCKS5 proxy configured; refusing to start a direct outbound"
            );
        };

        let config = serde_json::json!({
            "log": { "level": "warn" },
            // Resolve sing-box and health-check domains through DoH over the
            // upstream SOCKS tunnel. Browser DNS is separately constrained by
            // Chrome's host-resolver rules and SOCKS5 remote resolution.
            "dns": {
                "servers": [{
                    "tag": "remote",
                    "address": "https://1.1.1.1/dns-query",
                    "detour": "upstream"
                }],
                "strategy": "ipv4_only",
                "final": "remote"
            },
            "inbounds": [
                { "type": "http",  "tag": "http-in",  "listen": "127.0.0.1", "listen_port": 7890 },
                { "type": "socks", "tag": "socks-in", "listen": "127.0.0.1", "listen_port": 7891 }
            ],
            "outbounds": [outbound],
            "route": { "final": "upstream" }
        });

        if let Some(parent) = self.config.config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write config
        fs::write(&self.config.config_path, config.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.config.config_path, fs::Permissions::from_mode(0o600))?;
        }
        info!("Generated sing-box config at {:?}", self.config.config_path);

        Ok(())
    }
}

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
