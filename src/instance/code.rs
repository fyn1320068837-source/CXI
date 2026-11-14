// Claude Code CLI adapter (Instance A)
use super::{
    apply_locale_env, descendant_pids, isolated_command, terminate_process_tree, CapabilityReport,
    InstanceAdapter, ProcessHandle, HTTP_PROXY_ENDPOINT, LOOPBACK_NO_PROXY, SOCKS_PROXY_ENDPOINT,
};
use crate::geoip::SharedGeoProfile;
use anyhow::Result;
use async_trait::async_trait;
use std::process::Command;

pub struct CodeAdapter {
    profile_dir: String,
    geo_profile: SharedGeoProfile,
}

impl CodeAdapter {
    pub fn new_with_geo_profile(profile_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self {
            profile_dir,
            geo_profile,
        }
    }
}

fn configure_code_command(
    command: &mut Command,
    profile_dir: &str,
    profile: &crate::geoip::GeoProfile,
) {
    command
        .env("HOME", profile_dir)
        .env("HTTPS_PROXY", HTTP_PROXY_ENDPOINT)
        .env("HTTP_PROXY", HTTP_PROXY_ENDPOINT)
        .env("ALL_PROXY", SOCKS_PROXY_ENDPOINT)
        .env("NO_PROXY", LOOPBACK_NO_PROXY);
    apply_locale_env(command, profile);
}

#[async_trait]
impl InstanceAdapter for CodeAdapter {
    async fn detect(&self) -> CapabilityReport {
        // Check if `claude` CLI is available
        let output = Command::new("which").arg("claude").output();

        match output {
            Ok(o) if o.status.success() => {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                // Try to get version
                let version = Command::new(&path)
                    .arg("--version")
                    .output()
                    .ok()
                    .and_then(|v| String::from_utf8(v.stdout).ok())
                    .map(|v| v.trim().to_string());

                CapabilityReport {
                    installed: true,
                    version,
                    path: Some(path),
                }
            }
            _ => CapabilityReport {
                installed: false,
                version: None,
                path: None,
            },
        }
    }

    async fn prepare(&self) -> Result<()> {
        // Create isolated HOME directory for Claude Code
        std::fs::create_dir_all(&self.profile_dir)?;
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        // Launch claude with custom HOME and proxy env vars
        let profile = self.geo_profile.read().await.clone();
        let executable = self
            .detect()
            .await
            .path
            .ok_or_else(|| anyhow::anyhow!("claude is not installed"))?;
        let mut command = isolated_command(executable);
        configure_code_command(&mut command, &self.profile_dir, &profile);
        let child = command.spawn()?;

        Ok(ProcessHandle {
            root_pid: child.id(),
        })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        terminate_process_tree(handle.root_pid).await;
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        descendant_pids(handle.root_pid)
    }
}

#[cfg(test)]
mod tests {
    use super::configure_code_command;
    use crate::geoip::GeoProfile;
    use std::process::Command;

    fn env_value(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == key)
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    #[test]
    fn code_command_uses_isolated_home_and_local_proxy() {
        let profile = GeoProfile {
            timezone: Some("America/New_York".to_string()),
            locale: Some("en_US.UTF-8".to_string()),
            ..GeoProfile::default()
        };
        let mut command = Command::new("claude");
        configure_code_command(&mut command, "/tmp/cim-code-home", &profile);
        assert_eq!(
            env_value(&command, "HOME").as_deref(),
            Some("/tmp/cim-code-home")
        );
        assert_eq!(
            env_value(&command, "HTTPS_PROXY").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            env_value(&command, "ALL_PROXY").as_deref(),
            Some("socks5://127.0.0.1:7891")
        );
        assert_eq!(
            env_value(&command, "NO_PROXY").as_deref(),
            Some("localhost,127.0.0.1")
        );
        assert_eq!(
            env_value(&command, "TZ").as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            env_value(&command, "LC_ALL").as_deref(),
            Some("en_US.UTF-8")
        );
    }
}
