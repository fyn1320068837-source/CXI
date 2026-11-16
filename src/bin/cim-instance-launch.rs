//! Privileged launcher used by CIM to start an instance with the PF group.
//!
//! The helper is installed root-owned and is only reachable through the
//! narrow sudoers entry created by the firewall setup. It drops to the
//! requesting user's UID/GID and supplementary groups before executing the
//! requested program, so it never runs an instance as root.

use anyhow::{bail, Context, Result};
use nix::unistd::{setgid, setuid, Gid, Uid};
use std::env;
use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The PF rules are intentionally tied to one root-managed group.  Do not
/// make this configurable through the launcher's environment or arguments:
/// doing so would let a caller choose a group that is not covered by PF.
const ISOLATION_GROUP: &str = "cimgrp";

fn main() -> Result<()> {
    if !Uid::effective().is_root() {
        bail!("cim-instance-launch must run as root via sudo");
    }

    let args: Vec<String> = env::args().collect();
    let (uid, requested_gid, user, command_index) = parse_args(&args)?;
    if uid == 0 {
        bail!("refusing to launch an instance as root");
    }

    // The helper is useful only through the narrow sudoers entry.  Requiring
    // both values prevents a direct root invocation from impersonating a
    // different account by supplying arbitrary --uid/--user values.
    let sudo_uid = env::var("SUDO_UID")
        .context("missing SUDO_UID; launch through sudo")?
        .parse::<u32>()
        .context("invalid SUDO_UID")?;
    if sudo_uid != uid {
        bail!("target UID does not match the requesting user");
    }
    let sudo_user = env::var("SUDO_USER").context("missing SUDO_USER; launch through sudo")?;
    if sudo_user != user {
        bail!("target user does not match the requesting user");
    }

    let isolation_gid = resolve_isolation_gid()?;
    if requested_gid != isolation_gid {
        bail!("refusing non-cimgrp GID {requested_gid}; {ISOLATION_GROUP} has GID {isolation_gid}");
    }

    let program = &args[command_index];
    let program = allowed_program(program, &user)?;
    let user_cstr = CString::new(user.as_str()).context("invalid user name")?;
    let target_gid = Gid::from_raw(isolation_gid);
    let initgroups_result =
        unsafe { nix::libc::initgroups(user_cstr.as_ptr(), isolation_gid as i32) };
    if initgroups_result != 0 {
        bail!(
            "initialize target supplementary groups: {}",
            std::io::Error::last_os_error()
        );
    }
    setgid(target_gid).context("set target effective GID")?;
    setuid(Uid::from_raw(uid)).context("set target UID")?;

    let mut command = Command::new(&program);
    command.args(&args[command_index + 1..]);

    // exec replaces this helper; the child therefore inherits the dropped
    // credentials and the environment preserved by sudo -E.
    let error = command.exec();
    Err(error).context("exec isolated instance")
}

fn parse_args(args: &[String]) -> Result<(u32, u32, String, usize)> {
    let mut uid = None;
    let mut gid = None;
    let mut user = None;
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--uid" => {
                index += 1;
                uid = Some(parse_id(args, index, "uid")?);
            }
            "--gid" => {
                index += 1;
                gid = Some(parse_id(args, index, "gid")?);
            }
            "--user" => {
                index += 1;
                user = Some(args.get(index).cloned().context("missing --user value")?);
            }
            "--" => {
                index += 1;
                break;
            }
            value => bail!("unknown launcher option: {value}"),
        }
        index += 1;
    }

    let uid = uid.context("missing --uid")?;
    let gid = gid.context("missing --gid")?;
    let user = user.context("missing --user")?;
    if index >= args.len() {
        bail!("missing instance program after --");
    }
    Ok((uid, gid, user, index))
}

/// Resolve the only group the privileged launcher is allowed to assign.
fn resolve_isolation_gid() -> Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/dscl")
            .args([".", "-read", "/Groups/cimgrp", "PrimaryGroupID"])
            .output()
            .context("read cimgrp from Directory Service")?;
        if !output.status.success() {
            bail!("cimgrp is not present in Directory Service");
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("PrimaryGroupID:")?.trim().parse().ok())
            .context("invalid cimgrp PrimaryGroupID")
    }

    #[cfg(not(target_os = "macos"))]
    {
        let output = Command::new("/usr/bin/getent")
            .args(["group", ISOLATION_GROUP])
            .output()
            .context("read cimgrp from group database")?;
        if !output.status.success() {
            bail!("cimgrp is not present in the group database");
        }
        String::from_utf8_lossy(&output.stdout)
            .split(':')
            .nth(2)
            .and_then(|value| value.trim().parse().ok())
            .context("invalid cimgrp GID")
    }
}

/// Resolve and validate the executable before dropping privileges.  The
/// process itself is still launched as the requesting user, but accepting an
/// arbitrary path here would make the root helper an unreviewable process
/// launcher.  App-bundled entry points are exact paths; CLI entry points are
/// restricted to the system/Homebrew/CIM bin directories.
fn allowed_program(program: &str, user: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if !path.is_absolute() {
        bail!("instance program must be an absolute path");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve instance program {}", path.display()))?;
    // Homebrew and npm commonly install the approved CLI as a symlink. Check
    // the canonical target and execute only that target. This prevents a
    // replaced allowlisted symlink from turning the helper into an arbitrary
    // launcher.
    if !is_allowed_program(&canonical, user) {
        bail!(
            "instance program is not on the CIM allowlist: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn is_allowed_program(path: &Path, user: &str) -> bool {
    const APP_ENTRYPOINTS: &[&str] = &[
        "/Applications/Claude.app/Contents/MacOS/Claude",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        "/Applications/Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code",
    ];
    if APP_ENTRYPOINTS
        .iter()
        .any(|candidate| path == Path::new(candidate))
    {
        return true;
    }

    let user_home = format!("/Users/{user}");
    let allowed_cli = [
        "/usr/local/bin/claude".to_string(),
        "/opt/homebrew/bin/claude".to_string(),
        "/usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe".to_string(),
        "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe".to_string(),
        "/usr/local/bin/code".to_string(),
        "/opt/homebrew/bin/code".to_string(),
        format!("{user_home}/.local/bin/claude"),
        format!("{user_home}/.cim/bin/claude"),
        format!("{user_home}/.local/bin/code"),
        format!("{user_home}/.cim/bin/code"),
    ];
    allowed_cli
        .iter()
        .any(|candidate| path == Path::new(candidate))
}

fn parse_id(args: &[String], index: usize, name: &str) -> Result<u32> {
    args.get(index)
        .with_context(|| format!("missing --{name} value"))?
        .parse()
        .with_context(|| format!("invalid --{name} value"))
}

#[cfg(test)]
mod tests {
    use super::{is_allowed_program, parse_args, ISOLATION_GROUP};
    use std::path::Path;

    #[test]
    fn parser_requires_fixed_gid_argument_and_rejects_unknown_options() {
        let args = [
            "cim-instance-launch".to_string(),
            "--uid".to_string(),
            "501".to_string(),
            "--gid".to_string(),
            "999".to_string(),
            "--user".to_string(),
            "alice".to_string(),
            "--".to_string(),
            "/Applications/Claude.app/Contents/MacOS/Claude".to_string(),
        ];
        let (_, gid, user, index) = parse_args(&args).expect("valid launch syntax");
        assert_eq!(gid, 999);
        assert_eq!(user, "alice");
        assert_eq!(
            args[index],
            "/Applications/Claude.app/Contents/MacOS/Claude"
        );
        assert_eq!(ISOLATION_GROUP, "cimgrp");

        let mut bad = args.to_vec();
        bad[1] = "--root".to_string();
        assert!(parse_args(&bad).is_err());
    }

    #[test]
    fn allowlist_accepts_managed_entrypoints() {
        assert!(is_allowed_program(
            Path::new("/Applications/Claude.app/Contents/MacOS/Claude"),
            "alice"
        ));
        assert!(is_allowed_program(
            Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            "alice"
        ));
        assert!(is_allowed_program(
            Path::new("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
            "alice"
        ));
        assert!(is_allowed_program(
            Path::new("/Users/alice/.cim/bin/claude"),
            "alice"
        ));
        assert!(is_allowed_program(
            Path::new("/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"),
            "alice"
        ));
    }

    #[test]
    fn allowlist_rejects_shells_and_other_arbitrary_programs() {
        assert!(!is_allowed_program(Path::new("/bin/sh"), "alice"));
        assert!(!is_allowed_program(Path::new("/usr/bin/id"), "alice"));
        assert!(!is_allowed_program(
            Path::new("/Users/alice/.cim/bin/other"),
            "alice"
        ));
        assert!(!is_allowed_program(
            Path::new("/Users/bob/.cim/bin/claude"),
            "alice"
        ));
    }
}
