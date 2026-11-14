// Claude Desktop adapter (Instance B) - Soft isolation mode
use super::{CapabilityReport, InstanceAdapter, ProcessHandle};
use anyhow::{Context, Result};
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
            if let Ok(output) = Command::new("pgrep").arg("-f").arg(pattern).output() {
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
            path: if installed {
                Some(self.app_path.clone())
            } else {
                None
            },
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
        let Some(pid) = self.get_claude_pid().await else {
            // `open` can succeed while the app process is still unavailable.
            // Do not leave a newly started Desktop instance behind when the
            // adapter cannot return a handle to the manager.
            let _ = Command::new("osascript")
                .arg("-e")
                .arg("quit app \"Claude\"")
                .status();
            anyhow::bail!("Failed to find Claude Desktop process; launch rolled back");
        };

        Ok(ProcessHandle { root_pid: pid })
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
        if let Ok(output) = Command::new("pgrep").arg("-f").arg("Claude.app").output() {
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
}
