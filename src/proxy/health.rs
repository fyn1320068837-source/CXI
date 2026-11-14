use super::*;

impl ProxyManager {
    /// Check exit IP through proxy using curl (more reliable for SOCKS5)
    pub async fn check_exit_ip(&self) -> Result<String> {
        let output = tokio::process::Command::new("curl")
            .args([
                "-s",
                "-x",
                "socks5://127.0.0.1:7891",
                "--max-time",
                &self.config.health_check_timeout.to_string(),
                "https://ipv4.icanhazip.com",
            ])
            .output()
            .await
            .context("Failed to run curl")?;

        if output.status.success() {
            let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !ip.is_empty() {
                info!("Exit IP: {}", ip);
                return Ok(ip);
            }
        }

        // Fallback endpoints
        for endpoint in &self.config.exit_ip_endpoints {
            let result = tokio::process::Command::new("curl")
                .args([
                    "-s",
                    "-x",
                    "socks5://127.0.0.1:7891",
                    "--max-time",
                    &self.config.health_check_timeout.to_string(),
                    endpoint.as_str(),
                ])
                .output()
                .await;

            if let Ok(out) = result {
                if out.status.success() {
                    let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !ip.is_empty() && ip.parse::<IpAddr>().is_ok() {
                        info!("Exit IP: {}", ip);
                        return Ok(ip);
                    }
                }
            }
        }

        anyhow::bail!("All exit IP endpoints failed")
    }

    /// Resolve the current exit IP and update the shared locale profile.
    pub async fn refresh_geo_profile(&self) -> Result<GeoProfile> {
        let exit_ip = self.check_exit_ip().await?;
        self.refresh_geo_profile_for_ip(&exit_ip).await
    }

    pub async fn refresh_geo_profile_for_ip(&self, exit_ip: &str) -> Result<GeoProfile> {
        let ip = parse_exit_ip(exit_ip)?;
        let ip_text = ip.to_string();
        let now = Instant::now();

        let cached_profile = {
            let cache = self.geo_cache.lock().unwrap();
            cache.entry.as_ref().and_then(|entry| {
                if entry.profile.source_ip.as_deref() == Some(ip_text.as_str())
                    && now.duration_since(entry.fetched_at) < GEO_CACHE_TTL
                {
                    Some(entry.profile.clone())
                } else {
                    None
                }
            })
        };
        if let Some(profile) = cached_profile {
            *self.geo_profile.write().await = profile.clone();
            return Ok(profile);
        }

        let rate_limited = {
            let cache = self.geo_cache.lock().unwrap();
            cache
                .last_attempt
                .map(|(last_ip, last_attempt)| {
                    last_ip == ip && now.duration_since(last_attempt) < GEO_RETRY_INTERVAL
                })
                .unwrap_or(false)
        };
        if rate_limited {
            let current = self.geo_profile.read().await.clone();
            if current.source_ip.as_deref() == Some(ip_text.as_str()) && current.has_geo() {
                return Ok(current);
            }
            anyhow::bail!("GeoIP lookup is rate limited for {}", ip);
        }

        self.geo_cache.lock().unwrap().last_attempt = Some((ip, now));

        {
            let mut profile = self.geo_profile.write().await;
            reset_profile_if_ip_changed(&mut profile, &ip_text);
        }

        let provider = IpApiProvider::new()?;
        match provider.resolve(ip).await {
            Ok(record) => {
                let profile = GeoProfile::from_record(ip, record);
                self.geo_cache.lock().unwrap().entry = Some(GeoCacheEntry {
                    profile: profile.clone(),
                    fetched_at: Instant::now(),
                });
                *self.geo_profile.write().await = profile.clone();
                Ok(profile)
            }
            Err(error) => {
                // Keep a previously resolved profile for the same exit IP so
                // a transient GeoIP outage does not reset new instances to
                // the host locale. A changed IP was already reset above.
                warn!("GeoIP resolution failed for {}: {}", ip, error);
                Err(error)
            }
        }
    }

    pub(super) async fn clear_geo_profile(&self) {
        *self.geo_profile.write().await = GeoProfile::default();
    }

    pub(super) fn clear_geo_profile_sync(&self) {
        if let Ok(mut profile) = self.geo_profile.try_write() {
            *profile = GeoProfile::default();
        }
    }

    /// Health check loop
    pub async fn health_check_loop(&self) {
        let mut check_interval = interval(Duration::from_secs(self.config.health_check_interval));
        let mut consecutive_failures = 0;

        loop {
            check_interval.tick().await;

            // Check if process is still running
            if !self.is_running() {
                self.mark_unhealthy();
                self.clear_geo_profile().await;
                error!("sing-box process died, triggering fail-closed");
                continue;
            }

            // Check connectivity
            match self.check_exit_ip().await {
                Ok(ip) => {
                    self.mark_healthy();
                    info!("Health check passed, exit IP: {}", ip);
                    if let Err(error) = self.refresh_geo_profile_for_ip(&ip).await {
                        warn!("GeoIP resolution failed for {}: {}", ip, error);
                    }
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!("Health check failed ({}): {}", consecutive_failures, e);

                    if consecutive_failures >= HEALTH_FAILURE_THRESHOLD {
                        self.mark_unhealthy();
                        self.clear_geo_profile().await;
                        error!(
                            "Health check failed {} times, triggering fail-closed",
                            HEALTH_FAILURE_THRESHOLD
                        );
                        consecutive_failures = 0;
                    }
                }
            }
        }
    }
}
