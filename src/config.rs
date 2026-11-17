// Configuration module - TOML config parsing
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;
mod instances;
pub use instances::InstanceConfigs;

fn legacy_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "legacy_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub instances: InstanceConfigs,
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

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.schema_version, 1 | 2),
            "unsupported config schema_version: {}",
            self.schema_version
        );
        self.instances.validate()
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let config_path = Self::config_path();

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        self.save_to_file(&config_path)?;
        info!("Config saved to {:?}", config_path);
        Ok(())
    }

    fn save_to_file(&self, path: &Path) -> Result<()> {
        self.validate()?;
        // Keep legacy upstream fields and extension tables not owned by Config.
        let mut document: toml::Value = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(error) => return Err(error.into()),
        };
        fn merge(base: &mut toml::Value, update: toml::Value) {
            match (base, update) {
                (toml::Value::Table(base), toml::Value::Table(update)) => {
                    for (key, value) in update {
                        if let Some(existing) = base.get_mut(&key) {
                            merge(existing, value);
                        } else {
                            base.insert(key, value);
                        }
                    }
                }
                (base, update) => *base = update,
            }
        }
        merge(&mut document, toml::Value::try_from(self)?);
        let content = toml::to_string_pretty(&document)?;
        let temporary = path.with_file_name(format!(".cim-config-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
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
            schema_version: 2,
            instances: InstanceConfigs::default(),
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
    fn legacy_load_is_read_only_and_save_preserves_unmodeled_fields() {
        let path =
            std::env::temp_dir().join(format!("cim-config-test-{}.toml", uuid::Uuid::new_v4()));
        let mut legacy = toml::Value::try_from(Config::default()).unwrap();
        legacy.as_table_mut().unwrap().remove("schema_version");
        legacy.as_table_mut().unwrap().remove("instances");
        legacy["proxy"].as_table_mut().unwrap().insert(
            "host".into(),
            toml::Value::String("proxy.example.test".into()),
        );
        legacy["proxy"]
            .as_table_mut()
            .unwrap()
            .insert("port".into(), toml::Value::Integer(1080));
        legacy.as_table_mut().unwrap().insert(
            "upstream".into(),
            toml::Value::Table(toml::from_str("username = 'fixture-user'").unwrap()),
        );
        let original = toml::to_string(&legacy).unwrap();
        std::fs::write(&path, &original).unwrap();
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.schema_version, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        for kind in crate::instance::InstanceKind::ALL {
            assert!(config.instances.get(kind).enabled);
            assert!(!config.instances.get(kind).autostart);
        }
        config.save_to_file(&path).unwrap();
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["proxy"]["host"], legacy["proxy"]["host"]);
        assert_eq!(saved["proxy"]["port"], legacy["proxy"]["port"]);
        assert_eq!(saved["upstream"], legacy["upstream"]);
        Config::load_from_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn future_schema_is_rejected() {
        let config = Config {
            schema_version: 999,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

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
