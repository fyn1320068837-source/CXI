// Claude Code CLI adapter (Instance A)
use super::{
    apply_locale_env, descendant_pids, isolated_command, resolve_executable,
    terminate_process_tree, terminate_processes, CapabilityReport, InstanceAdapter, ProcessHandle,
    HTTP_PROXY_ENDPOINT, LOOPBACK_NO_PROXY, SOCKS_PROXY_ENDPOINT,
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

fn parse_processes_with_home(output: &str, profile_dir: &str) -> Vec<u32> {
    let marker = format!("HOME={profile_dir}");
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            command
                .split_whitespace()
                .any(|argument| argument == marker)
                .then(|| pid.parse::<u32>().ok())
                .flatten()
        })
        .collect()
}

fn processes_with_home(profile_dir: &str) -> Vec<u32> {
    let Ok(output) = Command::new("ps")
        .args(["-eww", "-axo", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    parse_processes_with_home(&String::from_utf8_lossy(&output.stdout), profile_dir)
}

async fn terminate_code_processes(root_pid: u32, profile_dir: &str) {
    let marker = profile_dir.to_owned();
    let marker_pids = tokio::task::spawn_blocking(move || processes_with_home(&marker))
        .await
        .unwrap_or_default();
    terminate_process_tree(root_pid).await;
    if !marker_pids.is_empty() {
        terminate_processes(&marker_pids).await;
    }
}

#[async_trait]
impl InstanceAdapter for CodeAdapter {
    async fn detect(&self) -> CapabilityReport {
        // Check if `claude` CLI is available
        match resolve_executable(&[
            "claude",
            "/opt/homebrew/bin/claude",
            "/usr/local/bin/claude",
        ]) {
            Some(path) => {
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
            None => CapabilityReport {
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
        terminate_code_processes(handle.root_pid, &self.profile_dir).await;
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        let profile_dir = self.profile_dir.clone();
        let marker_pids = tokio::task::spawn_blocking(move || processes_with_home(&profile_dir))
            .await
            .unwrap_or_default();
        let mut pids = descendant_pids(handle.root_pid);
        pids.extend(marker_pids);
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

#[cfg(test)]
mod tests {
    use super::{configure_code_command, parse_processes_with_home};
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

    #[test]
    fn home_marker_matches_only_the_code_profile() {
        let profile = "/tmp/cim-code-home";
        let ps = format!(
            " 101 claude HOME={profile} TZ=America/New_York\n 102 helper HOME={profile}-other\n"
        );
        assert_eq!(parse_processes_with_home(&ps, profile), vec![101]);
    }
}
