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
use std::process::Command;

fn main() -> Result<()> {
    if !Uid::effective().is_root() {
        bail!("cim-instance-launch must run as root via sudo");
    }

    let args: Vec<String> = env::args().collect();
    let (uid, gid, user, command_index) = parse_args(&args)?;
    if uid == 0 {
        bail!("refusing to launch an instance as root");
    }

    if let Some(sudo_uid) = env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        if sudo_uid != uid {
            bail!("target UID does not match the requesting user");
        }
    }

    let user_cstr = CString::new(user.as_str()).context("invalid user name")?;
    let target_gid = Gid::from_raw(gid);
    let initgroups_result = unsafe { nix::libc::initgroups(user_cstr.as_ptr(), gid as i32) };
    if initgroups_result != 0 {
        bail!(
            "initialize target supplementary groups: {}",
            std::io::Error::last_os_error()
        );
    }
    setgid(target_gid).context("set target effective GID")?;
    setuid(Uid::from_raw(uid)).context("set target UID")?;

    let program = &args[command_index];
    let mut command = Command::new(program);
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

fn parse_id(args: &[String], index: usize, name: &str) -> Result<u32> {
    args.get(index)
        .with_context(|| format!("missing --{name} value"))?
        .parse()
        .with_context(|| format!("invalid --{name} value"))
}
