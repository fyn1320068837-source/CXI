// Instance Manager - manages lifecycle of all instances
use super::{InstanceAdapter, InstanceKind, ProcessHandle, CodeAdapter, DesktopAdapter, VsCodeAdapter, WebAdapter};
use crate::storage::Storage;
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
}

impl InstanceManager {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            code: Arc::new(CodeAdapter::new(
                format!("{}/.cim/instances/code", std::env::var("HOME").unwrap())
            )),
            desktop: Arc::new(DesktopAdapter::new(false)),
            vscode: Arc::new(VsCodeAdapter::new(
                format!("{}/.cim/instances/vscode/data", std::env::var("HOME").unwrap()),
                format!("{}/.cim/instances/vscode/extensions", std::env::var("HOME").unwrap()),
            )),
            web: Arc::new(WebAdapter::new(
                format!("{}/.cim/instances/web", std::env::var("HOME").unwrap())
            )),
            storage: None,
        }
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
        status.insert(InstanceKind::Code, self.is_running(InstanceKind::Code).await);
        status.insert(InstanceKind::Desktop, self.is_running(InstanceKind::Desktop).await);
        status.insert(InstanceKind::VsCode, self.is_running(InstanceKind::VsCode).await);
        status.insert(InstanceKind::Web, self.is_running(InstanceKind::Web).await);
        status
    }
}
