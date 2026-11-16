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
# A blocked instance cannot reach the host, LAN or internet, including
# non-TCP/UDP protocols. The proxy loopback ports are also closed while
# the upstream proxy is unhealthy.
block return out quick from any to any group cimgrp
"#;

const ALLOW_RULES_TEMPLATE: &str = r#"# CIM healthy-state rules.
# Direct egress remains blocked even while the upstream proxy is healthy.
# The only permitted destination is the local CIM HTTP/SOCKS gateway.
pass out quick proto tcp from any to 127.0.0.1 port 7890 group cimgrp
pass out quick proto tcp from any to 127.0.0.1 port 7891 group cimgrp
pass out quick proto tcp from any to ::1 port 7890 group cimgrp
pass out quick proto tcp from any to ::1 port 7891 group cimgrp
block return out quick from any to any group cimgrp
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
    allow_rules_path: PathBuf,
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
        let allow_rules_path = if config.rules_path.ends_with(".block") {
            PathBuf::from(format!(
                "{}.allow",
                config.rules_path.trim_end_matches(".block")
            ))
        } else {
            PathBuf::from(format!("{}.allow", config.rules_path))
        };
        let installed = Self::probe_anchor(&config.anchor_name)
            && block_rules_path.exists()
            && allow_rules_path.exists();
        Self {
            config,
            block_rules_path,
            allow_rules_path,
            state: Arc::new(Mutex::new(FirewallState {
                installed,
                // Presence of the anchor and rule files only proves that the
                // controller can attempt a transition.  Do not claim an
                // allowed/blocked policy until pfctl has returned and the
                // live rules have been checked against the expected policy.
                mode: FirewallMode::Unavailable,
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
                FirewallMode::Unavailable => {
                    if state.installed {
                        "PF 已安装 · 规则状态未知".to_string()
                    } else {
                        "软隔离模式".to_string()
                    }
                }
                FirewallMode::Allowed => "PF 已安装 · 仅代理放行".to_string(),
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

    pub fn allow_rules_path(&self) -> &Path {
        &self.allow_rules_path
    }

    /// Load the block rules into the CIM anchor. If PF is not installed or
    /// the anchor cannot be controlled, this is recorded as soft isolation and
    /// the caller continues to enforce the process/API health gate.
    pub fn set_blocked(&self) -> Result<()> {
        if !self.is_ready() {
            return Ok(());
        }
        let output = match run_privileged(&[
            "pfctl",
            "-a",
            &self.config.anchor_name,
            "-f",
            self.block_rules_path.to_string_lossy().as_ref(),
        ]) {
            Ok(output) => output,
            Err(error) => return self.record_error(format!("load PF block rules: {error}")),
        };
        if !output.status.success() {
            return self.record_error(format_command_error("load PF block rules", &output));
        }
        // Drop states created under the previous healthy anchor. Otherwise a
        // connection that was already established could survive the rule
        // transition until its normal timeout.
        match run_privileged(&["pfctl", "-a", &self.config.anchor_name, "-F", "states"]) {
            Ok(states) if states.status.success() => {}
            Ok(states) => {
                return self.record_error(format_command_error("flush PF states", &states));
            }
            Err(error) => return self.record_error(format!("flush PF states: {error}")),
        }
        self.verify_live_rules(FirewallMode::Blocked)?;
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
        // Keep a restrictive anchor loaded in healthy state. Clearing the
        // anchor would leave a crash/restart window in which instances could
        // connect directly before the daemon re-applies the block rules.
        let output = match run_privileged(&[
            "pfctl",
            "-a",
            &self.config.anchor_name,
            "-f",
            self.allow_rules_path.to_string_lossy().as_ref(),
        ]) {
            Ok(output) => output,
            Err(error) => return self.record_error(format!("load PF allow rules: {error}")),
        };
        if !output.status.success() {
            return self.record_error(format_command_error("load PF allow rules", &output));
        }
        self.verify_live_rules(FirewallMode::Allowed)?;
        self.update_state(true, FirewallMode::Allowed, None);
        info!(anchor = %self.config.anchor_name, "PF fail-closed rules switched to proxy-only allow state");
        Ok(())
    }

    /// Prepare the persistent block rule file and isolation group. The
    /// system PF anchor declaration is intentionally left to the documented
    /// administrator step because editing /etc/pf.conf is host-wide.
    pub fn install(&self) -> Result<()> {
        let temp = std::env::temp_dir().join(format!("cim-pf-{}.block", std::process::id()));
        let allow_temp = std::env::temp_dir().join(format!("cim-pf-{}.allow", std::process::id()));
        let rules = BLOCK_RULES.replace("cimgrp", &self.config.isolation_group);
        let allow_rules = ALLOW_RULES_TEMPLATE.replace("cimgrp", &self.config.isolation_group);
        std::fs::write(&temp, rules)?;
        std::fs::write(&allow_temp, allow_rules)?;

        let parent = self
            .block_rules_path
            .parent()
            .unwrap_or_else(|| Path::new("/etc/pf.anchors"));
        let mkdir = run_privileged(&["mkdir", "-p", parent.to_string_lossy().as_ref()])?;
        if !mkdir.status.success() {
            let _ = std::fs::remove_file(&temp);
            let _ = std::fs::remove_file(&allow_temp);
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
            let _ = std::fs::remove_file(&allow_temp);
            return Err(anyhow::anyhow!(format_command_error(
                "install PF block rules",
                &install
            )));
        }

        let allow_install = run_privileged(&[
            "install",
            "-m",
            "0644",
            allow_temp.to_string_lossy().as_ref(),
            self.allow_rules_path.to_string_lossy().as_ref(),
        ])?;
        let _ = std::fs::remove_file(&allow_temp);
        if !allow_install.status.success() {
            return Err(anyhow::anyhow!(format_command_error(
                "install PF allow rules",
                &allow_install
            )));
        }

        let group_path = format!("/Groups/{}", self.config.isolation_group);
        let group_exists = run_privileged(&["dscl", ".", "-read", &group_path])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !group_exists {
            let group =
                run_privileged(&["dseditgroup", "-o", "create", &self.config.isolation_group])?;
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
        let allow_check = run_privileged(&[
            "pfctl",
            "-nf",
            self.allow_rules_path.to_string_lossy().as_ref(),
        ])?;
        if !allow_check.status.success() {
            return Err(anyhow::anyhow!(format_command_error(
                "validate PF allow rules",
                &allow_check
            )));
        }
        // Installation only writes and syntax-checks the files. The live
        // anchor is transitioned and verified by set_blocked/set_allowed;
        // do not report allowed before pfctl has loaded the policy.
        self.update_state(true, FirewallMode::Unavailable, None);
        Ok(())
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        // Both fixed rule files must exist so launchd can use the narrow
        // sudoers entries without granting pfctl access to arbitrary paths.
        state.installed && self.block_rules_path.exists() && self.allow_rules_path.exists()
    }

    fn update_state(&self, installed: bool, mode: FirewallMode, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.installed = installed;
        state.mode = mode;
        state.last_error = error;
    }

    fn record_error<T>(&self, message: String) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        // A failed pfctl transition invalidates the previous policy claim.
        // Keep the installation bit for retry, but make the effective mode
        // explicitly unknown so API consumers cannot treat stale state as
        // fail-closed evidence.
        state.mode = FirewallMode::Unavailable;
        state.last_error = Some(message.clone());
        warn!("{}", message);
        Err(anyhow::anyhow!(message))
    }

    fn verify_live_rules(&self, expected: FirewallMode) -> Result<()> {
        let output = match run_privileged(&["pfctl", "-a", &self.config.anchor_name, "-s", "rules"])
        {
            Ok(output) => output,
            Err(error) => return self.record_error(format!("inspect live PF rules: {error}")),
        };
        if !output.status.success() {
            return self.record_error(format_command_error("inspect live PF rules", &output));
        }
        let rules = String::from_utf8_lossy(&output.stdout);
        let group_id = resolve_group_id(&self.config.isolation_group);
        if !live_rules_match(
            expected,
            &rules,
            &self.config.isolation_group,
            group_id.as_deref(),
        ) {
            return self.record_error(format!(
                "live PF rules do not match expected {:?} policy for anchor {}",
                expected, self.config.anchor_name
            ));
        }
        Ok(())
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

fn live_rules_match(mode: FirewallMode, rules: &str, group: &str, group_id: Option<&str>) -> bool {
    let has_block = rules.lines().any(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        tokens.starts_with(&["block", "return", "out", "quick"])
            && (tokens.contains(&"all")
                || tokens
                    .windows(4)
                    .any(|window| window == ["from", "any", "to", "any"]))
            && line_group_matches(&tokens, group, group_id)
    });
    let has_http = rules
        .lines()
        .any(|line| line_has_proxy_rule(line, "127.0.0.1", "7890", group, group_id));
    let has_socks = rules
        .lines()
        .any(|line| line_has_proxy_rule(line, "127.0.0.1", "7891", group, group_id));
    let has_http6 = rules
        .lines()
        .any(|line| line_has_proxy_rule(line, "::1", "7890", group, group_id));
    let has_socks6 = rules
        .lines()
        .any(|line| line_has_proxy_rule(line, "::1", "7891", group, group_id));
    let has_pass = rules.lines().any(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        tokens.first() == Some(&"pass") && line_group_matches(&tokens, group, group_id)
    });
    let has_unsafe_pass = rules.lines().any(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        tokens.first() == Some(&"pass")
            && line_group_matches(&tokens, group, group_id)
            && !["127.0.0.1", "::1"].iter().any(|host| {
                ["7890", "7891"]
                    .iter()
                    .any(|port| line_has_proxy_rule(line, host, port, group, group_id))
            })
    });
    match mode {
        FirewallMode::Blocked => has_block && !has_pass,
        FirewallMode::Allowed => {
            has_block && has_http && has_socks && has_http6 && has_socks6 && !has_unsafe_pass
        }
        FirewallMode::Unavailable => false,
    }
}

fn line_group_matches(tokens: &[&str], group: &str, group_id: Option<&str>) -> bool {
    tokens.windows(2).any(|window| {
        if window[0] != "group" {
            return false;
        }
        window[1] == group
            || (window[1] == "="
                && tokens.windows(3).any(|triple| {
                    triple[0] == "group" && triple[1] == "=" && group_id == Some(triple[2])
                }))
    })
}

fn line_has_proxy_rule(
    line: &str,
    host: &str,
    port: &str,
    group: &str,
    group_id: Option<&str>,
) -> bool {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    tokens.first() == Some(&"pass")
        && tokens.windows(2).any(|window| window == ["pass", "out"])
        && tokens.windows(2).any(|window| window == ["out", "quick"])
        && tokens.windows(2).any(|window| window == ["proto", "tcp"])
        && tokens.windows(2).any(|window| window == ["from", "any"])
        && tokens.windows(2).any(|window| window == ["to", host])
        && tokens.windows(2).any(|window| {
            window[0] == "port"
                && (window[1] == port
                    || (window[1] == "="
                        && tokens.windows(3).any(|triple| {
                            triple[0] == "port" && triple[1] == "=" && triple[2] == port
                        })))
        })
        && line_group_matches(&tokens, group, group_id)
}

fn resolve_group_id(group: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let path = format!("/Groups/{group}");
        Command::new("/usr/bin/dscl")
            .args([".", "-read", &path, "PrimaryGroupID"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8(output.stdout)
                    .ok()?
                    .lines()
                    .find_map(|line| {
                        line.trim_start()
                            .strip_prefix("PrimaryGroupID:")?
                            .trim()
                            .parse()
                            .ok()
                    })
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Command::new("/usr/bin/getent")
            .args(["group", group])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8(output.stdout)
                    .ok()?
                    .split(':')
                    .nth(2)
                    .map(str::trim)
                    .map(ToOwned::to_owned)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{live_rules_match, FirewallMode, ALLOW_RULES_TEMPLATE, BLOCK_RULES};

    #[test]
    fn healthy_rules_keep_direct_egress_blocked() {
        assert!(ALLOW_RULES_TEMPLATE.contains("127.0.0.1 port 7890"));
        assert!(ALLOW_RULES_TEMPLATE.contains("127.0.0.1 port 7891"));
        assert!(ALLOW_RULES_TEMPLATE.contains("::1 port 7890"));
        assert!(ALLOW_RULES_TEMPLATE.contains("::1 port 7891"));
        assert!(
            ALLOW_RULES_TEMPLATE.contains("block return out quick from any to any group cimgrp")
        );
        assert!(!ALLOW_RULES_TEMPLATE.contains("127.0.0.0/8"));
    }

    #[test]
    fn blocked_rules_cover_all_ip_protocols() {
        assert!(BLOCK_RULES.contains("block return out quick from any to any group cimgrp"));
        assert!(!BLOCK_RULES.contains("proto tcp"));
        assert!(!BLOCK_RULES.contains("proto udp"));
    }

    #[test]
    fn live_rule_validation_requires_the_expected_policy() {
        let blocked = BLOCK_RULES;
        let allowed = ALLOW_RULES_TEMPLATE;
        assert!(live_rules_match(
            FirewallMode::Blocked,
            blocked,
            "cimgrp",
            None
        ));
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            blocked,
            "cimgrp",
            None
        ));
        assert!(live_rules_match(
            FirewallMode::Allowed,
            allowed,
            "cimgrp",
            None
        ));
        assert!(!live_rules_match(
            FirewallMode::Blocked,
            allowed,
            "cimgrp",
            None
        ));
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            "block return out quick from any to any group cimgrp",
            "cimgrp",
            None
        ));
        assert!(!live_rules_match(
            FirewallMode::Blocked,
            "pass out quick proto tcp from any to 127.0.0.1 port 7890 group cimgrp\nblock return out quick from any to any group cimgrp",
            "cimgrp",
            None
        ));
        let pfctl = "# anchor com.cim\npass\tout\tquick\tinet proto tcp from any to 127.0.0.1 port = 7890 group = 501 flags S/SA\npass out quick inet proto tcp from any to 127.0.0.1 port = 7891 group = 501 flags S/SA\npass out quick inet6 proto tcp from any to ::1 port = 7890 group = 501 flags S/SA\npass out quick inet6 proto tcp from any to ::1 port = 7891 group = 501 flags S/SA\nblock return out quick all group = 501";
        assert!(live_rules_match(
            FirewallMode::Allowed,
            pfctl,
            "cimgrp",
            Some("501")
        ));
        let unsafe_pass = "pass out quick inet proto tcp from any to 203.0.113.10 port = 443 group = 501\nblock return out quick all group = 501";
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            unsafe_pass,
            "cimgrp",
            Some("501")
        ));
        let inbound_pass = "pass in quick inet proto tcp from any to 127.0.0.1 port = 7890 group = 501\npass out quick inet proto tcp from any to 127.0.0.1 port = 7891 group = 501\npass out quick inet6 proto tcp from any to ::1 port = 7890 group = 501\npass out quick inet6 proto tcp from any to ::1 port = 7891 group = 501\nblock return out quick all group = 501";
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            inbound_pass,
            "cimgrp",
            Some("501")
        ));
        let source_host = "pass out quick inet proto tcp from 127.0.0.1 to any port = 7890 group = 501\npass out quick inet proto tcp from any to 127.0.0.1 port = 7891 group = 501\npass out quick inet6 proto tcp from any to ::1 port = 7890 group = 501\npass out quick inet6 proto tcp from any to ::1 port = 7891 group = 501\nblock return out quick all group = 501";
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            source_host,
            "cimgrp",
            Some("501")
        ));
    }
}
