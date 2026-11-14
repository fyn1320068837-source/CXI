use super::*;

impl ProxyManager {
    /// Start sing-box process
    pub async fn start(&self) -> Result<()> {
        self.mark_unhealthy();
        {
            let mut process = self.sing_box_process.lock().unwrap();
            if let Some(child) = process.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        warn!("sing-box already running");
                        return Ok(());
                    }
                    Ok(Some(_)) => {
                        *process = None;
                    }
                    Err(error) => {
                        warn!("Could not check existing sing-box status: {}", error);
                        *process = None;
                    }
                }
            }
        }

        info!("Starting sing-box from {:?}", self.config.binary_path);

        // Generate config
        self.generate_config().await?;

        // Start sing-box, capture stderr for diagnostics
        let child = Command::new(&self.config.binary_path)
            .arg("run")
            .arg("-c")
            .arg(&self.config.config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start sing-box")?;

        info!("sing-box started with PID: {}", child.id());
        *self.sing_box_process.lock().unwrap() = Some(child);

        // Wait longer for sing-box to initialize with upstream proxy
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify it's still alive
        let mut process = self.sing_box_process.lock().unwrap();
        if let Some(child) = process.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    *process = None;
                    anyhow::bail!("sing-box exited immediately with status: {}", status);
                }
                Ok(None) => {
                    info!("sing-box is running and stable");
                    // The process is stable, but the proxy is not considered
                    // healthy until an exit-IP probe succeeds.
                }
                Err(e) => {
                    warn!("Could not check sing-box status: {}", e);
                }
            }
        }

        Ok(())
    }

    /// Stop sing-box process
    pub async fn stop(&self) -> Result<()> {
        self.mark_unhealthy();
        let child = self.sing_box_process.lock().unwrap().take();
        if let Some(mut child) = child {
            info!("Stopping sing-box PID: {}", child.id());

            // Try graceful shutdown first
            let _ = child.kill();

            // Wait for process to exit
            match tokio::time::timeout(Duration::from_secs(5), async {
                tokio::task::spawn_blocking(move || child.wait()).await
            })
            .await
            {
                Ok(_) => info!("sing-box stopped gracefully"),
                Err(_) => warn!("sing-box did not stop in time"),
            }
        }

        self.clear_geo_profile().await;

        Ok(())
    }

    /// Check the process that this manager started, without matching unrelated
    /// sing-box processes on the host.
    pub fn is_running(&self) -> bool {
        let mut process = self.sing_box_process.lock().unwrap();
        if let Some(child) = process.as_mut() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    warn!("sing-box exited with status: {}", status);
                    *process = None;
                    self.mark_unhealthy();
                    self.clear_geo_profile_sync();
                    false
                }
                Err(error) => {
                    error!("Error checking sing-box status: {}", error);
                    self.mark_unhealthy();
                    self.clear_geo_profile_sync();
                    false
                }
            }
        } else {
            false
        }
    }

    pub fn is_healthy(&self) -> bool {
        let firewall = self.firewall.status();
        self.proxy_healthy.load(Ordering::Acquire)
            && firewall.installed
            && firewall.mode == crate::firewall::FirewallMode::Allowed
            && self.is_running()
    }

    pub fn mark_healthy(&self) {
        let firewall = self.firewall.status();
        if !firewall.installed {
            self.proxy_healthy.store(false, Ordering::Release);
            warn!("Refusing healthy proxy state while PF fail-closed is not installed");
            return;
        }
        if let Err(error) = self.firewall.set_allowed() {
            self.proxy_healthy.store(false, Ordering::Release);
            warn!(
                "Failed to clear PF fail-closed rules after proxy recovery: {}",
                error
            );
            return;
        }
        self.proxy_healthy.store(true, Ordering::Release);
    }

    pub fn mark_unhealthy(&self) {
        if let Err(error) = self.firewall.set_blocked() {
            warn!("Failed to enable PF fail-closed rules: {}", error);
        }
        // Set the health bit after PF has been asked to block so observers do
        // not see an unhealthy proxy while the anchor is still in allowed
        // mode. If PF cannot be controlled, the bit remains false and launch
        // gates refuse new instances.
        self.proxy_healthy.store(false, Ordering::Release);
    }

    pub async fn restart(&self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }
}
