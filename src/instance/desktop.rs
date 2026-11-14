// Claude Desktop adapter (Instance B) - Desktop Lite host isolation.
//
// The Lite backend deliberately launches the app binary directly instead of
// using LaunchServices (`open -a`). This lets the root-owned CIM launcher add
// cimgrp and lets us inject an isolated HOME, proxy and locale environment.
// It is a network-hardened best-effort backend; CoreLocation remains outside
// the guarantees of this adapter and is reported by the API layer as partial.
use super::{
    apply_locale_env, descendant_pids, isolated_command, resolve_executable,
    terminate_process_tree, terminate_processes, CapabilityReport, InstanceAdapter, ProcessHandle,
    HTTP_PROXY_ENDPOINT, LOOPBACK_NO_PROXY, SOCKS_PROXY_ENDPOINT,
};
use crate::geoip::SharedGeoProfile;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::process::{Command, Stdio};

const CLAUDE_APP_PATH: &str = "/Applications/Claude.app";
const CLAUDE_BINARY_PATH: &str = "/Applications/Claude.app/Contents/MacOS/Claude";

pub struct DesktopAdapter {
    app_path: String,
    profile_dir: String,
    geo_profile: SharedGeoProfile,
}

impl DesktopAdapter {
    /// Backwards-compatible constructor for callers that used the old VM
    /// toggle. Desktop Lite is now the only local backend; the boolean is
    /// intentionally ignored and a fresh runtime GeoIP profile is used.
    #[allow(dead_code)]
    pub fn new(_use_vm: bool) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self::new_with_geo_profile(
            format!("{home}/.cim/instances/desktop"),
            crate::geoip::shared_profile(),
        )
    }

    pub fn new_with_geo_profile(profile_dir: String, geo_profile: SharedGeoProfile) -> Self {
        Self {
            app_path: CLAUDE_APP_PATH.to_string(),
            profile_dir,
            geo_profile,
        }
    }

    fn executable(&self) -> Option<String> {
        resolve_executable(&[CLAUDE_BINARY_PATH])
    }
}

fn configure_desktop_command(
    command: &mut Command,
    profile_dir: &str,
    profile: &crate::geoip::GeoProfile,
) {
    let user_data_dir = format!("{profile_dir}/user-data");
    let config_dir = format!("{profile_dir}/config");
    let cache_dir = format!("{profile_dir}/cache");
    let data_dir = format!("{profile_dir}/data");
    let tmp_dir = format!("{profile_dir}/tmp");
    let proxy_arg = format!("--proxy-server={HTTP_PROXY_ENDPOINT}");

    command
        .args([
            &format!("--user-data-dir={user_data_dir}"),
            &proxy_arg,
            "--disable-quic",
        ])
        .env("HOME", profile_dir)
        .env("TMPDIR", &tmp_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_CACHE_HOME", &cache_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("HTTPS_PROXY", HTTP_PROXY_ENDPOINT)
        .env("HTTP_PROXY", HTTP_PROXY_ENDPOINT)
        .env("ALL_PROXY", SOCKS_PROXY_ENDPOINT)
        .env("NO_PROXY", LOOPBACK_NO_PROXY);
    apply_locale_env(command, profile);
}

fn parse_desktop_processes(output: &str, profile_dir: &str) -> Vec<u32> {
    let binary_marker = CLAUDE_BINARY_PATH;
    let user_data_marker = format!("--user-data-dir={profile_dir}/user-data");
    let home_marker = format!("HOME={profile_dir}");
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            let marker = command.contains(binary_marker)
                || command.contains(&user_data_marker)
                || command.contains(&home_marker);
            marker.then(|| pid.parse::<u32>().ok()).flatten()
        })
        .collect()
}

fn processes_with_profile(profile_dir: &str) -> Vec<u32> {
    let Ok(output) = Command::new("ps")
        .args(["-eww", "-axo", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    parse_desktop_processes(&String::from_utf8_lossy(&output.stdout), profile_dir)
}

fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn terminate_desktop_processes(root_pid: u32, profile_dir: &str) {
    let marker = profile_dir.to_owned();
    let marker_pids = tokio::task::spawn_blocking(move || processes_with_profile(&marker))
        .await
        .unwrap_or_default();
    terminate_process_tree(root_pid).await;
    if !marker_pids.is_empty() {
        terminate_processes(&marker_pids).await;
    }
}

#[async_trait]
impl InstanceAdapter for DesktopAdapter {
    async fn detect(&self) -> CapabilityReport {
        let installed = Path::new(&self.app_path).is_dir();
        let executable = self.executable();
        CapabilityReport {
            installed: installed && executable.is_some(),
            version: if installed {
                Some("Unknown".to_string())
            } else {
                None
            },
            path: executable,
        }
    }

    async fn prepare(&self) -> Result<()> {
        for directory in [
            self.profile_dir.as_str(),
            &format!("{}/user-data", self.profile_dir),
            &format!("{}/config", self.profile_dir),
            &format!("{}/cache", self.profile_dir),
            &format!("{}/data", self.profile_dir),
            &format!("{}/tmp", self.profile_dir),
        ] {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("failed to create Desktop Lite directory {directory}"))?;
        }
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        let executable = self
            .detect()
            .await
            .path
            .ok_or_else(|| anyhow::anyhow!("Claude Desktop executable is not installed"))?;
        let profile = self.geo_profile.read().await.clone();
        let mut command = isolated_command(executable);
        configure_desktop_command(&mut command, &self.profile_dir, &profile);
        let child = command
            .spawn()
            .context("failed to launch Claude Desktop Lite executable")?;
        let root_pid = child.id();

        // Allow Electron to create its browser helpers. If the direct binary
        // exits immediately and no profile-marked helper remains, roll back so
        // the manager cannot publish a dead Desktop instance.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let marker = self.profile_dir.clone();
        let has_marked_process =
            tokio::task::spawn_blocking(move || !processes_with_profile(&marker).is_empty())
                .await
                .unwrap_or(false);
        if !has_marked_process && !process_is_alive(root_pid) {
            terminate_process_tree(root_pid).await;
            anyhow::bail!("Claude Desktop Lite exited during startup; launch rolled back");
        }

        Ok(ProcessHandle { root_pid })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        terminate_desktop_processes(handle.root_pid, &self.profile_dir).await;
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        let profile_dir = self.profile_dir.clone();
        let marker_pids = tokio::task::spawn_blocking(move || processes_with_profile(&profile_dir))
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
    use super::{configure_desktop_command, parse_desktop_processes};
    use crate::geoip::GeoProfile;
    use std::process::Command;

    fn env_value(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == key)
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    #[test]
    fn desktop_command_uses_isolated_home_and_proxy() {
        let profile = GeoProfile {
            timezone: Some("America/New_York".to_string()),
            locale: Some("en_US.UTF-8".to_string()),
            ..GeoProfile::default()
        };
        let mut command = Command::new(super::CLAUDE_BINARY_PATH);
        configure_desktop_command(&mut command, "/tmp/cim-desktop", &profile);
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .iter()
            .any(|arg| arg == "--user-data-dir=/tmp/cim-desktop/user-data"));
        assert!(args
            .iter()
            .any(|arg| arg == "--proxy-server=http://127.0.0.1:7890"));
        assert!(args.iter().any(|arg| arg == "--disable-quic"));
        assert_eq!(
            env_value(&command, "HOME").as_deref(),
            Some("/tmp/cim-desktop")
        );
        assert_eq!(
            env_value(&command, "TMPDIR").as_deref(),
            Some("/tmp/cim-desktop/tmp")
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
            env_value(&command, "TZ").as_deref(),
            Some("America/New_York")
        );
    }

    #[test]
    fn desktop_marker_matches_only_its_profile() {
        let profile = "/tmp/cim-desktop";
        let ps = format!(
            " 101 /Applications/Claude.app/Contents/MacOS/Claude --user-data-dir={profile}/user-data\n 102 helper --user-data-dir={profile}-other/user-data\n 103 helper HOME={profile}\n"
        );
        assert_eq!(parse_desktop_processes(&ps, profile), vec![101, 103]);
    }
}
