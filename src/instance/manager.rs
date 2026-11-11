// Instance Manager - manages lifecycle of all instances
use super::{
    CodeAdapter, DesktopAdapter, InstanceAdapter, InstanceKind, ProcessHandle, ValidationStatus,
    VsCodeAdapter, WebAdapter,
};
use crate::geoip::SharedGeoProfile;
use crate::storage::Storage;
use crate::verifier::{NetworkVerifier, VerificationStatus};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct InstanceManager {
    instances: Arc<RwLock<HashMap<InstanceKind, Option<ProcessHandle>>>>,
    code: Arc<CodeAdapter>,
    desktop: Arc<DesktopAdapter>,
    vscode: Arc<VsCodeAdapter>,
    web: Arc<WebAdapter>,
    storage: Option<Arc<Storage>>,
    verifier: NetworkVerifier,
}

impl InstanceManager {
    pub fn with_geo_profile(geo_profile: SharedGeoProfile) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            code: Arc::new(CodeAdapter::new_with_geo_profile(
                format!("{}/.cim/instances/code", std::env::var("HOME").unwrap()),
                geo_profile.clone(),
            )),
            desktop: Arc::new(DesktopAdapter::new(false)),
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

    pub async fn launch(&self, kind: InstanceKind) -> Result<()> {
        let adapter: Arc<dyn InstanceAdapter> = match kind {
            InstanceKind::Code => self.code.clone(),
            InstanceKind::Desktop => self.desktop.clone(),
            InstanceKind::VsCode => self.vscode.clone(),
            InstanceKind::Web => self.web.clone(),
        };

        let capability = adapter.detect().await;
        tracing::debug!(
            instance = ?kind,
            installed = capability.installed,
            version = ?capability.version,
            path = ?capability.path,
            "instance capability detected"
        );
        if !capability.installed {
            anyhow::bail!("{} is not installed", format!("{:?}", kind).to_lowercase());
        }

        // Prepare
        adapter.prepare().await?;

        // Launch
        let handle = adapter.launch().await?;

        // Save to storage
        if let Some(ref storage) = self.storage {
            let kind_str = format!("{:?}", kind).to_lowercase();
            match storage.save_instance_state(&kind_str, "running", Some(handle.root_pid)) {
                Ok(id) => tracing::info!("Saved instance state to storage: id={}", id),
                Err(e) => tracing::error!("Failed to save instance state: {}", e),
            }
        } else {
            tracing::warn!("No storage configured for InstanceManager");
        }

        // Store handle
        let mut instances = self.instances.write().await;
        instances.insert(kind, Some(handle));

        Ok(())
    }

    pub async fn stop(&self, kind: InstanceKind) -> Result<()> {
        let mut instances = self.instances.write().await;

        if let Some(Some(handle)) = instances.get(&kind) {
            let adapter: Arc<dyn InstanceAdapter> = match kind {
                InstanceKind::Code => self.code.clone(),
                InstanceKind::Desktop => self.desktop.clone(),
                InstanceKind::VsCode => self.vscode.clone(),
                InstanceKind::Web => self.web.clone(),
            };

            adapter.stop(handle).await?;

            // Save to storage
            if let Some(ref storage) = self.storage {
                let kind_str = format!("{:?}", kind).to_lowercase();
                match storage.save_instance_state(&kind_str, "stopped", None) {
                    Ok(id) => tracing::info!("Saved stopped state to storage: id={}", id),
                    Err(e) => tracing::error!("Failed to save stopped state: {}", e),
                }
            } else {
                tracing::warn!("No storage configured for stop");
            }

            instances.insert(kind, None);
        }

        Ok(())
    }

    pub async fn is_running(&self, kind: InstanceKind) -> bool {
        let instances = self.instances.read().await;
        instances.get(&kind).and_then(|h| h.as_ref()).is_some()
    }

    pub async fn status_all(&self) -> HashMap<InstanceKind, bool> {
        let mut status = HashMap::new();
        status.insert(
            InstanceKind::Code,
            self.is_running(InstanceKind::Code).await,
        );
        status.insert(
            InstanceKind::Desktop,
            self.is_running(InstanceKind::Desktop).await,
        );
        status.insert(
            InstanceKind::VsCode,
            self.is_running(InstanceKind::VsCode).await,
        );
        status.insert(InstanceKind::Web, self.is_running(InstanceKind::Web).await);
        status
    }

    pub async fn pid_all(&self) -> HashMap<InstanceKind, Option<u32>> {
        let instances = self.instances.read().await;
        instances
            .iter()
            .map(|(kind, handle)| (*kind, handle.as_ref().map(|value| value.root_pid)))
            .collect()
    }

    pub async fn validation_all(&self) -> HashMap<InstanceKind, InstanceValidation> {
        let handles = self.instances.read().await.clone();
        let mut result = HashMap::new();

        for kind in [
            InstanceKind::Code,
            InstanceKind::Desktop,
            InstanceKind::VsCode,
            InstanceKind::Web,
        ] {
            if kind == InstanceKind::Desktop {
                result.insert(
                    kind,
                    InstanceValidation {
                        status: ValidationStatus::Unsupported,
                        details: "Desktop 自动覆盖未支持，当前为软隔离模式".to_string(),
                    },
                );
                continue;
            }

            let Some(Some(handle)) = handles.get(&kind) else {
                result.insert(kind, InstanceValidation::unknown("instance stopped"));
                continue;
            };

            let adapter: Arc<dyn InstanceAdapter> = match kind {
                InstanceKind::Code => self.code.clone(),
                InstanceKind::Desktop => self.desktop.clone(),
                InstanceKind::VsCode => self.vscode.clone(),
                InstanceKind::Web => self.web.clone(),
            };
            let pids = adapter.process_tree(handle).await;
            match self.verifier.verify_process(&pids).await {
                Ok(check) => {
                    let status = match check.status() {
                        VerificationStatus::Pass => ValidationStatus::Pass,
                        VerificationStatus::Fail => ValidationStatus::Fail,
                        VerificationStatus::Unknown => ValidationStatus::Unknown,
                    };
                    result.insert(
                        kind,
                        InstanceValidation {
                            status,
                            details: check.details(),
                        },
                    );
                }
                Err(error) => {
                    result.insert(
                        kind,
                        InstanceValidation {
                            status: ValidationStatus::Fail,
                            details: format!("lsof verification failed: {}", error),
                        },
                    );
                }
            }
        }
        result
    }
}

pub struct InstanceValidation {
    pub status: ValidationStatus,
    pub details: String,
}

impl InstanceValidation {
    fn unknown(details: &str) -> Self {
        Self {
            status: ValidationStatus::Unknown,
            details: details.to_string(),
        }
    }
}
