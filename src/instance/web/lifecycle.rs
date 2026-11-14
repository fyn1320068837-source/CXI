use super::cdp::{
    apply_browser_profile, find_free_port, monitor_browser_requests, monitor_browser_socket,
    monitor_browser_targets, processes_using_session_dir, processes_using_session_marker,
};
use super::*;
use crate::instance::{terminate_process_tree, terminate_processes, SOCKS_PROXY_ENDPOINT};
use async_trait::async_trait;

async fn terminate_web_processes(
    root_pid: u32,
    session_dir: Option<&str>,
    profile_dir: Option<&str>,
) {
    let marker_pids = if let Some(session_dir) = session_dir {
        let marker = session_dir.to_owned();
        tokio::task::spawn_blocking(move || processes_using_session_marker(&marker))
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let profile_pids = if let Some(profile_dir) = profile_dir {
        let profile = profile_dir.to_owned();
        tokio::task::spawn_blocking(move || processes_using_session_dir(&profile))
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    terminate_process_tree(root_pid).await;

    if session_dir.is_some() || profile_dir.is_some() {
        let remaining_marker_pids = if let Some(session_dir) = session_dir {
            let marker = session_dir.to_owned();
            tokio::task::spawn_blocking(move || processes_using_session_marker(&marker))
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut pids = marker_pids;
        pids.extend(profile_pids);
        pids.extend(remaining_marker_pids);
        pids.sort_unstable();
        pids.dedup();
        if !pids.is_empty() {
            terminate_processes(&pids).await;
        }
    }
}

fn migrate_legacy_profile(adapter: &WebAdapter) -> Result<()> {
    let profile = std::path::Path::new(&adapter.profile_dir);
    let target_default = profile.join("Default");
    let legacy_default = std::path::Path::new(&adapter.user_data_dir).join("Default");
    if target_default.exists() || !legacy_default.exists() {
        return Ok(());
    }

    // Older CIM releases used the instance root itself as Chrome's
    // user-data-dir. Move only the profile data and Local State; leave the
    // sessions directory under CIM's lifecycle control.
    std::fs::rename(&legacy_default, &target_default)?;
    let legacy_local_state = std::path::Path::new(&adapter.user_data_dir).join("Local State");
    let target_local_state = profile.join("Local State");
    if legacy_local_state.exists() && !target_local_state.exists() {
        std::fs::rename(legacy_local_state, target_local_state)?;
    }
    tracing::info!(profile = %profile.display(), "Migrated legacy Web profile into persistent isolated directory");
    Ok(())
}

fn clear_profile_locks(adapter: &WebAdapter) -> Result<()> {
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let path = std::path::Path::new(&adapter.profile_dir).join(name);
        if path.exists() || std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn language_list(language: &str) -> String {
    let base = language.split('-').next().unwrap_or(language);
    if base == language {
        language.to_string()
    } else {
        format!("{language},{base}")
    }
}

fn update_chrome_language_preference(path: &std::path::Path, language: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(path)?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid Chrome preference JSON: {}", path.display()))?;
    value["intl"]["selected_languages"] = serde_json::Value::String(language_list(language));

    // Chrome may be starting with a freshly migrated profile. Replace the
    // file atomically so a crash cannot leave the persistent profile invalid.
    let temporary = path.with_extension("cim-language.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn sync_persistent_language(adapter: &WebAdapter, profile: &GeoProfile) -> Result<()> {
    let Some(language) = profile.language.as_deref() else {
        return Ok(());
    };
    let profile_root = std::path::Path::new(&adapter.profile_dir);
    update_chrome_language_preference(&profile_root.join("Local State"), language)?;
    update_chrome_language_preference(&profile_root.join("Default").join("Preferences"), language)?;
    tracing::debug!(language, "Synchronized persistent Web language preference");
    Ok(())
}

pub(crate) async fn cleanup_orphaned_sessions(adapter: &WebAdapter) -> Result<()> {
    let sessions_root = std::path::Path::new(&adapter.user_data_dir).join("sessions");
    let active_sessions = adapter
        .sessions
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<std::collections::HashSet<_>>();

    // A daemon restart can leave the persistent-profile Chrome tree behind.
    // The Web runtime is single-instance, so when no session is active these
    // are stale CIM-owned processes and can be terminated safely. Keep the
    // profile directory itself so login and Cloudflare clearance cookies stay.
    if active_sessions.is_empty() {
        let profile_dir = adapter.profile_dir.clone();
        let profile_pids =
            tokio::task::spawn_blocking(move || processes_using_session_dir(&profile_dir))
                .await
                .unwrap_or_default();
        if !profile_pids.is_empty() {
            terminate_processes(&profile_pids).await;
        }
        let legacy_root = adapter.user_data_dir.clone();
        let legacy_pids =
            tokio::task::spawn_blocking(move || processes_using_session_dir(&legacy_root))
                .await
                .unwrap_or_default();
        if !legacy_pids.is_empty() {
            terminate_processes(&legacy_pids).await;
        }
    }

    if !sessions_root.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path_string = path.to_string_lossy().into_owned();
        if active_sessions.contains(&path_string) {
            continue;
        }

        let marker = path_string.clone();
        // Match both the old per-session user-data-dir and the new explicit
        // marker so a daemon upgrade also cleans pre-existing orphan trees.
        let marker_pids = tokio::task::spawn_blocking(move || processes_using_session_dir(&marker))
            .await
            .unwrap_or_default();
        if !marker_pids.is_empty() {
            terminate_processes(&marker_pids).await;
        }
        std::fs::remove_dir_all(&path)?;
        tracing::info!(session_dir = %path_string, "Removed orphaned Web session");
    }
    Ok(())
}

fn browser_launch_args(
    profile_dir: &str,
    session_dir: &str,
    debug_port: u16,
    profile: &GeoProfile,
) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={profile_dir}"),
        // Keep a per-launch marker separate from the persistent profile. It
        // lets cleanup find detached Chromium helpers without ever deleting
        // the profile that contains login and clearance cookies.
        format!("--cim-session-dir={session_dir}"),
        format!("--proxy-server={SOCKS_PROXY_ENDPOINT}"),
        "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".to_string(),
        // Chrome shows an infobar for the host-resolver-rules switch even
        // though it is the supported Chromium mechanism for blocking local
        // DNS prefetch. Chromium's webdriver test mode suppresses that
        // warning without removing the resolver rule itself.
        "--test-type=webdriver".to_string(),
        // Do not allow WebRTC or HTTP/3 to create UDP paths outside the
        // SOCKS5 tunnel. The process-level verifier still checks the
        // resulting sockets, but these flags remove the browser's direct
        // UDP fallback paths up front.
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
        "--disable-quic".to_string(),
        "--disable-background-networking".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        format!("--remote-debugging-port={debug_port}"),
        "--remote-allow-origins=*".to_string(),
    ];
    if let Some(language) = profile.language.as_deref() {
        args.push(format!("--lang={language}"));
    }
    // Do not navigate before DevTools has installed the locale, timezone,
    // geolocation and Accept-Language overrides. Otherwise the first
    // document is fetched with the host locale and only a later refresh
    // reflects the proxy's GeoIP profile.
    args.push("about:blank".to_string());
    args
}

#[async_trait]
impl InstanceAdapter for WebAdapter {
    async fn detect(&self) -> CapabilityReport {
        // Try to find Chromium or Chrome
        let browsers = vec![
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ];

        for path in browsers {
            if std::path::Path::new(path).exists() {
                return CapabilityReport {
                    installed: true,
                    version: None,
                    path: Some(path.to_string()),
                };
            }
        }

        CapabilityReport {
            installed: false,
            version: None,
            path: None,
        }
    }

    async fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.user_data_dir)?;
        std::fs::create_dir_all(&self.profile_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.profile_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        cleanup_orphaned_sessions(self).await?;
        migrate_legacy_profile(self)?;
        clear_profile_locks(self)?;
        let profile = self.geo_profile.read().await.clone();
        sync_persistent_language(self, &profile)?;
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        let report = self.detect().await;
        let browser_path = report
            .path
            .ok_or_else(|| anyhow::anyhow!("No browser found"))?;
        let profile = self.geo_profile.read().await.clone();
        let debug_port = find_free_port()?;
        let session_dir = format!("{}/sessions/{}", self.user_data_dir, Uuid::new_v4());
        std::fs::create_dir_all(&session_dir)?;

        if !profile.is_ready() {
            let _ = std::fs::remove_dir_all(&session_dir);
            anyhow::bail!(
                "GeoIP profile is not ready; refusing to navigate Web with host locale/timezone"
            );
        }

        let args = browser_launch_args(&self.profile_dir, &session_dir, debug_port, &profile);

        let mut command = isolated_command(&browser_path);
        command.args(&args);
        // Keep the user's macOS HOME/keychain search list. Chrome's cookie
        // encryption and App-Bound Encryption use the login keychain; moving
        // HOME to the CIM data directory makes Chrome ask for a separate,
        // nonexistent keychain. Profile and network isolation are enforced
        // by --user-data-dir, cimgrp and PF instead.
        apply_locale_env(&mut command, &profile);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&session_dir);
                return Err(error.into());
            }
        };
        let root_pid = child.id();

        if profile.is_ready() {
            let capture_requests = self.request_log_tx.is_some();
            let browser_session =
                match apply_browser_profile(debug_port, &profile, capture_requests).await {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::error!(
                            port = debug_port,
                            error = %error,
                            chain = %format!("{:#}", error),
                            "Failed to apply browser locale/timezone profile"
                        );
                        terminate_web_processes(
                            root_pid,
                            Some(&session_dir),
                            Some(&self.profile_dir),
                        )
                        .await;
                        let _ = std::fs::remove_dir_all(&session_dir);
                        return Err(
                            error.context("Failed to apply browser locale/timezone profile")
                        );
                    }
                };

            let initial_target_id = browser_session.target_id.clone();

            // Keep the same CDP connection that installed the overrides. A
            // second debugger client can race the profile connection on some
            // Chrome builds, so request events are consumed from this socket
            // after the first navigation instead of attaching another client.
            if let Some(sender) = self.request_log_tx.clone() {
                tokio::spawn(monitor_browser_socket(
                    browser_session.socket,
                    root_pid,
                    sender.clone(),
                ));
            }
            tokio::spawn(monitor_browser_targets(
                debug_port,
                root_pid,
                profile,
                initial_target_id,
                self.request_log_tx.clone(),
            ));
        } else if let Some(sender) = self.request_log_tx.clone() {
            // Without GeoIP data Chrome starts directly on Claude, so the
            // monitor attaches independently because there is no profile
            // connection to share.
            tokio::spawn(monitor_browser_requests(debug_port, root_pid, sender));
        }

        self.sessions.lock().unwrap().insert(root_pid, session_dir);

        Ok(ProcessHandle { root_pid })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        let session_dir = self.sessions.lock().unwrap().remove(&handle.root_pid);
        terminate_web_processes(
            handle.root_pid,
            session_dir.as_deref(),
            Some(&self.profile_dir),
        )
        .await;
        if let Some(session_dir) = session_dir {
            let _ = std::fs::remove_dir_all(session_dir);
        }
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        let mut pids = descendant_pids(handle.root_pid);
        let session_dir = self.sessions.lock().unwrap().get(&handle.root_pid).cloned();
        if let Some(session_dir) = session_dir {
            let marker = session_dir;
            let marker_pids =
                tokio::task::spawn_blocking(move || processes_using_session_marker(&marker))
                    .await
                    .unwrap_or_default();
            pids.extend(marker_pids);
        }
        let profile_dir = self.profile_dir.clone();
        let profile_pids =
            tokio::task::spawn_blocking(move || processes_using_session_dir(&profile_dir))
                .await
                .unwrap_or_default();
        pids.extend(profile_pids);
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

#[cfg(test)]
mod tests {
    use super::browser_launch_args;
    use crate::geoip::{shared_profile, GeoProfile};
    use crate::instance::WebAdapter;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn web_command_uses_persistent_profile_and_network_guards() {
        let profile = GeoProfile {
            language: Some("en-US".to_string()),
            ..GeoProfile::default()
        };
        let args = browser_launch_args(
            "/tmp/cim-web/profile",
            "/tmp/cim-web/sessions/test-session",
            4567,
            &profile,
        );

        assert!(args
            .iter()
            .any(|arg| { arg == "--user-data-dir=/tmp/cim-web/profile" }));
        assert!(args
            .iter()
            .any(|arg| { arg == "--cim-session-dir=/tmp/cim-web/sessions/test-session" }));
        assert!(args
            .iter()
            .any(|arg| arg == "--proxy-server=socks5://127.0.0.1:7891"));
        assert!(args
            .iter()
            .any(|arg| { arg == "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1" }));
        assert!(args.iter().any(|arg| arg == "--test-type=webdriver"));
        assert!(args
            .iter()
            .any(|arg| arg == "--force-webrtc-ip-handling-policy=disable_non_proxied_udp"));
        assert!(args.iter().any(|arg| arg == "--disable-quic"));
        assert!(args.iter().any(|arg| arg == "--lang=en-US"));
        assert_eq!(args.last().map(String::as_str), Some("about:blank"));
    }

    #[test]
    fn persistent_profile_is_separate_from_transient_session_marker() {
        let args = browser_launch_args(
            "/tmp/cim-web/profile",
            "/tmp/cim-web/sessions/one",
            4567,
            &GeoProfile::default(),
        );
        assert!(args
            .iter()
            .any(|arg| arg == "--user-data-dir=/tmp/cim-web/profile"));
        assert!(args
            .iter()
            .any(|arg| arg == "--cim-session-dir=/tmp/cim-web/sessions/one"));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("--user-data-dir=/tmp/cim-web/sessions/")));
    }

    #[tokio::test]
    async fn cleanup_removes_orphaned_session_directories() {
        let root = std::env::temp_dir().join(format!("cim-web-cleanup-{}", Uuid::new_v4()));
        let orphan = root.join("sessions/orphan");
        fs::create_dir_all(&orphan).expect("create orphan session fixture");
        let adapter =
            WebAdapter::new_with_geo_profile(root.to_string_lossy().into_owned(), shared_profile());

        super::cleanup_orphaned_sessions(&adapter)
            .await
            .expect("cleanup orphaned sessions");
        assert!(!orphan.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migrate_legacy_profile_moves_cookie_store_inputs_only() {
        let root = std::env::temp_dir().join(format!("cim-web-migrate-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("Default")).expect("create legacy profile");
        fs::create_dir_all(root.join("sessions/old")).expect("create sessions root");
        fs::write(root.join("Default/Cookies"), b"legacy-cookie-store").expect("write cookies");
        fs::write(root.join("Local State"), b"legacy-local-state").expect("write local state");
        let adapter =
            WebAdapter::new_with_geo_profile(root.to_string_lossy().into_owned(), shared_profile());
        fs::create_dir_all(&adapter.profile_dir).expect("create persistent profile");

        super::migrate_legacy_profile(&adapter).expect("migrate legacy profile");
        assert!(adapter.profile_dir.as_str().contains("/profile"));
        assert!(std::path::Path::new(&adapter.profile_dir)
            .join("Default/Cookies")
            .exists());
        assert!(std::path::Path::new(&adapter.profile_dir)
            .join("Local State")
            .exists());
        assert!(root.join("sessions/old").exists());
        assert!(!root.join("Default").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clear_profile_locks_removes_stale_chrome_singletons() {
        let root = std::env::temp_dir().join(format!("cim-web-locks-{}", Uuid::new_v4()));
        let profile = root.join("profile");
        fs::create_dir_all(&profile).expect("create profile");
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            fs::write(profile.join(name), b"stale").expect("create stale lock");
        }
        let adapter =
            WebAdapter::new_with_geo_profile(root.to_string_lossy().into_owned(), shared_profile());
        super::clear_profile_locks(&adapter).expect("clear stale locks");
        for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            assert!(!profile.join(name).exists());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persistent_language_updates_chrome_preferences() {
        let root = std::env::temp_dir().join(format!("cim-web-language-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("Default")).expect("create profile");
        let local_state = root.join("Local State");
        let preferences = root.join("Default/Preferences");
        fs::write(
            &local_state,
            br#"{"intl":{"selected_languages":"zh-CN,zh"}}"#,
        )
        .expect("write local state");
        fs::write(
            &preferences,
            br#"{"intl":{"selected_languages":"zh-CN,zh"}}"#,
        )
        .expect("write preferences");

        super::update_chrome_language_preference(&local_state, "en-US")
            .expect("update local state");
        super::update_chrome_language_preference(&preferences, "en-US")
            .expect("update preferences");

        for path in [local_state, preferences] {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(path).expect("read preference"))
                    .expect("parse preference");
            assert_eq!(
                value["intl"]["selected_languages"],
                serde_json::Value::String("en-US,en".to_string())
            );
        }
        let _ = fs::remove_dir_all(root);
    }
}
