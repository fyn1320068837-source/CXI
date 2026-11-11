//! macOS PF fail-closed controller.
//!
//! PF is an optional kernel-level safety net. The daemon always keeps the
//! proxy health gate in place, and uses this controller when the configured PF
//! anchor and isolation group are installed on the host.

use crate::config::FirewallConfig;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

const BLOCK_RULES: &str = r#"# CIM fail-closed rules.
# Instances are launched with the cimgrp effective group.
# Only the local HTTP/SOCKS listeners remain reachable while blocked.
block return out quick proto { tcp udp } from any to !127.0.0.0/8 group cimgrp
block return out quick proto { tcp udp } from any to !::1 group cimgrp
pass out quick proto { tcp udp } from any to 127.0.0.0/8 group cimgrp
pass out quick proto { tcp udp } from any to ::1 group cimgrp
"#;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FirewallMode {
    Unavailable,
    Allowed,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirewallStatus {
    pub installed: bool,
    pub mode: FirewallMode,
    pub status: String,
    pub anchor: String,
    pub isolation_group: String,
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct FirewallState {
    installed: bool,
    mode: FirewallMode,
    last_error: Option<String>,
}

/// Controls only the CIM anchor. It never enables/disables the system PF
/// engine and never changes the host's default route.
pub struct FirewallController {
    config: FirewallConfig,
    block_rules_path: PathBuf,
    state: Arc<Mutex<FirewallState>>,
}

impl FirewallController {
    pub fn new(config: FirewallConfig) -> Self {
        let base = PathBuf::from(&config.rules_path);
        let block_rules_path = if config.rules_path.ends_with(".block") {
            base
        } else {
            PathBuf::from(format!("{}.block", config.rules_path))
        };
        let installed = Self::probe_anchor(&config.anchor_name);
        Self {
            config,
            block_rules_path,
            state: Arc::new(Mutex::new(FirewallState {
                installed,
                mode: if installed {
                    FirewallMode::Allowed
                } else {
                    FirewallMode::Unavailable
                },
                last_error: None,
            })),
        }
    }

    pub fn status(&self) -> FirewallStatus {
        let state = self.state.lock().unwrap();
        FirewallStatus {
            installed: state.installed,
            mode: state.mode,
            status: match state.mode {
                FirewallMode::Unavailable => "软隔离模式".to_string(),
                FirewallMode::Allowed => "PF 已安装 · 放行态".to_string(),
                FirewallMode::Blocked => "PF 已安装 · 故障阻断".to_string(),
            },
            anchor: self.config.anchor_name.clone(),
            isolation_group: self.config.isolation_group.clone(),
            last_error: state.last_error.clone(),
        }
    }

    pub fn block_rules_path(&self) -> &Path {
        &self.block_rules_path
    }

    /// Load the block rules into the CIM anchor. If PF is not installed or
    /// the anchor cannot be controlled, this is recorded as soft isolation and
    /// the caller continues to enforce the process/API health gate.
    pub fn set_blocked(&self) -> Result<()> {
        if !self.is_ready() {
            return Ok(());
        }
        let output = run_privileged(&[
            "pfctl",
            "-a",
            &self.config.anchor_name,
            "-f",
            self.block_rules_path.to_string_lossy().as_ref(),
        ])?;
        if !output.status.success() {
            return self.record_error(format_command_error("load PF block rules", &output));
        }
        self.update_state(true, FirewallMode::Blocked, None);
        info!(anchor = %self.config.anchor_name, "PF fail-closed rules enabled");
        Ok(())
    }

    /// Clear only the CIM anchor rules. The global PF engine and unrelated
    /// anchors are left untouched.
    pub fn set_allowed(&self) -> Result<()> {
        if !self.is_ready() {
            return Ok(());
        }
        let output = run_privileged(&["pfctl", "-a", &self.config.anchor_name, "-F", "rules"])?;
        if !output.status.success() {
            return self.record_error(format_command_error("clear PF block rules", &output));
        }
        self.update_state(true, FirewallMode::Allowed, None);
        info!(anchor = %self.config.anchor_name, "PF fail-closed rules cleared");
        Ok(())
    }

    /// Prepare the persistent block rule file and isolation group. The
    /// system PF anchor declaration is intentionally left to the documented
    /// administrator step because editing /etc/pf.conf is host-wide.
    pub fn install(&self) -> Result<()> {
        let temp = std::env::temp_dir().join(format!("cim-pf-{}.block", std::process::id()));
        let rules = BLOCK_RULES.replace("cimgrp", &self.config.isolation_group);
        std::fs::write(&temp, rules)?;

        let parent = self
            .block_rules_path
            .parent()
            .unwrap_or_else(|| Path::new("/etc/pf.anchors"));
        let mkdir = run_privileged(&["mkdir", "-p", parent.to_string_lossy().as_ref()])?;
        if !mkdir.status.success() {
            let _ = std::fs::remove_file(&temp);
            return Err(anyhow::anyhow!(format_command_error(
                "create PF anchor directory",
                &mkdir
            )));
        }

        let install = run_privileged(&[
            "install",
            "-m",
            "0644",
            temp.to_string_lossy().as_ref(),
            self.block_rules_path.to_string_lossy().as_ref(),
        ])?;
        let _ = std::fs::remove_file(&temp);
        if !install.status.success() {
            return Err(anyhow::anyhow!(format_command_error(
                "install PF block rules",
                &install
            )));
        }

        let group_path = format!("/Groups/{}", self.config.isolation_group);
        let group_exists = run_privileged(&["dscl", ".", "-read", &group_path])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !group_exists {
            let group = run_privileged(&["dscl", ".", "-create", &group_path])?;
            if !group.status.success() {
                return Err(anyhow::anyhow!(format_command_error(
                    "create CIM isolation group",
                    &group
                )));
            }
        }

        let _ = run_privileged(&[
            "dscl",
            ".",
            "-create",
            &group_path,
            "RealName",
            "CIM Isolation Group",
        ]);
        if let Ok(user) = std::env::var("USER") {
            let _ = run_privileged(&[
                "dseditgroup",
                "-o",
                "edit",
                "-a",
                &user,
                "-t",
                "user",
                &self.config.isolation_group,
            ]);
        }

        // Syntax-check the installed rules. Loading is done by set_blocked so
        // the daemon can keep the current mode explicit.
        let check = run_privileged(&[
            "pfctl",
            "-nf",
            self.block_rules_path.to_string_lossy().as_ref(),
        ])?;
        if !check.status.success() {
            return Err(anyhow::anyhow!(format_command_error(
                "validate PF block rules",
                &check
            )));
        }
        self.update_state(true, FirewallMode::Allowed, None);
        Ok(())
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.installed && self.block_rules_path.exists()
    }

    fn update_state(&self, installed: bool, mode: FirewallMode, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.installed = installed;
        state.mode = mode;
        state.last_error = error;
    }

    fn record_error<T>(&self, message: String) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        state.last_error = Some(message.clone());
        warn!("{}", message);
        Err(anyhow::anyhow!(message))
    }

    fn probe_anchor(anchor: &str) -> bool {
        run_privileged(&["pfctl", "-a", anchor, "-s", "rules"])
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

fn run_privileged(args: &[&str]) -> Result<Output> {
    #[cfg(target_os = "macos")]
    let (program, command_args): (&str, Vec<&str>) = {
        let running_as_root = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim() == "0")
            .unwrap_or(false);
        if running_as_root {
            (args[0], args[1..].to_vec())
        } else {
            let mut values = vec!["-n", args[0]];
            values.extend_from_slice(&args[1..]);
            ("sudo", values)
        }
    };
    #[cfg(not(target_os = "macos"))]
    let (program, command_args): (&str, Vec<&str>) = (args[0], args[1..].to_vec());

    Command::new(program)
        .args(command_args)
        .output()
        .with_context(|| format!("failed to run {}", program))
}

fn format_command_error(action: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("{} (exit status {})", action, output.status)
    } else {
        format!("{}: {}", action, stderr)
    }
}
