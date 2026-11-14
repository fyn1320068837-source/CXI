// VS Code Claude extension adapter (Instance C)
use super::{
    apply_locale_env, descendant_pids, isolated_command, terminate_process_tree, CapabilityReport,
    InstanceAdapter, ProcessHandle,
};
use crate::geoip::SharedGeoProfile;
use anyhow::Result;
use async_trait::async_trait;
use std::process::Command;

pub struct VsCodeAdapter {
    user_data_dir: String,
    extensions_dir: String,
    geo_profile: SharedGeoProfile,
}

impl VsCodeAdapter {
    pub fn new_with_geo_profile(
        user_data_dir: String,
        extensions_dir: String,
        geo_profile: SharedGeoProfile,
    ) -> Self {
        Self {
            user_data_dir,
            extensions_dir,
            geo_profile,
        }
    }
}

#[async_trait]
impl InstanceAdapter for VsCodeAdapter {
    async fn detect(&self) -> CapabilityReport {
        // Check if `code` CLI is available
        let output = Command::new("which").arg("code").output();

        match output {
            Ok(o) if o.status.success() => {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                CapabilityReport {
                    installed: true,
                    version: None,
                    path: Some(path),
                }
            }
            _ => CapabilityReport {
                installed: false,
                version: None,
                path: None,
            },
        }
    }

    async fn prepare(&self) -> Result<()> {
        // Create directories
        std::fs::create_dir_all(&self.user_data_dir)?;
        std::fs::create_dir_all(&self.extensions_dir)?;

        // Install Claude extension
        let executable = self
            .detect()
            .await
            .path
            .ok_or_else(|| anyhow::anyhow!("code is not installed"))?;
        Command::new(executable)
            .args([
                "--install-extension",
                "anthropic.claude-code",
                "--extensions-dir",
                &self.extensions_dir,
            ])
            .output()?;

        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        let profile = self.geo_profile.read().await.clone();
        let executable = self
            .detect()
            .await
            .path
            .ok_or_else(|| anyhow::anyhow!("code is not installed"))?;
        let mut command = isolated_command(executable);
        command.args([
            "--user-data-dir",
            &self.user_data_dir,
            "--extensions-dir",
            &self.extensions_dir,
            "--proxy-server=http://127.0.0.1:7890",
            "--new-window",
        ]);
        if let Some(language) = profile.language.as_deref() {
            command.arg("--locale").arg(language);
        }
        apply_locale_env(&mut command, &profile);
        let child = command.spawn()?;

        Ok(ProcessHandle {
            root_pid: child.id(),
        })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        terminate_process_tree(handle.root_pid).await;
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        descendant_pids(handle.root_pid)
    }
}
