// Claude Desktop adapter (Instance B) - Soft isolation mode
use super::{InstanceAdapter, InstanceKind, ProcessHandle, CapabilityReport, ValidationReport, ValidationStatus};
use anyhow::{Result, Context};
use async_trait::async_trait;
use std::process::Command;

pub struct DesktopAdapter {
    use_vm: bool,
    app_path: String,
}

impl DesktopAdapter {
    pub fn new(use_vm: bool) -> Self {
        Self {
            use_vm,
            app_path: "/Applications/Claude.app".to_string(),
        }
    }

    fn check_vm_support(&self) -> bool {
        Command::new("which")
            .arg("tart")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    async fn get_claude_pid(&self) -> Option<u32> {
        // Wait a bit more for the app to fully launch
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Try multiple patterns to find Claude Desktop
        let patterns = vec![
            "Claude.app/Contents/MacOS/Claude",
            "Claude.app",
            "/Applications/Claude.app",
        ];

        for pattern in patterns {
            if let Ok(output) = Command::new("pgrep")
                .arg("-f")
                .arg(pattern)
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(pid_str) = stdout.lines().next() {
                    if let Ok(pid) = pid_str.trim().parse() {
                        return Some(pid);
                    }
                }
            }
        }

        None
    }
}

#[async_trait]
impl InstanceAdapter for DesktopAdapter {
    fn kind(&self) -> InstanceKind {
        InstanceKind::Desktop
    }

    async fn detect(&self) -> CapabilityReport {
        let installed = std::path::Path::new(&self.app_path).exists();
        let version = if installed {
            Some("Unknown".to_string())
        } else {
            None
        };

        CapabilityReport {
            installed,
            version,
            path: if installed { Some(self.app_path.clone()) } else { None },
        }
    }

    async fn prepare(&self) -> Result<()> {
        if self.use_vm {
            // VM mode preparation would go here
            // For now, soft isolation requires no preparation
        }
        Ok(())
    }

    async fn launch(&self) -> Result<ProcessHandle> {
        if self.use_vm {
            anyhow::bail!("VM mode not yet implemented");
        }

        // Soft isolation mode: launch directly with open
        Command::new("open")
            .arg("-a")
            .arg("Claude")
            .spawn()
            .context("Failed to launch Claude Desktop")?;

        // Wait for app to launch
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Get actual Claude process PID
        let pid = self.get_claude_pid().await
            .ok_or_else(|| anyhow::anyhow!("Failed to find Claude Desktop process"))?;

        Ok(ProcessHandle {
            root_pid: pid,
            kind: InstanceKind::Desktop,
        })
    }

    async fn stop(&self, _handle: &ProcessHandle) -> Result<()> {
        if self.use_vm {
            anyhow::bail!("VM mode not yet implemented");
        }

        // Soft isolation: use osascript to quit gracefully
        Command::new("osascript")
            .arg("-e")
            .arg("quit app \"Claude\"")
            .status()
            .context("Failed to quit Claude Desktop")?;

        Ok(())
    }

    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        let mut pids = vec![handle.root_pid];

        // Use pgrep to find all Claude.app processes
        if let Ok(output) = Command::new("pgrep")
            .arg("-f")
            .arg("Claude.app")
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }

        pids
    }

    async fn verify_network(&self, _handle: &ProcessHandle) -> ValidationReport {
        if self.use_vm {
            ValidationReport {
                status: ValidationStatus::Unknown,
                details: "VM network verification not yet implemented".to_string(),
            }
        } else {
            ValidationReport {
                status: ValidationStatus::Unsupported,
                details: "Soft isolation mode - network verification not supported".to_string(),
            }
        }
    }
}
