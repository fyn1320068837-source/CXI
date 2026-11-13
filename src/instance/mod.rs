// Instance Adapters - unified interface for A/B/C/D instances
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use tokio::sync::mpsc;

use crate::geoip::GeoProfile;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum InstanceKind {
    Code,    // A: Claude Code CLI
    Desktop, // B: Claude Desktop app
    VsCode,  // C: VS Code extension
    Web,     // D: Claude Web in Chromium
}

impl InstanceKind {
    pub const ALL: [Self; 4] = [Self::Code, Self::Desktop, Self::VsCode, Self::Web];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Desktop => "desktop",
            Self::VsCode => "vscode",
            Self::Web => "web",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "desktop" => Some(Self::Desktop),
            "vscode" => Some(Self::VsCode),
            "web" => Some(Self::Web),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub root_pid: u32,
}

/// A completed request observed from an instance's network stack.
#[derive(Debug, Clone, Serialize)]
pub struct RequestLogEntry {
    pub timestamp: String,
    pub instance: String,
    pub method: String,
    pub url: String,
    pub status: u16,
    pub latency_ms: u64,
    pub size_bytes: u64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub type RequestLogSender = mpsc::Sender<RequestLogEntry>;

#[derive(Debug)]
pub struct CapabilityReport {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ValidationStatus {
    Pass,
    Fail,
    Unknown,
    Unsupported,
}

#[async_trait]
pub trait InstanceAdapter: Send + Sync {
    async fn detect(&self) -> CapabilityReport;
    async fn prepare(&self) -> Result<()>;
    async fn launch(&self) -> Result<ProcessHandle>;
    async fn stop(&self, handle: &ProcessHandle) -> Result<()>;
    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32>;
}

/// Apply the locale values derived from the current exit IP to a child
/// process. The host process is never modified; only newly launched
/// instances receive these overrides.
pub fn apply_locale_env(command: &mut Command, profile: &GeoProfile) {
    if let Some(timezone) = profile.timezone.as_deref() {
        command.env("TZ", timezone);
    }
    if let Some(locale) = profile.locale.as_deref() {
        command.env("LANG", locale);
        command.env("LC_ALL", locale);
    }
}

const INSTANCE_LAUNCHER: &str = "/usr/local/libexec/cim-instance-launch";

/// Return the numeric GID for the optional PF isolation group.
pub fn isolation_group_gid() -> Option<u32> {
    let group = std::env::var("CIM_ISOLATION_GROUP").unwrap_or_else(|_| "cimgrp".to_string());
    #[cfg(target_os = "macos")]
    {
        let group_path = format!("/Groups/{group}");
        Command::new("dscl")
            .args([".", "-read", &group_path, "PrimaryGroupID"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| parse_primary_group_id(&output.stdout))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Command::new("getent")
            .args(["group", &group])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8(output.stdout)
                    .ok()?
                    .split(':')
                    .nth(2)
                    .and_then(|value| value.trim().parse().ok())
            })
    }
}

#[cfg(target_os = "macos")]
fn parse_primary_group_id(output: &[u8]) -> Option<u32> {
    String::from_utf8_lossy(output)
        .lines()
        .find_map(|line| line.strip_prefix("PrimaryGroupID:")?.trim().parse().ok())
}

/// Build a command that starts an instance with the PF isolation GID.
///
/// The daemon remains a normal user process. When the helper and group are
/// installed, sudo runs only the root-owned launcher, which immediately
/// restores the requesting user's UID and supplementary groups before exec.
pub fn isolated_command(program: impl AsRef<OsStr>) -> Command {
    let Some(gid) = isolation_group_gid() else {
        return Command::new(program);
    };
    if !Path::new(INSTANCE_LAUNCHER).is_file() {
        return Command::new(program);
    }

    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u32>().ok());
    let user = std::env::var("USER").ok().filter(|value| !value.is_empty());
    let (Some(uid), Some(user)) = (uid, user) else {
        return Command::new(program);
    };

    let mut command = Command::new("/usr/bin/sudo");
    command.args([
        "-n",
        "-E",
        INSTANCE_LAUNCHER,
        "--uid",
        &uid.to_string(),
        "--gid",
        &gid.to_string(),
        "--user",
        &user,
        "--",
    ]);
    command.arg(program);
    command
}

/// Enumerate a process and all descendants using the portable macOS pgrep
/// parent query. Failed/vanished children are ignored because the verifier
/// should inspect the processes that still exist at the instant of the check.
pub fn descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut result = vec![root_pid];
    let mut pending = vec![root_pid];
    while let Some(parent) = pending.pop() {
        let Ok(output) = Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                if !result.contains(&pid) {
                    result.push(pid);
                    pending.push(pid);
                }
            }
        }
    }
    result
}

// Concrete adapters
mod code;
mod desktop;
mod manager;
mod vscode;
mod web;

pub use code::CodeAdapter;
pub use desktop::DesktopAdapter;
pub use manager::InstanceManager;
pub use vscode::VsCodeAdapter;
pub use web::WebAdapter;

#[cfg(test)]
mod tests {
    use super::InstanceKind;

    #[test]
    fn instance_kind_round_trips_api_names() {
        for kind in InstanceKind::ALL {
            assert_eq!(InstanceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(InstanceKind::parse("unknown"), None);
    }
}
