// Proxy Manager - manages sing-box lifecycle and health checks
use anyhow::{Result, Context};
use std::process::{Child, Command, Stdio};
use std::path::PathBuf;
use tokio::time::{interval, Duration};
use tracing::{info, warn, error};

pub struct ProxyManager {
    sing_box_process: Option<Child>,
    config: ProxyConfig,
}

impl Clone for ProxyManager {
    fn clone(&self) -> Self {
        Self {
            sing_box_process: None,
            config: self.config.clone(),
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
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            sing_box_process: None,
            config,
        }
    }

    /// Start sing-box process
    pub async fn start(&mut self) -> Result<()> {
        if self.sing_box_process.is_some() {
            warn!("sing-box already running");
            return Ok(());
        }

        info!("Starting sing-box from {:?}", self.config.binary_path);

        // Generate config if needed
        self.generate_config().await?;

        // Start sing-box
        let child = Command::new(&self.config.binary_path)
            .arg("run")
            .arg("-c")
            .arg(&self.config.config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start sing-box")?;

        info!("sing-box started with PID: {}", child.id());
        self.sing_box_process = Some(child);

        // Wait for sing-box to initialize
        tokio::time::sleep(Duration::from_secs(2)).await;

        Ok(())
    }

    /// Stop sing-box process
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.sing_box_process.take() {
            info!("Stopping sing-box PID: {}", child.id());

            // Try graceful shutdown first
            let _ = child.kill();

            // Wait for process to exit
            match tokio::time::timeout(Duration::from_secs(5), async {
                tokio::task::spawn_blocking(move || child.wait()).await
            }).await {
                Ok(_) => info!("sing-box stopped gracefully"),
                Err(_) => warn!("sing-box did not stop in time"),
            }
        }

        Ok(())
    }

    /// Check if sing-box is running
    pub fn is_running(&mut self) -> bool {
        if let Some(child) = &mut self.sing_box_process {
            match child.try_wait() {
                Ok(None) => true,  // Still running
                Ok(Some(status)) => {
                    warn!("sing-box exited with status: {}", status);
                    self.sing_box_process = None;
                    false
                }
                Err(e) => {
                    error!("Error checking sing-box status: {}", e);
                    false
                }
            }
        } else {
            false
        }
    }

    /// Check exit IP through proxy
    pub async fn check_exit_ip(&self) -> Result<String> {
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::http("http://127.0.0.1:7890")?)
            .timeout(Duration::from_secs(self.config.health_check_timeout))
            .build()?;

        // Try each endpoint until one succeeds
        for endpoint in &self.config.exit_ip_endpoints {
            match client.get(endpoint).send().await {
                Ok(resp) => {
                    if let Ok(ip) = resp.text().await {
                        let ip = ip.trim().to_string();
                        info!("Exit IP: {}", ip);
                        return Ok(ip);
                    }
                }
                Err(e) => {
                    warn!("Failed to check exit IP at {}: {}", endpoint, e);
                }
            }
        }

        anyhow::bail!("All exit IP endpoints failed")
    }

    /// Health check loop
    pub async fn health_check_loop(&mut self) -> Result<()> {
        let mut check_interval = interval(Duration::from_secs(self.config.health_check_interval));
        let mut consecutive_failures = 0;

        loop {
            check_interval.tick().await;

            // Check if process is still running
            if !self.is_running() {
                error!("sing-box process died, triggering fail-closed");
                // TODO: Trigger fail-closed
                break;
            }

            // Check connectivity
            match self.check_exit_ip().await {
                Ok(ip) => {
                    info!("Health check passed, exit IP: {}", ip);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!("Health check failed ({}): {}", consecutive_failures, e);

                    if consecutive_failures >= 2 {
                        error!("Health check failed 2 times, triggering fail-closed");
                        // TODO: Trigger fail-closed
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Generate sing-box config with real upstream proxy
    async fn generate_config(&self) -> Result<()> {
        use std::fs;

        // 读取 config.toml 中的代理设置
        let home = std::env::var("HOME").unwrap_or_default();
        let toml_path = format!("{}/.cim/config.toml", home);
        let toml_content = fs::read_to_string(&toml_path).unwrap_or_default();

        let upstream_host     = extract_toml_val(&toml_content, "host").unwrap_or_default();
        let upstream_port: u16 = extract_toml_val(&toml_content, "port")
            .and_then(|v| v.parse().ok()).unwrap_or(0);
        let upstream_user     = extract_toml_val(&toml_content, "username").unwrap_or_default();

        // 从 Keychain 读取密码
        let upstream_pass = get_keychain_password("com.cim.proxy", &upstream_host)
            .unwrap_or_default();

        // 决定出站类型
        let outbound = if !upstream_host.is_empty() && upstream_port > 0 {
            info!("Configuring sing-box upstream: socks5://{}:{}", upstream_host, upstream_port);
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
            warn!("No upstream proxy configured, using direct outbound");
            serde_json::json!({ "type": "direct", "tag": "upstream" })
        };

        let config = serde_json::json!({
            "log": { "level": "warn" },
            "dns": {
                "servers": [{
                    "tag": "remote",
                    "address": "https://1.1.1.1/dns-query",
                    "detour": "upstream"
                }],
                "strategy": "ipv4_only"
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
        info!("Generated sing-box config at {:?}", self.config.config_path);

        Ok(())
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
        if !in_proxy { continue; }
        // 只在 [proxy] section 里匹配
        let prefix_space = format!("{} =", key);
        let prefix_eq    = format!("{}=", key);
        if t.starts_with(&prefix_space) || t.starts_with(&prefix_eq) {
            if let Some(val) = t.splitn(2, '=').nth(1) {
                let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
                if !val.is_empty() { return Some(val); }
            }
        }
    }
    None
}

fn get_keychain_password(service: &str, account: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(&["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .ok()?;
        if output.status.success() {
            let pwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !pwd.is_empty() { return Some(pwd); }
        }
    }
    None
}

impl Drop for ProxyManager {
    fn drop(&mut self) {
        if let Some(mut child) = self.sing_box_process.take() {
            let _ = child.kill();
        }
    }
}
