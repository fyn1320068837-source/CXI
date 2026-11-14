// Instance Manager - manages lifecycle of all instances
use super::runtime::{InstanceRuntime, LaunchTransaction};
use super::{
    CodeAdapter, DesktopAdapter, InstanceAdapter, InstanceKind, RequestLogSender, ValidationStatus,
    VsCodeAdapter, WebAdapter,
};
use crate::geoip::{GeoProfile, SharedGeoProfile};
use crate::storage::Storage;
use crate::verifier::{NetworkVerifier, VerificationStatus};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub struct InstanceManager {
    runtime: InstanceRuntime,
    code: Arc<CodeAdapter>,
    desktop: Arc<DesktopAdapter>,
    vscode: Arc<VsCodeAdapter>,
    web: Arc<WebAdapter>,
    storage: Option<Arc<Storage>>,
    verifier: NetworkVerifier,
    geo_profile: SharedGeoProfile,
}

impl InstanceManager {
    pub fn with_geo_profile(geo_profile: SharedGeoProfile) -> Self {
        Self {
            runtime: InstanceRuntime::new(),
            code: Arc::new(CodeAdapter::new_with_geo_profile(
                format!("{}/.cim/instances/code", std::env::var("HOME").unwrap()),
                geo_profile.clone(),
            )),
            desktop: Arc::new(DesktopAdapter::new_with_geo_profile(
                format!("{}/.cim/instances/desktop", std::env::var("HOME").unwrap()),
                geo_profile.clone(),
            )),
            vscode: Arc::new(VsCodeAdapter::new_with_geo_profile(
                format!(
                    "{}/.cim/instances/vscode/data",
                    std::env::var("HOME").unwrap()
                ),
                format!(
                    "{}/.cim/instances/vscode/extensions",
                    std::env::var("HOME").unwrap()
                ),
                geo_profile.clone(),
            )),
            web: Arc::new(WebAdapter::new_with_geo_profile(
                format!("{}/.cim/instances/web", std::env::var("HOME").unwrap()),
                geo_profile.clone(),
            )),
            storage: None,
            verifier: NetworkVerifier::new(),
            geo_profile,
        }
    }

    pub fn with_allow_lan(mut self, allow_lan: bool) -> Self {
        self.verifier = NetworkVerifier::with_allow_lan(allow_lan);
        self
    }

    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn with_request_log_sender(mut self, sender: RequestLogSender) -> Self {
        if let Some(web) = Arc::get_mut(&mut self.web) {
            web.set_request_log_sender(sender);
        } else {
            tracing::warn!("Could not attach request log sink to WebAdapter");
        }
        self
    }

    pub async fn cleanup_orphaned_sessions(&self) -> Result<()> {
        self.web.cleanup_orphaned_sessions().await
    }

    fn adapter_for(&self, kind: InstanceKind) -> Arc<dyn InstanceAdapter> {
        match kind {
            InstanceKind::Code => self.code.clone(),
            InstanceKind::Desktop => self.desktop.clone(),
            InstanceKind::VsCode => self.vscode.clone(),
            InstanceKind::Web => self.web.clone(),
        }
    }

    pub async fn launch(&self, kind: InstanceKind) -> Result<()> {
        let adapter = self.adapter_for(kind);
        let reservation = self.runtime.reserve(kind).await?;

        let capability = adapter.detect().await;
        tracing::debug!(
            instance = ?kind,
            installed = capability.installed,
            version = ?capability.version,
            path = ?capability.path,
            "instance capability detected"
        );
        if !capability.installed {
            reservation.abort().await;
            anyhow::bail!("{} is not installed", kind.as_str());
        }

        // WebAdapter's fallback path opens Claude immediately when no GeoIP
        // profile is present. Block that path at the manager boundary so a
        // transient GeoIP outage can never expose a first navigation with the
        // host's locale/timezone. The proxy tunnel itself remains healthy and
        // can recover GeoIP independently.
        if kind == InstanceKind::Web {
            let profile = self.geo_profile.read().await;
            if !web_profile_ready(&profile) {
                drop(profile);
                reservation.abort().await;
                anyhow::bail!(
                    "GeoIP profile is not ready; Web launch is blocked until the exit location is resolved"
                );
            }
        }

        // Prepare
        if let Err(error) = adapter.prepare().await {
            reservation.abort().await;
            return Err(error.context(format!("failed to prepare {} instance", kind.as_str())));
        }

        // Launch
        let handle = match adapter.launch().await {
            Ok(handle) => handle,
            Err(error) => {
                reservation.abort().await;
                return Err(error.context(format!("failed to launch {} instance", kind.as_str())));
            }
        };
        let mut transaction = LaunchTransaction::new(adapter.clone(), handle);
        let root_pid = transaction
            .handle()
            .expect("launch transaction must own a process")
            .root_pid;

        // Save to storage
        if let Some(ref storage) = self.storage {
            let kind_str = kind.as_str();
            match storage
                .save_instance_state_async(kind_str, "running", Some(root_pid))
                .await
            {
                Ok(id) => tracing::info!("Saved instance state to storage: id={}", id),
                Err(error) => {
                    let rollback = transaction.rollback().await;
                    reservation.abort().await;
                    tracing::warn!(
                        instance = %kind.as_str(),
                        "Instance launch rolled back; proxy health state was left unchanged"
                    );
                    if let Err(cleanup_error) = rollback {
                        return Err(error.context(format!(
                            "failed to save instance state; launch rollback also failed: {}",
                            cleanup_error
                        )));
                    }
                    return Err(error.context("failed to save instance state; launch rolled back"));
                }
            }
        } else {
            tracing::warn!("No storage configured for InstanceManager");
        }

        // Publish the handle only after every fallible launch step succeeded.
        let handle = transaction.commit();
        self.runtime.commit(reservation, handle).await;

        Ok(())
    }

    pub async fn stop(&self, kind: InstanceKind) -> Result<()> {
        let adapter = self.adapter_for(kind);
        if self.runtime.stop(kind, &adapter).await? {
            if let Some(ref storage) = self.storage {
                match storage
                    .save_instance_state_async(kind.as_str(), "stopped", None)
                    .await
                {
                    Ok(id) => tracing::info!("Saved stopped state to storage: id={}", id),
                    Err(error) => tracing::error!("Failed to save stopped state: {}", error),
                }
            } else {
                tracing::warn!("No storage configured for stop");
            }
        }

        Ok(())
    }

    pub async fn is_running(&self, kind: InstanceKind) -> bool {
        self.runtime.is_running(kind).await
    }

    pub async fn status_all(&self) -> HashMap<InstanceKind, bool> {
        let mut status = HashMap::new();
        for kind in InstanceKind::ALL {
            status.insert(kind, self.is_running(kind).await);
        }
        status
    }

    pub async fn pid_all(&self) -> HashMap<InstanceKind, Option<u32>> {
        self.runtime.pid_all().await
    }

    pub async fn validation_all(&self) -> HashMap<InstanceKind, InstanceValidation> {
        let handles = self.runtime.handles().await;
        let mut result = HashMap::new();

        for kind in InstanceKind::ALL {
            let Some(handle) = handles.get(&kind) else {
                result.insert(kind, InstanceValidation::unknown("instance stopped"));
                continue;
            };

            let adapter = self.adapter_for(kind);
            let pids = adapter.process_tree(handle).await;
            match self.verifier.verify_process(&pids).await {
                Ok(check) => {
                    let snapshot = check.snapshot();
                    let validation_status = snapshot.status;
                    let status = match validation_status {
                        VerificationStatus::Pass => ValidationStatus::Pass,
                        VerificationStatus::Fail => ValidationStatus::Fail,
                        VerificationStatus::Unknown => ValidationStatus::Unknown,
                    };
                    let details = snapshot.details.clone();
                    let checked_at = snapshot.checked_at.clone();
                    let kind_str = kind.as_str();
                    let status_str = validation_status.as_str();
                    if snapshot.direct == 0 {
                        tracing::info!(
                            instance = %kind_str,
                            status = status_str,
                            checked_pids = snapshot.checked_pids,
                            proxied = snapshot.proxied,
                            local = snapshot.local,
                            direct = snapshot.direct,
                            "Network validation snapshot"
                        );
                    } else {
                        tracing::warn!(
                            instance = %kind_str,
                            status = status_str,
                            checked_pids = snapshot.checked_pids,
                            proxied = snapshot.proxied,
                            local = snapshot.local,
                            direct = snapshot.direct,
                            details = %details,
                            "Network validation detected direct connection"
                        );
                    }
                    if let Some(storage) = &self.storage {
                        if let Err(error) = storage
                            .save_validation_evidence_async(
                                kind_str,
                                status_str,
                                snapshot.proxied,
                                snapshot.local,
                                snapshot.direct,
                                &details,
                            )
                            .await
                        {
                            tracing::warn!(
                                instance = %kind_str,
                                error = %error,
                                "Failed to persist network validation evidence"
                            );
                        }
                    }
                    result.insert(
                        kind,
                        InstanceValidation {
                            status,
                            details,
                            checked_at: Some(checked_at),
                            reason: snapshot.reason,
                            direct_endpoints: snapshot.direct_endpoints,
                        },
                    );
                }
                Err(error) => {
                    let details = format!("lsof verification failed: {}", error);
                    let kind_str = kind.as_str();
                    if let Some(storage) = &self.storage {
                        if let Err(storage_error) = storage
                            .save_validation_evidence_async(kind_str, "fail", 0, 0, 0, &details)
                            .await
                        {
                            tracing::warn!(
                                instance = %kind_str,
                                error = %storage_error,
                                "Failed to persist network validation error"
                            );
                        }
                    }
                    result.insert(
                        kind,
                        InstanceValidation {
                            status: ValidationStatus::Fail,
                            details,
                            checked_at: Some(chrono::Utc::now().to_rfc3339()),
                            reason: Some("verifier_error".to_string()),
                            direct_endpoints: Vec::new(),
                        },
                    );
                }
            }
        }
        result
    }
}

fn web_profile_ready(profile: &GeoProfile) -> bool {
    profile.is_ready()
}

pub struct InstanceValidation {
    pub status: ValidationStatus,
    pub details: String,
    pub checked_at: Option<String>,
    pub reason: Option<String>,
    pub direct_endpoints: Vec<String>,
}

impl InstanceValidation {
    fn unknown(details: &str) -> Self {
        Self {
            status: ValidationStatus::Unknown,
            details: details.to_string(),
            checked_at: None,
            reason: Some("instance_stopped".to_string()),
            direct_endpoints: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::web_profile_ready;
    use crate::geoip::GeoProfile;

    #[test]
    fn web_launch_requires_runtime_geo_profile() {
        assert!(!web_profile_ready(&GeoProfile::default()));
        assert!(!web_profile_ready(&GeoProfile {
            source_ip: Some("203.0.113.5".to_string()),
            ..GeoProfile::default()
        }));
        assert!(web_profile_ready(&GeoProfile {
            source_ip: Some("203.0.113.5".to_string()),
            timezone: Some("America/New_York".to_string()),
            language: Some("en-US".to_string()),
            locale: Some("en_US.UTF-8".to_string()),
            ..GeoProfile::default()
        }));
    }
}
