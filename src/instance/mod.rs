// Instance Adapters - unified interface for A/B/C/D instances
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::process::Command;

use crate::geoip::GeoProfile;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum InstanceKind {
    Code,    // A: Claude Code CLI
    Desktop, // B: Claude Desktop app
    VsCode,  // C: VS Code extension
    Web,     // D: Claude Web in Chromium
}

#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub root_pid: u32,
}

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

/// Return the effective gid for the optional PF isolation group. Instances
/// keep the normal user id but use this group so PF can block them without
/// blocking the sing-box process that owns the upstream connection.
pub fn isolation_group_gid() -> Option<u32> {
    let group = std::env::var("CIM_ISOLATION_GROUP").unwrap_or_else(|_| "cimgrp".to_string());
    Command::new("id")
        .args(["-g", &group])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(unix)]
pub fn apply_isolation_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    if let Some(gid) = isolation_group_gid() {
        command.gid(gid);
    }
}

#[cfg(not(unix))]
pub fn apply_isolation_group(_command: &mut Command) {}

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
