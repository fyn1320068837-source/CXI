use super::cdp::{
    apply_browser_profile, find_free_port, monitor_browser_requests, monitor_browser_socket,
    monitor_browser_targets, processes_using_session_dir,
};
use super::*;
use async_trait::async_trait;

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

        let mut args = vec![
            format!("--user-data-dir={}", session_dir),
            "--proxy-server=socks5://127.0.0.1:7891".to_string(),
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
            format!("--remote-debugging-port={}", debug_port),
            "--remote-allow-origins=*".to_string(),
        ];
        if let Some(language) = profile.language.as_deref() {
            args.push(format!("--lang={}", language));
        }
        // Do not navigate before DevTools has installed the locale, timezone,
        // geolocation and Accept-Language overrides. Otherwise the first
        // document is fetched with the host locale and only a later refresh
        // reflects the proxy's GeoIP profile.
        args.push("about:blank".to_string());

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
                        terminate_process_tree(root_pid).await;
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
        terminate_process_tree(handle.root_pid).await;
        if let Some(session_dir) = self.sessions.lock().unwrap().remove(&handle.root_pid) {
            let _ = std::fs::remove_dir_all(session_dir);
        }
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        let mut pids = descendant_pids(handle.root_pid);
        if let Some(session_dir) = self.sessions.lock().unwrap().get(&handle.root_pid).cloned() {
            pids.extend(processes_using_session_dir(&session_dir));
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}
