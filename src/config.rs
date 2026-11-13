// Configuration module - TOML config parsing
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub proxy: ProxyConfig,
    pub verification: VerificationConfig,
    pub geoip: GeoIpConfig,
    pub firewall: FirewallConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub listen_addr: String,
    pub database_path: String,
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub sing_box_binary: String,
    pub sing_box_config: String,
    pub health_check_interval: u64,
    pub health_check_timeout: u64,
    pub exit_ip_endpoints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationConfig {
    pub interval: u64,
    pub allow_lan: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoIpConfig {
    pub provider: String,
    pub maxmind_db_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallConfig {
    pub anchor_name: String,
    pub rules_path: String,
    pub isolation_group: String,
}

impl Config {
    /// Load config from file, or create default if not exists
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        if config_path.exists() {
            Self::load_from_file(&config_path)
        } else {
            info!(
                "Config file not found, creating default at {:?}",
                config_path
            );
            let config = Self::default();
            config.save()?;
            Ok(config)
        }
    }

    /// Load config from specific file
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context(format!("Failed to read config file: {:?}", path))?;

        let config: Config = toml::from_str(&content).context("Failed to parse config file")?;

        Ok(config)
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, content)?;

        info!("Config saved to {:?}", config_path);
        Ok(())
    }

    /// Get default config file path
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").expect("HOME not set");
        PathBuf::from(home).join(".cim/config.toml")
    }

    /// Expand tilde in paths
    pub fn expand_path(path: &str) -> String {
        if path.starts_with("~/") {
            let home = std::env::var("HOME").expect("HOME not set");
            path.replacen("~", &home, 1)
        } else {
            path.to_string()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig {
                listen_addr: "127.0.0.1:8080".to_string(),
                database_path: "~/.cim/cim.db".to_string(),
                log_level: "info".to_string(),
            },
            proxy: ProxyConfig {
                sing_box_binary: "~/.cim/sing-box/sing-box".to_string(),
                sing_box_config: "~/.cim/sing-box/config.json".to_string(),
                health_check_interval: 5,
                health_check_timeout: 5,
                exit_ip_endpoints: vec![
                    "https://api.ipify.org".to_string(),
                    "https://ifconfig.me/ip".to_string(),
                    "https://icanhazip.com".to_string(),
                ],
            },
            verification: VerificationConfig {
                interval: 30,
                allow_lan: true,
            },
            geoip: GeoIpConfig {
                provider: "ip-api".to_string(),
                maxmind_db_path: None,
            },
            firewall: FirewallConfig {
                anchor_name: "com.cim".to_string(),
                rules_path: "/etc/pf.anchors/com.cim".to_string(),
                isolation_group: "cimgrp".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.daemon.listen_addr, "127.0.0.1:8080");
        assert_eq!(config.proxy.health_check_interval, 5);
    }

    #[test]
    fn test_expand_path() {
        std::env::set_var("HOME", "/Users/test");
        assert_eq!(Config::expand_path("~/test"), "/Users/test/test");
        assert_eq!(Config::expand_path("/absolute/path"), "/absolute/path");
    }
}
