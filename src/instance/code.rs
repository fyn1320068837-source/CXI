// Claude Code CLI adapter (Instance A)
use super::{InstanceAdapter, InstanceKind, ProcessHandle, CapabilityReport, ValidationReport, ValidationStatus};
use anyhow::{Result, Context};
use async_trait::async_trait;
use std::process::Command;

pub struct CodeAdapter {
    profile_dir: String,
}

impl CodeAdapter {
    pub fn new(profile_dir: String) -> Self {
        Self { profile_dir }
    }
}

#[async_trait]
impl InstanceAdapter for CodeAdapter {
    fn kind(&self) -> InstanceKind {
        InstanceKind::Code
    }

    async fn detect(&self) -> CapabilityReport {
        // Check if `claude` CLI is available
        let output = Command::new("which").arg("claude").output();

        match output {
            Ok(o) if o.status.success() => {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                // Try to get version
                let version = Command::new("claude")
                    .arg("--version")
                    .output()
                    .ok()
                    .and_then(|v| String::from_utf8(v.stdout).ok())
                    .map(|v| v.trim().to_string());

                CapabilityReport {
                    installed: true,
                    version,
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
        // Create isolated HOME directory for Claude Code
        std::fs::create_dir_all(&self.profile_dir)?;
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        // Launch claude with custom HOME and proxy env vars
        let child = Command::new("claude")
            .env("HOME", &self.profile_dir)
            .env("HTTPS_PROXY", "http://127.0.0.1:7890")
            .env("HTTP_PROXY", "http://127.0.0.1:7890")
            .env("ALL_PROXY", "socks5://127.0.0.1:7891")
            .env("NO_PROXY", "localhost,127.0.0.1")
            .spawn()?;

        Ok(ProcessHandle {
            root_pid: child.id(),
            kind: InstanceKind::Code,
        })
    }

    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        // Send SIGTERM, wait 5s, then SIGKILL
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let pid = Pid::from_raw(handle.root_pid as i32);
        let _ = kill(pid, Signal::SIGTERM);

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // Force kill
        let _ = kill(pid, Signal::SIGKILL);
        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        // Use sysinfo to recursively enumerate child processes
        // TODO: Implement full process tree enumeration
        vec![handle.root_pid]
    }

    async fn verify_network(&self, _handle: &ProcessHandle) -> ValidationReport {
        // TODO: Use lsof to verify all connections go through proxy
        ValidationReport {
            status: ValidationStatus::Unknown,
            details: "Network verification not yet implemented".to_string(),
        }
    }
}
