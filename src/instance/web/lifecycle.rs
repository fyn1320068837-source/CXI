use super::cdp::{
    apply_browser_profile, find_free_port, monitor_browser_requests, monitor_browser_socket,
    monitor_browser_targets, processes_using_session_dir,
};
use super::*;
use crate::instance::{terminate_process_tree, terminate_processes, SOCKS_PROXY_ENDPOINT};
use async_trait::async_trait;

async fn terminate_web_processes(root_pid: u32, session_dir: Option<&str>) {
    let marker_pids = if let Some(session_dir) = session_dir {
        let marker = session_dir.to_owned();
        tokio::task::spawn_blocking(move || processes_using_session_dir(&marker))
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    terminate_process_tree(root_pid).await;

    if let Some(session_dir) = session_dir {
        let marker = session_dir.to_owned();
        let remaining_marker_pids =
            tokio::task::spawn_blocking(move || processes_using_session_dir(&marker))
                .await
                .unwrap_or_default();
        let mut pids = marker_pids;
        pids.extend(remaining_marker_pids);
        pids.sort_unstable();
        pids.dedup();
        if !pids.is_empty() {
            terminate_processes(&pids).await;
        }
    }
}

pub(crate) async fn cleanup_orphaned_sessions(adapter: &WebAdapter) -> Result<()> {
    let sessions_root = std::path::Path::new(&adapter.user_data_dir).join("sessions");
    if !sessions_root.exists() {
        return Ok(());
    }

    let active_sessions = adapter
        .sessions
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<std::collections::HashSet<_>>();

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

fn browser_launch_args(session_dir: &str, debug_port: u16, profile: &GeoProfile) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={session_dir}"),
        format!("--proxy-server={SOCKS_PROXY_ENDPOINT}"),
        "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".to_string(),
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
        cleanup_orphaned_sessions(self).await?;
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

        let args = browser_launch_args(&session_dir, debug_port, &profile);

        let mut command = isolated_command(&browser_path);
        command.args(&args);
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
                        terminate_web_processes(root_pid, Some(&session_dir)).await;
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
        terminate_web_processes(handle.root_pid, session_dir.as_deref()).await;
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
                tokio::task::spawn_blocking(move || processes_using_session_dir(&marker))
                    .await
                    .unwrap_or_default();
            pids.extend(marker_pids);
        }
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
    fn web_command_uses_unique_session_proxy_and_network_guards() {
        let profile = GeoProfile {
            language: Some("en-US".to_string()),
            ..GeoProfile::default()
        };
        let args = browser_launch_args("/tmp/cim-web/sessions/test-session", 4567, &profile);

        assert!(args
            .iter()
            .any(|arg| { arg == "--user-data-dir=/tmp/cim-web/sessions/test-session" }));
        assert!(args
            .iter()
            .any(|arg| arg == "--proxy-server=socks5://127.0.0.1:7891"));
        assert!(args
            .iter()
            .any(|arg| { arg == "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1" }));
        assert!(args
            .iter()
            .any(|arg| arg == "--force-webrtc-ip-handling-policy=disable_non_proxied_udp"));
        assert!(args.iter().any(|arg| arg == "--disable-quic"));
        assert!(args.iter().any(|arg| arg == "--lang=en-US"));
        assert_eq!(args.last().map(String::as_str), Some("about:blank"));
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
}
