// Claude Desktop adapter (Instance B) - Desktop Lite host isolation.
//
// The Lite backend deliberately launches the app binary directly instead of
// using LaunchServices (`open -a`). This lets the root-owned CIM launcher add
// cimgrp and lets us inject an isolated HOME, proxy and locale environment.
// It is a network-hardened best-effort backend; CoreLocation remains outside
// the guarantees of this adapter and is reported by the API layer as partial.
use super::{
    apply_instance_directories, apply_locale_env, apply_proxy_environment,
    clear_instance_environment, descendant_pids_checked, isolated_command, resolve_executable,
    terminate_process_tree, terminate_processes, CapabilityReport, InstanceAdapter, ProcessHandle,
    HTTP_PROXY_ENDPOINT,
};
use crate::geoip::SharedGeoProfile;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const CLAUDE_APP_PATH: &str = "/Applications/Claude.app";
const CLAUDE_BINARY_PATH: &str = "/Applications/Claude.app/Contents/MacOS/Claude";
const CLAUDE_APP_MARKER: &str = "/Applications/Claude.app/Contents/";
// Keep the Desktop child environment deterministic. In particular, do not
// pass through SSH agents, cloud credentials, API keys, or a user-controlled
// PATH to an app that can load extensions and helper processes.
const DESKTOP_SAFE_PATH: &str =
    "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/opt/homebrew/sbin";

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

    /// Remove every Claude.app process family before the daemon publishes a
    /// new managed Desktop instance. CIM intentionally owns all Claude.app
    /// windows on this host; Claude Code CLI and Claude Web are separate
    /// process families and are not matched by this marker.
    pub async fn cleanup_orphaned_processes(&self) -> Result<()> {
        let marker = self.profile_dir.clone();
        let orphaned = tokio::task::spawn_blocking(move || processes_with_claude_app(&marker))
            .await
            .context("Desktop orphan scan task failed")??;
        if !orphaned.is_empty() {
            terminate_processes(&orphaned).await;
        }
        Ok(())
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
    // macOS Keychain lookup follows the login user's HOME/search list. Keep
    // that host value so Claude can use its existing "Claude Key" item while
    // the application data and caches remain in the CIM profile.
    let host_home = std::env::var("HOME").unwrap_or_else(|_| profile_dir.to_string());
    let proxy_arg = format!("--proxy-server={HTTP_PROXY_ENDPOINT}");

    // `isolated_command` may use `sudo -E`; clear the daemon's entire
    // inherited environment before adding the small instance allowlist.
    clear_instance_environment(command, DESKTOP_SAFE_PATH);
    apply_instance_directories(command, profile_dir, &host_home);
    apply_proxy_environment(command);
    command.args([
        &format!("--user-data-dir={user_data_dir}"),
        &proxy_arg,
        "--disable-quic",
    ]);
    if let Some(language) = profile.language.as_deref() {
        command.arg(format!("--lang={language}"));
        let primary_language = language.split('-').next().unwrap_or(language);
        command.arg("-AppleLanguages");
        command.arg(format!("({primary_language})"));
    }
    if let Some(locale) = profile.locale.as_deref() {
        let apple_locale = locale.split('.').next().unwrap_or(locale);
        command.arg("-AppleLocale");
        command.arg(apple_locale);
    }
    apply_locale_env(command, profile);
}

/// Create one profile directory without following a pre-existing symlink and
/// make it private to the current user. The final path is checked with
/// `symlink_metadata` so dangling symlinks are rejected as well.
fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing Desktop Lite profile symlink at {}",
                    path.display()
                );
            }
            if !metadata.is_dir() {
                anyhow::bail!(
                    "Desktop Lite profile path is not a directory: {}",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| {
                format!("failed to create Desktop Lite directory {}", path.display())
            })?;
            // Re-check after creation. This also makes the invariant explicit
            // if a concurrent process replaced the path while it was created.
            let metadata = fs::symlink_metadata(path).with_context(|| {
                format!(
                    "failed to inspect Desktop Lite directory {}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "Desktop Lite profile path changed while creating: {}",
                    path.display()
                );
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect Desktop Lite directory {}",
                    path.display()
                )
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to set Desktop Lite directory permissions {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn secure_private_tree(path: &Path) -> Result<()> {
    ensure_private_directory(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "refusing symlink in Desktop profile backup: {}",
                child.display()
            );
        }
        if metadata.is_dir() {
            secure_private_tree(&child)?;
        } else if metadata.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&child, fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    Ok(())
}

/// Reset only the Chromium local-storage database when Claude's own persisted
/// SPA locale disagrees with the current GeoIP-derived language. Cookies,
/// IndexedDB, and the rest of the Desktop profile remain in place. The old
/// directory is retained as a private backup so a failed migration is
/// recoverable without asking the user to sign in again.
fn migrate_persisted_locale(profile_dir: &str, expected_language: Option<&str>) -> Result<()> {
    let Some(expected) = expected_language else {
        return Ok(());
    };
    let local_storage = Path::new(profile_dir).join("user-data/Local Storage");
    let metadata = match fs::symlink_metadata(&local_storage) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect Desktop local storage"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "refusing Desktop local storage symlink or non-directory: {}",
            local_storage.display()
        );
    }

    let leveldb = local_storage.join("leveldb");
    let mut mismatch = false;
    if let Ok(entries) = fs::read_dir(&leveldb) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if stored_locale_mismatches(&bytes, expected) {
                mismatch = true;
                break;
            }
        }
    }
    if !mismatch {
        return Ok(());
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let backup =
        Path::new(profile_dir).join(format!("user-data/Local Storage.cim-backup-{timestamp}"));
    fs::rename(&local_storage, &backup).with_context(|| {
        format!(
            "failed to back up stale Desktop locale storage {}",
            local_storage.display()
        )
    })?;
    if let Err(error) = secure_private_tree(&backup) {
        let _ = fs::rename(&backup, &local_storage);
        return Err(error).context("secure Desktop locale backup");
    }
    if let Err(error) = ensure_private_directory(&local_storage) {
        let _ = fs::rename(&backup, &local_storage);
        return Err(error);
    }
    tracing::info!(
        expected_language = expected,
        backup = %backup.display(),
        "reset stale Claude Desktop persisted locale"
    );
    Ok(())
}

fn stored_locale_mismatches(bytes: &[u8], expected: &str) -> bool {
    const KEY: &[u8] = b"spa:locale";
    let expected = expected.to_ascii_lowercase();
    let mut start = 0;
    while let Some(relative) = bytes[start..]
        .windows(KEY.len())
        .position(|window| window == KEY)
    {
        let key_end = start + relative + KEY.len();
        let tail = &bytes[key_end..bytes.len().min(key_end + 64)];
        let mut value = String::new();
        for byte in tail {
            if byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_' {
                value.push(*byte as char);
            } else if !value.is_empty() {
                break;
            }
        }
        if !value.is_empty() && value.to_ascii_lowercase() != expected {
            return true;
        }
        start = key_end;
    }
    false
}

fn command_has_profile_marker(command: &str, profile_dir: &str) -> bool {
    let user_data_marker = format!("--user-data-dir={profile_dir}/user-data");
    let home_marker = format!("HOME={profile_dir}");
    let tmp_marker = format!("TMPDIR={profile_dir}/tmp");
    command.split_whitespace().any(|argument| {
        argument == user_data_marker || argument == home_marker || argument == tmp_marker
    })
}

/// Discover the complete Claude.app process family. Electron helpers carry
/// paths under the app bundle even after they detach from the main process.
/// Ownership is classified separately by the exact CIM profile markers.
fn command_is_claude_executable(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .map(|executable| executable.starts_with(CLAUDE_APP_MARKER))
        .unwrap_or(false)
}

fn parse_desktop_processes(output: &str, profile_dir: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            let marker = command_is_claude_executable(&command)
                || command_has_profile_marker(&command, profile_dir);
            marker.then(|| pid.parse::<u32>().ok()).flatten()
        })
        .collect()
}

fn parse_profile_processes(output: &str, profile_dir: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            command_has_profile_marker(&command, profile_dir)
                .then(|| pid.parse::<u32>().ok())
                .flatten()
        })
        .collect()
}

fn processes_with_claude_app(profile_dir: &str) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-eww", "-axo", "pid=,command="])
        .output()
        .context("failed to run ps for Claude process scan")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "ps failed for Claude process scan: {}",
            if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            }
        );
    }
    Ok(parse_desktop_processes(
        &String::from_utf8_lossy(&output.stdout),
        profile_dir,
    ))
}

fn processes_with_profile(profile_dir: &str) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-eww", "-axo", "pid=,command="])
        .output()
        .context("failed to run ps for Desktop profile scan")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "ps failed for Desktop profile scan: {}",
            if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            }
        );
    }
    Ok(parse_profile_processes(
        &String::from_utf8_lossy(&output.stdout),
        profile_dir,
    ))
}

fn unmanaged_processes(all: &[u32], owned: &[u32]) -> Vec<u32> {
    all.iter()
        .copied()
        .filter(|pid| !owned.contains(pid))
        .collect()
}

async fn terminate_desktop_processes(root_pid: u32, profile_dir: &str) {
    let marker = profile_dir.to_owned();
    let claude_pids = tokio::task::spawn_blocking(move || processes_with_claude_app(&marker))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    terminate_process_tree(root_pid).await;
    if !claude_pids.is_empty() {
        terminate_processes(&claude_pids).await;
    }
    let marker = profile_dir.to_owned();
    let remaining_pids = tokio::task::spawn_blocking(move || processes_with_claude_app(&marker))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    if !remaining_pids.is_empty() {
        terminate_processes(&remaining_pids).await;
    }
}

#[async_trait]
impl InstanceAdapter for DesktopAdapter {
    async fn cleanup_orphaned_sessions(&self) -> Result<()> {
        self.cleanup_orphaned_processes().await
    }

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
            PathBuf::from(&self.profile_dir),
            PathBuf::from(format!("{}/user-data", self.profile_dir)),
            PathBuf::from(format!("{}/config", self.profile_dir)),
            PathBuf::from(format!("{}/cache", self.profile_dir)),
            PathBuf::from(format!("{}/data", self.profile_dir)),
            PathBuf::from(format!("{}/tmp", self.profile_dir)),
        ] {
            ensure_private_directory(&directory)?;
        }
        // CIM owns the complete Claude.app process family on this host. A
        // daemon restart or a manually launched copy must not remain outside
        // the proxy/PF lifecycle before a new controlled launch.
        let marker = self.profile_dir.clone();
        let orphaned = tokio::task::spawn_blocking(move || processes_with_claude_app(&marker))
            .await
            .context("Desktop orphan scan task failed")??;
        if !orphaned.is_empty() {
            terminate_processes(&orphaned).await;
        }
        let profile = self.geo_profile.read().await.clone();
        migrate_persisted_locale(&self.profile_dir, profile.language.as_deref())?;
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
        let has_marked_process = tokio::task::spawn_blocking(move || {
            processes_with_profile(&marker).map(|pids| !pids.is_empty())
        })
        .await
        .context("Desktop startup process scan task failed")??;
        if !has_marked_process {
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
        self.process_tree_checked(handle)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(error = %error, "Desktop process scan failed during best-effort cleanup");
                vec![handle.root_pid]
            })
    }

    async fn process_tree_checked(&self, handle: &ProcessHandle) -> Result<Vec<u32>> {
        let profile_dir = self.profile_dir.clone();
        let scan_dir = profile_dir.clone();
        let (all_pids, marker_pids) = tokio::task::spawn_blocking(move || {
            (
                processes_with_claude_app(&scan_dir),
                processes_with_profile(&profile_dir),
            )
        })
        .await
        .context("Desktop process scan task failed")?;
        let all_pids = all_pids?;
        let marker_pids = marker_pids?;

        // The daemon owns the complete Claude.app family. A process without
        // the CIM profile markers is an unmanaged manual/legacy launch; stop
        // it before it can be included in the Desktop security conclusion.
        let unmanaged = unmanaged_processes(&all_pids, &marker_pids);
        if !unmanaged.is_empty() {
            tracing::warn!(pids = ?unmanaged, "Stopping unmanaged Claude.app processes");
            terminate_processes(&unmanaged).await;
        }

        let mut pids = descendant_pids_checked(handle.root_pid)?;
        pids.extend(marker_pids);
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configure_desktop_command, ensure_private_directory, migrate_persisted_locale,
        parse_desktop_processes, parse_profile_processes, unmanaged_processes, DESKTOP_SAFE_PATH,
    };
    use crate::geoip::GeoProfile;
    use std::fs;
    use std::path::Path;
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
            language: Some("en-US".to_string()),
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
        assert!(args.iter().any(|arg| arg == "--lang=en-US"));
        assert!(args.iter().any(|arg| arg == "-AppleLanguages"));
        assert!(args.iter().any(|arg| arg == "(en)"));
        assert!(args.iter().any(|arg| arg == "-AppleLocale"));
        assert!(args.iter().any(|arg| arg == "en_US"));
        let host_home = std::env::var("HOME").expect("test HOME");
        assert_eq!(
            env_value(&command, "HOME").as_deref(),
            Some(host_home.as_str())
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
            Some("socks5h://127.0.0.1:7891")
        );
        assert_eq!(
            env_value(&command, "TZ").as_deref(),
            Some("America/New_York")
        );
    }

    #[test]
    fn desktop_command_drops_sensitive_inherited_environment() {
        let profile = GeoProfile::default();
        let mut command = Command::new(super::CLAUDE_BINARY_PATH);
        command
            .env("AWS_ACCESS_KEY_ID", "should-not-reach-desktop")
            .env("SSH_AUTH_SOCK", "/private/tmp/agent.sock")
            .env("GITHUB_TOKEN", "should-not-reach-desktop")
            .env("PATH", "/tmp/user-controlled");

        configure_desktop_command(&mut command, "/tmp/cim-desktop", &profile);

        assert_eq!(
            env_value(&command, "PATH").as_deref(),
            Some(DESKTOP_SAFE_PATH)
        );
        assert!(env_value(&command, "AWS_ACCESS_KEY_ID").is_none());
        assert!(env_value(&command, "SSH_AUTH_SOCK").is_none());
        assert!(env_value(&command, "GITHUB_TOKEN").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn desktop_profile_directories_are_private_and_reject_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root =
            std::env::temp_dir().join(format!("cim-desktop-profile-test-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        let child = profile.join("user-data");
        fs::create_dir_all(&child).expect("create test profile");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755))
            .expect("make profile deliberately broad");
        fs::set_permissions(&child, fs::Permissions::from_mode(0o755))
            .expect("make child deliberately broad");

        ensure_private_directory(Path::new(&profile)).expect("harden profile");
        ensure_private_directory(Path::new(&child)).expect("harden child");
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let symlink_path = root.join("symlink-profile");
        symlink(&profile, &symlink_path).expect("create test symlink");
        let error = ensure_private_directory(Path::new(&symlink_path))
            .expect_err("profile symlink must be rejected");
        assert!(error.to_string().contains("symlink"));

        fs::remove_dir_all(&root).expect("remove test profile");
    }

    #[test]
    fn stale_desktop_locale_is_backed_up_without_removing_profile_data() {
        let root =
            std::env::temp_dir().join(format!("cim-desktop-locale-test-{}", uuid::Uuid::new_v4()));
        let local_storage = root.join("user-data/Local Storage/leveldb");
        fs::create_dir_all(&local_storage).expect("create local storage");
        fs::write(
            local_storage.join("000003.log"),
            b"spa:locale\x08\x01zh-Hans",
        )
        .expect("write locale record");
        fs::write(root.join("user-data/Cookies"), b"keep-login-state")
            .expect("write cookie marker");

        migrate_persisted_locale(root.to_str().unwrap(), Some("en-US"))
            .expect("migrate stale locale");
        assert!(root.join("user-data/Local Storage").is_dir());
        assert!(root.join("user-data/Cookies").is_file());
        let backup_count = fs::read_dir(root.join("user-data"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("Local Storage.cim-backup-")
            })
            .count();
        assert_eq!(backup_count, 1);
        fs::remove_dir_all(root).expect("remove test profile");
    }

    #[test]
    fn desktop_marker_matches_only_its_profile() {
        let profile = "/tmp/cim-desktop";
        let ps = format!(
            " 101 /Applications/Claude.app/Contents/MacOS/Claude --user-data-dir={profile}/user-data\n 102 /Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper --type=renderer\n 103 helper --user-data-dir={profile}-other/user-data\n 104 helper HOME={profile}\n 105 helper HOME={profile}-other\n 106 /bin/sh -c echo /Applications/Claude.app/Contents/MacOS/Claude\n"
        );
        assert_eq!(parse_desktop_processes(&ps, profile), vec![101, 102, 104]);
        assert_eq!(parse_profile_processes(&ps, profile), vec![101, 104]);
        assert_eq!(
            unmanaged_processes(&[101, 102, 104], &[101, 104]),
            vec![102]
        );
    }
}
