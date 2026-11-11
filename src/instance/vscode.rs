// VS Code Claude extension adapter (Instance C)
use super::{InstanceAdapter, InstanceKind, ProcessHandle, CapabilityReport, ValidationReport, ValidationStatus};
use anyhow::{Result, Context};
use async_trait::async_trait;
use std::process::Command;

pub struct VsCodeAdapter {
    user_data_dir: String,
    extensions_dir: String,
}

impl VsCodeAdapter {
    pub fn new(user_data_dir: String, extensions_dir: String) -> Self {
        Self { user_data_dir, extensions_dir }
    }
}

#[async_trait]
impl InstanceAdapter for VsCodeAdapter {
    fn kind(&self) -> InstanceKind {
        InstanceKind::VsCode
    }

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
            }
        }
    }

    async fn prepare(&self) -> Result<()> {
        // Create directories
        std::fs::create_dir_all(&self.user_data_dir)?;
        std::fs::create_dir_all(&self.extensions_dir)?;

        // Install Claude extension
        Command::new("code")
            .args(&[
                "--install-extension",
                "anthropic.claude-code",
                "--extensions-dir",
                &self.extensions_dir,
            ])
            .output()?;

        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        let child = Command::new("code")
            .args(&[
                "--user-data-dir",
                &self.user_data_dir,
                "--extensions-dir",
                &self.extensions_dir,
                "--proxy-server=http://127.0.0.1:7890",
                "--new-window",
            ])
            .spawn()?;

        Ok(ProcessHandle {
            root_pid: child.id(),
            kind: InstanceKind::VsCode,
        })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let pid = Pid::from_raw(handle.root_pid as i32);
        let _ = kill(pid, Signal::SIGTERM);

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        let _ = kill(pid, Signal::SIGKILL);
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        vec![handle.root_pid]
    }

    async fn verify_network(&self, _handle: &ProcessHandle) -> ValidationReport {
        ValidationReport {
            status: ValidationStatus::Unknown,
            details: "Network verification not yet implemented".to_string(),
        }
    }
}
