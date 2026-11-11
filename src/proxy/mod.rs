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

    /// Generate sing-box config
    async fn generate_config(&self) -> Result<()> {
        use std::fs;

        // For now, generate a test config with direct outbound
        // In production, this should read proxy credentials from Keychain
        let config = serde_json::json!({
            "log": {
                "level": "warn"
            },
            "dns": {
                "servers": [{
                    "tag": "remote",
                    "address": "https://1.1.1.1/dns-query",
                    "detour": "upstream"
                }],
                "strategy": "ipv4_only"
            },
            "inbounds": [
                {
                    "type": "http",
                    "tag": "http-in",
                    "listen": "127.0.0.1",
                    "listen_port": 7890
                },
                {
                    "type": "socks",
                    "tag": "socks-in",
                    "listen": "127.0.0.1",
                    "listen_port": 7891
                }
            ],
            "outbounds": [{
                "type": "direct",
                "tag": "upstream"
            }],
            "route": {
                "final": "upstream"
            }
        });

        // Ensure parent directory exists
        if let Some(parent) = self.config.config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write config
        fs::write(&self.config.config_path, config.to_string())?;
        info!("Generated sing-box config at {:?}", self.config.config_path);

        Ok(())
    }
}

impl Drop for ProxyManager {
    fn drop(&mut self) {
        if let Some(mut child) = self.sing_box_process.take() {
            let _ = child.kill();
        }
    }
}
