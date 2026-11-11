// Claude Web in Chromium adapter (Instance D)
use super::{InstanceAdapter, InstanceKind, ProcessHandle, CapabilityReport, ValidationReport, ValidationStatus};
use anyhow::{Result, Context};
use async_trait::async_trait;
use std::process::Command;

pub struct WebAdapter {
    user_data_dir: String,
}

impl WebAdapter {
    pub fn new(user_data_dir: String) -> Self {
        Self { user_data_dir }
    }
}

#[async_trait]
impl InstanceAdapter for WebAdapter {
    fn kind(&self) -> InstanceKind {
        InstanceKind::Web
    }

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
        let browser_path = report.path.ok_or_else(|| anyhow::anyhow!("No browser found"))?;

        let child = Command::new(&browser_path)
            .args(&[
                &format!("--user-data-dir={}", self.user_data_dir),
                "--proxy-server=socks5://127.0.0.1:7891",
                "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1",
                "--force-webrtc-ip-handling-policy=default_public_interface_only",
                "--disable-background-networking",
                "--no-first-run",
                "--no-default-browser-check",
                "https://claude.ai",
            ])
            .spawn()?;

        Ok(ProcessHandle {
            root_pid: child.id(),
            kind: InstanceKind::Web,
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
