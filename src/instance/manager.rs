// Instance Manager - manages lifecycle of all instances
use super::runtime::{InstanceRuntime, LaunchTransaction};
use super::{InstanceAdapter, InstanceKind, RequestLogSender, ValidationStatus, APPLICATIONS};
use crate::geoip::{GeoProfile, SharedGeoProfile};
use crate::storage::Storage;
use crate::verifier::{NetworkVerifier, VerificationStatus};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

pub struct InstanceManager {
    runtime: InstanceRuntime,
    configs: crate::config::InstanceConfigs,
    adapters: HashMap<InstanceKind, Arc<dyn InstanceAdapter>>,
    storage: Option<Arc<Storage>>,
    verifier: NetworkVerifier,
    geo_profile: SharedGeoProfile,
}

/// One consistent view of instance state for the API and security reports.
pub struct InstanceSnapshot {
    pub status: HashMap<InstanceKind, bool>,
    pub pids: HashMap<InstanceKind, Option<u32>>,
    pub validation: HashMap<InstanceKind, InstanceValidation>,
}

impl InstanceManager {
    pub fn with_geo_profile(geo_profile: SharedGeoProfile) -> Self {
        Self {
            runtime: InstanceRuntime::new(),
            configs: crate::config::InstanceConfigs::default(),
            adapters: APPLICATIONS
                .iter()
                .map(|app| {
                    (
                        app.kind,
                        app.create_adapter(&std::env::var("HOME").unwrap(), geo_profile.clone()),
                    )
                })
                .collect(),
            storage: None,
            verifier: NetworkVerifier::new(),
            geo_profile,
        }
    }

    pub fn with_instance_configs(
        mut self,
        configs: crate::config::InstanceConfigs,
    ) -> Result<Self> {
        configs.validate()?;
        self.configs = configs;
        Ok(self)
    }

    pub fn instance_configs(&self) -> &crate::config::InstanceConfigs {
        &self.configs
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
        for adapter in self.adapters.values_mut() {
            if let Some(adapter) = Arc::get_mut(adapter) {
                adapter.set_request_log_sender(sender.clone());
            } else {
                tracing::warn!("Could not attach request log sink to shared adapter");
            }
        }
        self
    }

    pub async fn cleanup_orphaned_sessions(&self) -> Result<()> {
        // Preserve Web-before-Desktop cleanup; all hooks run even if disabled.
        let mut apps = APPLICATIONS.iter().collect::<Vec<_>>();
        apps.sort_by_key(|app| app.cleanup_priority);
        for app in apps {
            self.adapter_for(app.kind)?
                .cleanup_orphaned_sessions()
                .await?;
        }
        Ok(())
    }

    fn adapter_for(&self, kind: InstanceKind) -> Result<Arc<dyn InstanceAdapter>> {
        self.adapters
            .get(&kind)
            .cloned()
            .with_context(|| format!("no registered adapter for {}", kind.definition().id))
    }

    pub async fn launch(&self, kind: InstanceKind) -> Result<()> {
        self.configs.require_enabled(kind)?;
        let adapter = self.adapter_for(kind)?;
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
        if kind.definition().requires_geo_profile {
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
        let adapter = self.adapter_for(kind)?;
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

    pub async fn stop_all(&self) -> Result<()> {
        let mut failures = Vec::new();
        for kind in APPLICATIONS.iter().map(|app| app.kind) {
            if let Err(error) = self.stop(kind).await {
                failures.push(format!("{}: {error}", kind.as_str()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "failed to stop instances: {}",
                failures.join("; ")
            ))
        }
    }

    /// Route a Desktop authentication URL through the persistent CIM Web
    /// profile. The Web adapter validates the destination and performs the
    /// navigation through its loopback DevTools session.
    pub async fn open_web_auth_url(&self, url: &str) -> Result<()> {
        self.configs.require_enabled(InstanceKind::Desktop)?;
        let browser_id = self
            .configs
            .get(InstanceKind::Desktop)
            .auth_browser
            .as_deref()
            .context("Desktop has no authentication browser configured")?;
        let browser = InstanceKind::parse(browser_id).context("unknown authentication browser")?;
        self.configs.require_enabled(browser)?;
        let adapter = self.adapter_for(browser)?;
        if !self.is_running(browser).await {
            self.launch(browser)
                .await
                .context("failed to start CIM Web for Desktop authentication")?;
        }
        adapter.open_auth_url(url).await
    }

    pub async fn is_running(&self, kind: InstanceKind) -> bool {
        self.runtime.is_running(kind).await
    }

    pub async fn any_running(&self) -> bool {
        !self.runtime.handles().await.is_empty()
    }

    pub async fn snapshot_all(&self) -> InstanceSnapshot {
        let handles = self.runtime.handles().await;
        let status = APPLICATIONS
            .iter()
            .map(|app| app.kind)
            .map(|kind| (kind, handles.contains_key(&kind)))
            .collect();
        let pids = APPLICATIONS
            .iter()
            .map(|app| app.kind)
            .map(|kind| (kind, handles.get(&kind).map(|handle| handle.root_pid)))
            .collect();
        let validation = self.validation_for_handles(&handles).await;
        InstanceSnapshot {
            status,
            pids,
            validation,
        }
    }

    async fn validation_for_handles(
        &self,
        handles: &HashMap<InstanceKind, super::ProcessHandle>,
    ) -> HashMap<InstanceKind, InstanceValidation> {
        let mut result = HashMap::new();

        for kind in APPLICATIONS.iter().map(|app| app.kind) {
            let Some(handle) = handles.get(&kind) else {
                result.insert(kind, InstanceValidation::unknown("instance stopped"));
                continue;
            };

            let scan = async { self.adapter_for(kind)?.process_tree_checked(handle).await }.await;
            let pids = match scan {
                Ok(pids) => pids,
                Err(error) => {
                    let details = format!("process scan failed: {error}");
                    let kind_str = kind.as_str();
                    tracing::error!(instance = %kind_str, error = %error, "Instance process scan failed closed");
                    if let Some(storage) = &self.storage {
                        if let Err(storage_error) = storage
                            .save_validation_evidence_async(kind_str, "fail", 0, 0, 0, &details)
                            .await
                        {
                            tracing::warn!(
                                instance = %kind_str,
                                error = %storage_error,
                                "Failed to persist process scan failure"
                            );
                        }
                    }
                    result.insert(
                        kind,
                        InstanceValidation {
                            status: ValidationStatus::Fail,
                            details,
                            checked_at: Some(chrono::Utc::now().to_rfc3339()),
                            reason: Some("process_scan_failed".to_string()),
                            direct_endpoints: Vec::new(),
                        },
                    );
                    continue;
                }
            };
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

    struct RecordingAdapter {
        id: &'static str,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingAdapter {
        fn record(&self, action: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("{}:{action}", self.id));
        }
    }

    #[async_trait::async_trait]
    impl crate::instance::InstanceAdapter for RecordingAdapter {
        fn set_request_log_sender(&mut self, _: crate::instance::RequestLogSender) {
            self.record("logs");
        }
        async fn cleanup_orphaned_sessions(&self) -> anyhow::Result<()> {
            self.record("cleanup");
            Ok(())
        }
        async fn open_auth_url(&self, _: &str) -> anyhow::Result<()> {
            self.record("auth");
            Ok(())
        }
        async fn detect(&self) -> crate::instance::CapabilityReport {
            self.record("detect");
            crate::instance::CapabilityReport {
                installed: true,
                version: None,
                path: None,
            }
        }
        async fn prepare(&self) -> anyhow::Result<()> {
            self.record("prepare");
            Ok(())
        }
        async fn launch(&self) -> anyhow::Result<crate::instance::ProcessHandle> {
            self.record("launch");
            Ok(crate::instance::ProcessHandle { root_pid: 999_999 })
        }
        async fn stop(&self, _: &crate::instance::ProcessHandle) -> anyhow::Result<()> {
            self.record("stop");
            Ok(())
        }
        async fn process_tree(&self, _: &crate::instance::ProcessHandle) -> Vec<u32> {
            vec![]
        }
        async fn process_tree_checked(
            &self,
            _: &crate::instance::ProcessHandle,
        ) -> anyhow::Result<Vec<u32>> {
            self.record("scan");
            anyhow::bail!("fixture scan error")
        }
    }

    fn recording_manager() -> (
        super::InstanceManager,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let geo = std::sync::Arc::new(tokio::sync::RwLock::new(GeoProfile {
            source_ip: Some("203.0.113.5".into()),
            timezone: Some("America/New_York".into()),
            language: Some("en-US".into()),
            locale: Some("en_US.UTF-8".into()),
            ..GeoProfile::default()
        }));
        let mut manager = super::InstanceManager::with_geo_profile(geo);
        for app in &crate::instance::APPLICATIONS {
            manager.adapters.insert(
                app.kind,
                std::sync::Arc::new(RecordingAdapter {
                    id: app.id,
                    events: events.clone(),
                }),
            );
        }
        (manager, events)
    }

    #[tokio::test]
    async fn registered_dispatch_preserves_single_instance_lifecycle_and_scan_failure() {
        let (manager, events) = recording_manager();
        for app in &crate::instance::APPLICATIONS {
            let alias = crate::instance::InstanceKind::parse(app.legacy_alias).unwrap();
            assert!(std::sync::Arc::ptr_eq(
                &manager.adapter_for(alias).unwrap(),
                &manager.adapter_for(app.kind).unwrap()
            ));
            manager.launch(app.kind).await.unwrap();
            assert!(manager.launch(alias).await.is_err());
        }
        let snapshot = manager.snapshot_all().await;
        assert_eq!(snapshot.status.len(), 4);
        for value in snapshot.validation.values() {
            assert!(matches!(
                value.status,
                crate::instance::ValidationStatus::Fail
            ));
            assert_eq!(value.reason.as_deref(), Some("process_scan_failed"));
        }
        manager.stop_all().await.unwrap();
        assert!(!manager.any_running().await);
        let events = events.lock().unwrap();
        for app in &crate::instance::APPLICATIONS {
            for action in ["prepare", "launch", "scan", "stop"] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| **event == format!("{}:{action}", app.id))
                        .count(),
                    1
                );
            }
        }
    }

    #[tokio::test]
    async fn registry_hooks_preserve_cleanup_order_logs_and_auth_target() {
        let (manager, events) = recording_manager();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let manager = manager.with_request_log_sender(tx);
        manager.cleanup_orphaned_sessions().await.unwrap();
        let cleanups = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.ends_with(":cleanup"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            &cleanups[..2],
            ["claude-web:cleanup", "claude-desktop:cleanup"]
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.ends_with(":logs"))
                .count(),
            4
        );
        manager
            .open_web_auth_url("https://claude.ai/login")
            .await
            .unwrap();
        assert!(events.lock().unwrap().contains(&"claude-web:auth".into()));
        assert!(!events
            .lock()
            .unwrap()
            .contains(&"claude-desktop:launch".into()));
        manager.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn registered_web_still_requires_geo_and_missing_adapter_fails_closed() {
        let (mut manager, events) = recording_manager();
        *manager.geo_profile.write().await = GeoProfile::default();
        let error = manager
            .launch(crate::instance::InstanceKind::Web)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("GeoIP"));
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.ends_with(":prepare")));
        manager
            .adapters
            .remove(&crate::instance::InstanceKind::Code);
        assert!(manager
            .launch(crate::instance::InstanceKind::Code)
            .await
            .unwrap_err()
            .to_string()
            .contains("no registered adapter"));
        assert!(!manager.any_running().await);
    }

    #[tokio::test]
    async fn disabled_instances_are_rejected_before_detection_or_preparation() {
        let configs: crate::config::InstanceConfigs =
            toml::from_str("[claude-web]\nenabled = false\n[claude-desktop]\nenabled = false")
                .unwrap();
        let manager = super::InstanceManager::with_geo_profile(std::sync::Arc::new(
            tokio::sync::RwLock::new(GeoProfile::default()),
        ))
        .with_instance_configs(configs)
        .unwrap();
        for id in ["web", "claude-web", "desktop", "claude-desktop"] {
            let error = manager
                .launch(crate::instance::InstanceKind::parse(id).unwrap())
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("disabled by instance configuration"));
        }
        assert!(manager
            .open_web_auth_url("https://claude.ai/login")
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled"));
        assert!(!manager.any_running().await);
    }

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
