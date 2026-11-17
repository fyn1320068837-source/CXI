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
# Local host requests reaching a cimgrp listener create reply state (Chrome CDP).
# Instance-originated SYN packets are blocked on output before this input rule.
# Instance-initiated connections, including the proxy ports, remain blocked.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
block return out quick from any to any group cimgrp
"#;

const ALLOW_RULES_TEMPLATE: &str = r#"# CIM healthy-state rules.
# Direct egress remains blocked even while the upstream proxy is healthy.
# Host-initiated loopback control connections retain state for instance replies.
# Instances may initiate connections only to the local HTTP/SOCKS gateway.
# Proxy traffic is stateless so a block transition applies to the next packet
# of existing connections without flushing unrelated host connection states.
pass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group cimgrp flags S/SA keep state
pass out quick proto tcp from any to 127.0.0.1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to 127.0.0.1 port 7891 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7890 group cimgrp flags any no state
pass out quick proto tcp from any to ::1 port 7891 group cimgrp flags any no state
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
    pub global_enabled: bool,
    pub root_anchor_loaded: bool,
    pub mode: FirewallMode,
    pub status: String,
    pub anchor: String,
    pub isolation_group: String,
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct FirewallState {
    installed: bool,
    global_enabled: bool,
    root_anchor_loaded: bool,
    mode: FirewallMode,
    last_error: Option<String>,
}

/// Controls the CIM anchor and verifies that the global PF engine evaluates
/// it. A disabled engine is enabled only after restrictive rules are loaded;
/// the controller never disables PF or changes the host's default route.
pub struct FirewallController {
    config: FirewallConfig,
    block_rules_path: PathBuf,
    allow_rules_path: PathBuf,
    state: Arc<Mutex<FirewallState>>,
    transition_lock: Mutex<()>,
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
            transition_lock: Mutex::new(()),
            state: Arc::new(Mutex::new(FirewallState {
                installed,
                global_enabled: false,
                root_anchor_loaded: false,
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
            global_enabled: state.global_enabled,
            root_anchor_loaded: state.root_anchor_loaded,
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

    /// Load the block rules into the CIM anchor. A missing installation or
    /// failed transition is an error; callers must not treat it as a
    /// fail-closed success.
    pub fn set_blocked(&self) -> Result<()> {
        self.transition(FirewallMode::Blocked)
    }

    /// Allow only the local proxy after verifying the global engine and the
    /// root ruleset. Unrelated anchors are never changed.
    pub fn set_allowed(&self) -> Result<()> {
        self.transition(FirewallMode::Allowed)
    }

    fn transition(&self, mode: FirewallMode) -> Result<()> {
        if !self.is_ready() {
            let _transition = self.transition_lock.lock().unwrap();
            return self.record_error("PF fail-closed rules are not installed".to_string());
        }
        let group_id = resolve_group_id(&self.config.isolation_group);
        self.transition_with(mode, group_id.as_deref(), run_privileged)
    }

    fn transition_with(
        &self,
        mode: FirewallMode,
        group_id: Option<&str>,
        mut run: impl FnMut(&[&str]) -> Result<Output>,
    ) -> Result<()> {
        // Health checks and fault handling may run concurrently. Keep each
        // read/load/verify sequence atomic with respect to other transitions,
        // while status readers retain the last verified policy. A routine
        // health check must not briefly advertise Unavailable to the watchdog.
        let _transition = self.transition_lock.lock().unwrap();
        if mode == FirewallMode::Unavailable {
            return self.record_error("cannot transition to an unavailable PF policy".to_string());
        }
        let result = (|| {
            if mode == FirewallMode::Blocked {
                self.load_policy(mode, group_id, &mut run)?;
            }
            let info = checked_pf_command(&mut run, &["pfctl", "-s", "info"], "inspect PF engine")?;
            let was_enabled = parse_global_enabled(&info)?;
            if !was_enabled {
                // This is an observed loss of protection, not an in-progress
                // check. Invalidate immediately while recovery is attempted.
                self.update_state(true, FirewallMode::Unavailable, None);
            }
            let rules =
                checked_pf_command(&mut run, &["pfctl", "-s", "rules"], "inspect PF root rules")?;
            if !root_anchor_matches(&rules, &self.config.anchor_name) {
                anyhow::bail!(
                    "PF root rules do not safely reference anchor {} in both directions",
                    self.config.anchor_name
                );
            }
            if !was_enabled {
                // Loading an anchor while PF is disabled does not enforce it.
                // Install and verify the block policy before enabling PF, then
                // require an independent readback before allowing any traffic.
                if mode != FirewallMode::Blocked {
                    self.load_policy(FirewallMode::Blocked, group_id, &mut run)?;
                }
                checked_pf_command(&mut run, &["pfctl", "-e"], "enable PF engine")?;
                let info = checked_pf_command(
                    &mut run,
                    &["pfctl", "-s", "info"],
                    "verify PF engine enabled",
                )?;
                if !parse_global_enabled(&info)? {
                    anyhow::bail!("PF engine remains disabled after enable request");
                }
            }
            // Proxy allow rules use no state: every instance data packet is
            // filtered again after switching to blocked. Never invoke -F
            // states here; macOS does not provide an anchor-scoped guarantee
            // for that operation. Host-initiated local control state remains.
            if mode == FirewallMode::Allowed {
                self.load_policy(mode, group_id, &mut run)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return self.record_error(format!("verify PF enforcement: {error}"));
        }
        {
            let mut state = self.state.lock().unwrap();
            state.global_enabled = true;
            state.root_anchor_loaded = true;
            state.mode = mode;
            state.last_error = None;
        }
        info!(anchor = %self.config.anchor_name, ?mode, "PF global engine and anchor enforcement verified");
        Ok(())
    }

    fn load_policy(
        &self,
        mode: FirewallMode,
        group_id: Option<&str>,
        run: &mut impl FnMut(&[&str]) -> Result<Output>,
    ) -> Result<()> {
        let path = match mode {
            FirewallMode::Blocked => &self.block_rules_path,
            FirewallMode::Allowed => &self.allow_rules_path,
            FirewallMode::Unavailable => anyhow::bail!("cannot load an unavailable PF policy"),
        };
        checked_pf_command(
            run,
            &[
                "pfctl",
                "-a",
                &self.config.anchor_name,
                "-f",
                path.to_string_lossy().as_ref(),
            ],
            "load PF policy",
        )?;
        let rules = checked_pf_command(
            run,
            &["pfctl", "-a", &self.config.anchor_name, "-s", "rules"],
            "inspect live PF rules",
        )?;
        if !live_rules_match(mode, &rules, &self.config.isolation_group, group_id) {
            anyhow::bail!(
                "live PF rules do not match expected {:?} policy for anchor {}",
                mode,
                self.config.anchor_name
            );
        }
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
        if mode == FirewallMode::Unavailable {
            state.global_enabled = false;
            state.root_anchor_loaded = false;
        }
    }

    fn record_error<T>(&self, message: String) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        // A failed pfctl transition invalidates the previous policy claim.
        // Keep the installation bit for retry, but make the effective mode
        // explicitly unknown so API consumers cannot treat stale state as
        // fail-closed evidence.
        state.mode = FirewallMode::Unavailable;
        state.global_enabled = false;
        state.root_anchor_loaded = false;
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

fn checked_pf_command(
    run: &mut impl FnMut(&[&str]) -> Result<Output>,
    args: &[&str],
    action: &str,
) -> Result<String> {
    let output = run(args).with_context(|| action.to_string())?;
    if !output.status.success() {
        anyhow::bail!(format_command_error(action, &output));
    }
    String::from_utf8(output.stdout).with_context(|| format!("{action}: invalid UTF-8 output"))
}

fn parse_global_enabled(info: &str) -> Result<bool> {
    let statuses = info
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Status:"))
        .map(|value| value.split_whitespace().next())
        .collect::<Vec<_>>();
    match statuses.as_slice() {
        [Some("Enabled")] => Ok(true),
        [Some("Disabled")] => Ok(false),
        _ => anyhow::bail!("PF engine status is missing or unrecognized"),
    }
}

fn root_anchor_matches(rules: &str, anchor: &str) -> bool {
    for line in rules.lines() {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        if tokens.first() == Some(&"anchor")
            && tokens.get(1).map(|value| value.trim_matches('"')) == Some(anchor)
        {
            // Both directions must reach CIM: input creates state for host
            // control requests, output restricts instance-originated traffic.
            // Reject protocol/interface/address restrictions and child anchors.
            return matches!(&tokens[2..], ["all"] | ["quick", "all"]);
        }
        // A preceding terminal pass (or terminal anchor) can skip CIM. Be
        // conservative even if its address/group restriction looks harmless.
        // Non-quick system anchors remain supported; their nested policies
        // are outside this root-rules check and still require active probes.
        if matches!(tokens.first(), Some(&"pass") | Some(&"anchor")) && tokens.contains(&"quick") {
            return false;
        }
    }
    false
}

fn live_rules_match(mode: FirewallMode, rules: &str, group: &str, group_id: Option<&str>) -> bool {
    // Match the complete ordered whitelist, not just the presence of required
    // lines. Extra ungrouped/negated pass rules or a premature quick block must
    // not be reported as the configured policy.
    let lines = rules
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    match mode {
        FirewallMode::Blocked => {
            lines.len() == 2
                && line_has_host_control_rule(lines[0], group, group_id)
                && line_has_block_rule(lines[1], group, group_id)
        }
        FirewallMode::Allowed => {
            lines.len() == 6
                && line_has_host_control_rule(lines[0], group, group_id)
                && line_has_proxy_rule(lines[1], "127.0.0.1", "7890", group, group_id)
                && line_has_proxy_rule(lines[2], "127.0.0.1", "7891", group, group_id)
                && line_has_proxy_rule(lines[3], "::1", "7890", group, group_id)
                && line_has_proxy_rule(lines[4], "::1", "7891", group, group_id)
                && line_has_block_rule(lines[5], group, group_id)
        }
        FirewallMode::Unavailable => false,
    }
}

fn group_clause_matches(tokens: &[&str], group: &str, group_id: Option<&str>) -> bool {
    let name = match tokens {
        ["group", name] | ["group", "=", name] => *name,
        _ => return false,
    };
    name == group || group_id == Some(name)
}

fn line_has_host_control_rule(line: &str, group: &str, group_id: Option<&str>) -> bool {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    let Some(tail) = tokens.strip_prefix(&[
        "pass",
        "in",
        "quick",
        "on",
        "lo0",
        "inet",
        "proto",
        "tcp",
        "from",
        "127.0.0.1",
        "to",
        "127.0.0.1",
    ]) else {
        return false;
    };
    // pfctl may omit the default keep-state suffix. Explicit no-state or
    // broader flags must remain a mismatch.
    let Some(clause) = tail
        .strip_suffix(&["flags", "S/SA", "keep", "state"])
        .or_else(|| tail.strip_suffix(&["flags", "S/SA"]))
    else {
        return false;
    };
    group_clause_matches(clause, group, group_id)
}

fn line_has_block_rule(line: &str, group: &str, group_id: Option<&str>) -> bool {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    let Some(tail) = tokens.strip_prefix(&["block", "return", "out", "quick"]) else {
        return false;
    };
    let clause = tail
        .strip_prefix(&["all"])
        .or_else(|| tail.strip_prefix(&["from", "any", "to", "any"]));
    clause.is_some_and(|clause| group_clause_matches(clause, group, group_id))
}

fn line_has_proxy_rule(
    line: &str,
    host: &str,
    port: &str,
    group: &str,
    group_id: Option<&str>,
) -> bool {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    let Some(mut tail) = tokens.strip_prefix(&["pass", "out", "quick"]) else {
        return false;
    };
    if matches!(tail.first(), Some(&"inet") | Some(&"inet6")) {
        let expected_family = if host == "::1" { "inet6" } else { "inet" };
        if tail[0] != expected_family {
            return false;
        }
        tail = &tail[1..];
    }
    let Some(tail) = tail.strip_prefix(&["proto", "tcp", "from", "any", "to", host, "port"]) else {
        return false;
    };
    let tail = tail.strip_prefix(&["="]).unwrap_or(tail);
    let Some(tail) = tail.strip_prefix(&[port]) else {
        return false;
    };
    // Explicit no-state is mandatory. Otherwise an existing proxy state could
    // bypass a later block policy. With no-state, omitted flags means all TCP
    // packets; pfctl may therefore omit the redundant `flags any` spelling.
    let Some(clause) = tail
        .strip_suffix(&["flags", "any", "no", "state"])
        .or_else(|| tail.strip_suffix(&["no", "state"]))
    else {
        return false;
    };
    group_clause_matches(clause, group, group_id)
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
    use super::{
        live_rules_match, parse_global_enabled, root_anchor_matches, FirewallController,
        FirewallMode, FirewallState, FirewallStatus, ALLOW_RULES_TEMPLATE, BLOCK_RULES,
    };
    use crate::config::FirewallConfig;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::sync::{Arc, Mutex};

    const ROOT_RULES: &str = "anchor \"com.apple/*\" all\nanchor \"com.cim\" all";

    fn test_controller() -> FirewallController {
        FirewallController {
            config: FirewallConfig {
                anchor_name: "com.cim".to_string(),
                rules_path: "/tmp/cim-pf-test".to_string(),
                isolation_group: "cimgrp".to_string(),
            },
            block_rules_path: "/tmp/cim-pf-test.block".into(),
            allow_rules_path: "/tmp/cim-pf-test.allow".into(),
            transition_lock: Mutex::new(()),
            // Start with stale success to ensure failures invalidate it.
            state: Arc::new(Mutex::new(FirewallState {
                installed: true,
                global_enabled: true,
                root_anchor_loaded: true,
                mode: FirewallMode::Allowed,
                last_error: None,
            })),
        }
    }

    fn successful_output(text: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: text.as_bytes().to_vec(),
            stderr: vec![],
        }
    }

    fn simulate_transition(
        mode: FirewallMode,
        steps: &[(&str, bool, &str)],
    ) -> (anyhow::Result<()>, FirewallStatus) {
        let controller = test_controller();
        let mut steps = VecDeque::from(steps.to_vec());
        let result = controller.transition_with(mode, Some("501"), |args| {
            let (command, success, text) = steps.pop_front().expect("unexpected PF command");
            assert_eq!(args.join(" "), command);
            Ok(Output {
                status: ExitStatus::from_raw(if success { 0 } else { 256 }),
                stdout: if success {
                    text.as_bytes().to_vec()
                } else {
                    vec![]
                },
                stderr: if success {
                    vec![]
                } else {
                    text.as_bytes().to_vec()
                },
            })
        });
        assert!(
            steps.is_empty(),
            "unexecuted expected PF commands: {steps:?}"
        );
        (result, controller.status())
    }

    #[test]
    fn global_status_requires_exact_known_status() {
        assert!(parse_global_enabled("Status: Enabled for 0 days\nDebug: Urgent").unwrap());
        assert!(!parse_global_enabled("Status: Disabled\nDebug: Urgent").unwrap());
        for invalid in [
            "",
            "Status: Unknown",
            "Status: NotEnabled",
            "Status: Enabled\nStatus: Disabled",
        ] {
            assert!(parse_global_enabled(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn root_rules_require_both_directions_without_prior_quick_pass() {
        assert!(root_anchor_matches(ROOT_RULES, "com.cim"));
        assert!(root_anchor_matches(
            "anchor \"com.cim\" quick all",
            "com.cim"
        ));
        for invalid in [
            "",
            "anchor \"com.cim.other\" all",
            "anchor \"com.cim/*\" all",
            "anchor \"com.cim\" in all",
            "anchor \"com.cim\" out all",
            "anchor \"com.cim\" out quick all",
            "anchor \"com.cim\" out proto tcp all",
            "anchor \"com.cim\" out on en0 all",
            "pass out quick all\nanchor \"com.cim\" all",
            "pass quick all\nanchor \"com.cim\" all",
            "pass in quick all\nanchor \"com.cim\" all",
            "anchor \"other\" quick all\nanchor \"com.cim\" all",
        ] {
            assert!(!root_anchor_matches(invalid, "com.cim"), "{invalid}");
        }
    }

    #[test]
    fn allowed_transition_verifies_global_and_anchor_without_disrupting_states() {
        let (result, state) = simulate_transition(
            FirewallMode::Allowed,
            &[
                ("pfctl -s info", true, "Status: Enabled"),
                ("pfctl -s rules", true, ROOT_RULES),
                ("pfctl -a com.cim -f /tmp/cim-pf-test.allow", true, ""),
                ("pfctl -a com.cim -s rules", true, ALLOW_RULES_TEMPLATE),
            ],
        );
        result.unwrap();
        assert_eq!(state.mode, FirewallMode::Allowed);
        assert!(state.global_enabled && state.root_anchor_loaded);
    }

    #[test]
    fn healthy_recheck_preserves_last_verified_state_for_watchdog() {
        let controller = test_controller();
        controller.state.lock().unwrap().last_error = Some("previous failure".into());
        controller
            .transition_with(FirewallMode::Allowed, Some("501"), |args| {
                let state = controller.status();
                assert_eq!(state.mode, FirewallMode::Allowed);
                assert!(state.global_enabled && state.root_anchor_loaded);
                assert!(controller.transition_lock.try_lock().is_err());
                Ok(successful_output(match args {
                    ["pfctl", "-s", "info"] => "Status: Enabled",
                    ["pfctl", "-s", "rules"] => ROOT_RULES,
                    ["pfctl", "-a", "com.cim", "-s", "rules"] => ALLOW_RULES_TEMPLATE,
                    _ => "",
                }))
            })
            .unwrap();
        assert!(controller.status().last_error.is_none());
        assert!(controller.transition_lock.try_lock().is_ok());
    }

    #[test]
    fn disabled_engine_invalidates_evidence_before_recovery_commands() {
        let controller = test_controller();
        let mut first = true;
        let result = controller.transition_with(FirewallMode::Allowed, Some("501"), |args| {
            if first {
                first = false;
                assert_eq!(args, ["pfctl", "-s", "info"]);
                return Ok(successful_output("Status: Disabled"));
            }
            assert_eq!(args, ["pfctl", "-s", "rules"]);
            let state = controller.status();
            assert_eq!(state.mode, FirewallMode::Unavailable);
            assert!(!state.global_enabled && !state.root_anchor_loaded);
            anyhow::bail!("injected root rules read failure")
        });
        assert!(result.is_err());
        assert_eq!(controller.status().mode, FirewallMode::Unavailable);
    }

    #[test]
    fn concurrent_allowed_and_blocked_transitions_do_not_interleave() {
        use std::sync::mpsc;
        use std::time::Duration;

        let controller = Arc::new(test_controller());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let allowed_controller = Arc::clone(&controller);
        let allowed = std::thread::spawn(move || {
            let mut first = true;
            allowed_controller.transition_with(FirewallMode::Allowed, Some("501"), |args| {
                if first {
                    first = false;
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(successful_output(match args {
                    ["pfctl", "-s", "info"] => "Status: Enabled",
                    ["pfctl", "-s", "rules"] => ROOT_RULES,
                    ["pfctl", "-a", "com.cim", "-s", "rules"] => ALLOW_RULES_TEMPLATE,
                    _ => "",
                }))
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (blocked_enter_tx, blocked_enter_rx) = mpsc::channel();
        let blocked_controller = Arc::clone(&controller);
        let blocked = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let mut first = true;
            blocked_controller.transition_with(FirewallMode::Blocked, Some("501"), |args| {
                if first {
                    first = false;
                    blocked_enter_tx.send(()).unwrap();
                }
                Ok(successful_output(match args {
                    ["pfctl", "-s", "info"] => "Status: Enabled",
                    ["pfctl", "-s", "rules"] => ROOT_RULES,
                    ["pfctl", "-a", "com.cim", "-s", "rules"] => BLOCK_RULES,
                    _ => "",
                }))
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let before_release = blocked_enter_rx.recv_timeout(Duration::from_millis(50));
        release_tx.send(()).unwrap();
        allowed.join().unwrap().unwrap();
        blocked.join().unwrap().unwrap();
        assert!(matches!(
            before_release,
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(controller.status().mode, FirewallMode::Blocked);
        assert!(controller.status().global_enabled && controller.status().root_anchor_loaded);
    }

    #[test]
    fn disabled_engine_is_blocked_before_enable_and_verified_before_allow() {
        let (result, state) = simulate_transition(
            FirewallMode::Allowed,
            &[
                ("pfctl -s info", true, "Status: Disabled"),
                ("pfctl -s rules", true, ROOT_RULES),
                ("pfctl -a com.cim -f /tmp/cim-pf-test.block", true, ""),
                ("pfctl -a com.cim -s rules", true, BLOCK_RULES),
                ("pfctl -e", true, ""),
                ("pfctl -s info", true, "Status: Enabled"),
                ("pfctl -a com.cim -f /tmp/cim-pf-test.allow", true, ""),
                ("pfctl -a com.cim -s rules", true, ALLOW_RULES_TEMPLATE),
            ],
        );
        result.unwrap();
        assert_eq!(state.mode, FirewallMode::Allowed);
        assert!(state.global_enabled && state.root_anchor_loaded);
    }

    #[test]
    fn blocked_transition_verifies_policy_without_flushing_host_states() {
        let (result, state) = simulate_transition(
            FirewallMode::Blocked,
            &[
                ("pfctl -a com.cim -f /tmp/cim-pf-test.block", true, ""),
                ("pfctl -a com.cim -s rules", true, BLOCK_RULES),
                ("pfctl -s info", true, "Status: Enabled"),
                ("pfctl -s rules", true, ROOT_RULES),
            ],
        );
        result.unwrap();
        assert_eq!(state.mode, FirewallMode::Blocked);
        assert!(state.global_enabled && state.root_anchor_loaded);
    }

    #[test]
    fn failed_global_verification_never_retains_enforced_state() {
        let cases: Vec<Vec<(&str, bool, &str)>> = vec![
            vec![("pfctl -s info", false, "sudo: a password is required")],
            vec![("pfctl -s info", true, "unexpected output")],
            vec![
                ("pfctl -s info", true, "Status: Enabled"),
                ("pfctl -s rules", false, "denied"),
            ],
            vec![
                ("pfctl -s info", true, "Status: Enabled"),
                ("pfctl -s rules", true, "anchor \"com.apple/*\" all"),
            ],
            vec![
                ("pfctl -s info", true, "Status: Disabled"),
                ("pfctl -s rules", true, ROOT_RULES),
                ("pfctl -a com.cim -f /tmp/cim-pf-test.block", true, ""),
                ("pfctl -a com.cim -s rules", true, BLOCK_RULES),
                ("pfctl -e", true, ""),
                ("pfctl -s info", true, "Status: Disabled"),
            ],
        ];
        for steps in cases {
            let (result, state) = simulate_transition(FirewallMode::Allowed, &steps);
            assert!(result.is_err());
            assert_eq!(state.mode, FirewallMode::Unavailable);
            assert!(!state.global_enabled && !state.root_anchor_loaded);
            assert!(state.last_error.is_some());
        }
    }

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
        assert_eq!(
            ALLOW_RULES_TEMPLATE.matches("flags any no state").count(),
            4
        );
    }

    #[test]
    fn blocked_rules_cover_all_ip_protocols() {
        let block = BLOCK_RULES
            .lines()
            .find(|line| line.starts_with("block"))
            .unwrap();
        assert_eq!(block, "block return out quick from any to any group cimgrp");
        assert!(!block.contains("proto"));
        assert_eq!(
            BLOCK_RULES
                .lines()
                .filter(|line| line.starts_with("pass"))
                .count(),
            1
        );
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
        let pfctl = "# anchor com.cim\npass in quick on lo0 inet proto tcp from 127.0.0.1 to 127.0.0.1 group = 501 flags S/SA keep state\npass\tout\tquick\tinet proto tcp from any to 127.0.0.1 port = 7890 group = 501 flags any no state\npass out quick inet proto tcp from any to 127.0.0.1 port = 7891 group = 501 flags any no state\npass out quick inet6 proto tcp from any to ::1 port = 7890 group = 501 flags any no state\npass out quick inet6 proto tcp from any to ::1 port = 7891 group = 501 flags any no state\nblock return out quick all group = 501";
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

    #[test]
    fn live_rules_reject_any_extra_pass_and_broadened_control_rule() {
        for (mode, template) in [
            (FirewallMode::Allowed, ALLOW_RULES_TEMPLATE),
            (FirewallMode::Blocked, BLOCK_RULES),
        ] {
            for changed in [
                format!("pass out quick all\n{template}"),
                format!("{template}\npass out quick inet proto tcp from any to any"),
                template.replace("on lo0", "on en0"),
                template.replace("from 127.0.0.1 to 127.0.0.1", "from any to 127.0.0.1"),
                template.replace("from 127.0.0.1 to 127.0.0.1", "from 127.0.0.1 to any"),
                template.replace("to 127.0.0.1 group cimgrp", "to 127.0.0.1 group != cimgrp"),
                template.replace("to 127.0.0.1 group cimgrp", "to 127.0.0.1 group 20"),
                template.replace("to 127.0.0.1 group cimgrp", "to 127.0.0.1"),
                template.replace("pass in quick on lo0", "pass out quick on lo0"),
                template.replace("on lo0 inet proto tcp", "on lo0 inet6 proto tcp"),
                template.replace("on lo0 inet proto tcp", "on lo0 inet proto udp"),
                template.replace("flags S/SA keep state", "flags any keep state"),
                template.replace("flags S/SA keep state", "flags S/SA no state"),
                template.replace("block return out quick", "block return in quick"),
            ] {
                assert!(
                    !live_rules_match(mode, &changed, "cimgrp", Some("501")),
                    "{changed}"
                );
            }
        }
    }

    #[test]
    fn host_control_accepts_only_syn_state_defaults_and_exact_group() {
        for (mode, template) in [
            (FirewallMode::Allowed, ALLOW_RULES_TEMPLATE),
            (FirewallMode::Blocked, BLOCK_RULES),
        ] {
            assert!(live_rules_match(
                mode,
                &template.replace("flags S/SA keep state", "flags S/SA"),
                "cimgrp",
                None,
            ));
            assert!(live_rules_match(
                mode,
                &template.replace("group cimgrp", "group = 501"),
                "cimgrp",
                Some("501"),
            ));
            assert!(!live_rules_match(
                mode,
                &template.replace("flags S/SA keep state", ""),
                "cimgrp",
                None,
            ));
        }
    }

    #[test]
    fn proxy_rules_require_stateless_all_packet_filtering() {
        assert!(live_rules_match(
            FirewallMode::Allowed,
            &ALLOW_RULES_TEMPLATE.replace("flags any no state", "no state"),
            "cimgrp",
            None,
        ));
        for state in [
            "",
            "keep state",
            "flags S/SA keep state",
            "flags any keep state",
            "flags S/SA no state",
            "flags any",
        ] {
            // One stateful (or SYN-only) proxy rule is enough to reject the
            // entire policy; this is not a majority-of-rules check.
            let unsafe_rules = ALLOW_RULES_TEMPLATE.replacen("flags any no state", state, 1);
            assert!(
                !live_rules_match(FirewallMode::Allowed, &unsafe_rules, "cimgrp", None,),
                "{state}"
            );
        }
    }

    #[test]
    fn live_rules_reject_premature_block_and_negated_proxy_group() {
        for (mode, template) in [
            (FirewallMode::Allowed, ALLOW_RULES_TEMPLATE),
            (FirewallMode::Blocked, BLOCK_RULES),
        ] {
            let mut lines = template
                .lines()
                .filter(|line| !line.starts_with('#') && !line.is_empty())
                .collect::<Vec<_>>();
            lines.rotate_right(1);
            assert!(!live_rules_match(mode, &lines.join("\n"), "cimgrp", None));
            let without_control = template
                .lines()
                .filter(|line| !line.contains("on lo0"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!live_rules_match(mode, &without_control, "cimgrp", None));
        }
        assert!(!live_rules_match(
            FirewallMode::Allowed,
            &ALLOW_RULES_TEMPLATE.replace("port 7890 group cimgrp", "port 7890 group != cimgrp"),
            "cimgrp",
            None,
        ));
    }
}
