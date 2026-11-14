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

pub(crate) const HTTP_PROXY_ENDPOINT: &str = "http://127.0.0.1:7890";
pub(crate) const SOCKS_PROXY_ENDPOINT: &str = "socks5://127.0.0.1:7891";
pub(crate) const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.1";

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

/// Return whether the process-level PF boundary is ready for a managed
/// instance. The API layer uses this as a launch gate so a missing helper
/// cannot silently downgrade a protected instance to a direct child process.
pub fn isolation_ready() -> bool {
    isolation_group_gid().is_some() && Path::new(INSTANCE_LAUNCHER).is_file()
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

/// Resolve an instance executable using the daemon's PATH and common macOS
/// installation locations. launchd intentionally starts with a small PATH,
/// so relying on `which` alone would miss Homebrew and app-bundled CLIs.
pub(crate) fn resolve_executable(candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        if candidate.contains('/') {
            return Path::new(candidate)
                .is_file()
                .then(|| (*candidate).to_string());
        }
        Command::new("which")
            .arg(candidate)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
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

/// Terminate a captured set of instance processes, including helpers that
/// have already detached from the root process. PIDs come from either the
/// process tree or the unique Web session marker and are best-effort.
pub async fn terminate_processes(pids: &[u32]) {
    let mut captured = pids.to_vec();
    // A marker can identify a detached shell/helper rather than the original
    // browser root. Include its descendants before signalling so the helper
    // cannot orphan a long-running child when the shell exits.
    for pid in pids {
        for descendant in descendant_pids(*pid) {
            if !captured.contains(&descendant) {
                captured.push(descendant);
            }
        }
    }
    signal_process_tree(&captured, nix::sys::signal::Signal::SIGTERM);
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    signal_process_tree(&captured, nix::sys::signal::Signal::SIGKILL);
}

/// Terminate an instance and every descendant process that belongs to its
/// process tree. Adapters used to signal only the root process, which can
/// leave browser/extension helpers alive after a failed launch or stop.
///
/// The tree is collected again before the force-kill pass because a helper
/// may be spawned after the initial snapshot. Errors are intentionally
/// ignored: the caller is already handling a best-effort cleanup path.
pub async fn terminate_process_tree(root_pid: u32) {
    let initial_pids = descendant_pids(root_pid);
    signal_process_tree(&initial_pids, nix::sys::signal::Signal::SIGTERM);
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    let mut remaining_pids = initial_pids;
    for pid in descendant_pids(root_pid) {
        if !remaining_pids.contains(&pid) {
            remaining_pids.push(pid);
        }
    }
    signal_process_tree(&remaining_pids, nix::sys::signal::Signal::SIGKILL);
}

fn signal_process_tree(pids: &[u32], signal: nix::sys::signal::Signal) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // Signal children before their root so process-group helpers have a
    // chance to flush and exit without leaving grandchildren behind.
    for pid in pids.iter().rev() {
        if *pid == 0 || *pid > i32::MAX as u32 {
            continue;
        }
        let _ = kill(Pid::from_raw(*pid as i32), signal);
    }
}

// Concrete adapters
mod code;
mod desktop;
mod manager;
mod runtime;
mod vscode;
mod web;

pub use code::CodeAdapter;
pub use desktop::DesktopAdapter;
pub use manager::InstanceManager;
pub use vscode::VsCodeAdapter;
pub use web::WebAdapter;

#[cfg(test)]
mod tests {
    use super::{descendant_pids, terminate_process_tree, InstanceKind};
    use std::process::{Command, Stdio};

    #[test]
    fn instance_kind_round_trips_api_names() {
        for kind in InstanceKind::ALL {
            assert_eq!(InstanceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(InstanceKind::parse("unknown"), None);
    }

    #[tokio::test]
    async fn terminate_process_tree_cleans_root_and_helpers() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .expect("shell is required for the process-tree cleanup test");
        let root_pid = child.id();

        // Let the shell create its helper before taking the snapshot. The
        // cleanup path must cover both the adapter root and its descendants.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pids = descendant_pids(root_pid);
        assert!(pids.contains(&root_pid));
        // Some sandboxed macOS runners hide child PIDs from `pgrep -P`; the
        // root assertion still exercises the mandatory cleanup path there.

        terminate_process_tree(root_pid).await;

        let _ = child.try_wait();
        for pid in pids {
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(!alive, "process {pid} survived tree cleanup");
        }
    }
}
