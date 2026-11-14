// VS Code Claude extension adapter (Instance C)
use super::{
    apply_locale_env, descendant_pids, isolated_command, terminate_process_tree, CapabilityReport,
    InstanceAdapter, ProcessHandle, HTTP_PROXY_ENDPOINT,
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

fn configure_vscode_command(
    command: &mut Command,
    user_data_dir: &str,
    extensions_dir: &str,
    profile: &crate::geoip::GeoProfile,
) {
    let proxy_arg = format!("--proxy-server={HTTP_PROXY_ENDPOINT}");
    command.args([
        "--user-data-dir",
        user_data_dir,
        "--extensions-dir",
        extensions_dir,
        &proxy_arg,
        "--new-window",
    ]);
    if let Some(language) = profile.language.as_deref() {
        command.arg("--locale").arg(language);
    }
    apply_locale_env(command, profile);
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
        configure_vscode_command(
            &mut command,
            &self.user_data_dir,
            &self.extensions_dir,
            &profile,
        );
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

#[cfg(test)]
mod tests {
    use super::configure_vscode_command;
    use crate::geoip::GeoProfile;
    use std::process::Command;

    fn env_value(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == key)
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    #[test]
    fn vscode_command_keeps_data_dirs_and_proxy_separate() {
        let profile = GeoProfile {
            language: Some("zh-CN".to_string()),
            timezone: Some("Asia/Shanghai".to_string()),
            locale: Some("zh_CN.UTF-8".to_string()),
            ..GeoProfile::default()
        };
        let mut command = Command::new("code");
        configure_vscode_command(
            &mut command,
            "/tmp/cim-vscode-data",
            "/tmp/cim-vscode-extensions",
            &profile,
        );
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--user-data-dir", "/tmp/cim-vscode-data"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--extensions-dir", "/tmp/cim-vscode-extensions"]));
        assert!(args
            .iter()
            .any(|arg| arg == "--proxy-server=http://127.0.0.1:7890"));
        assert!(args.windows(2).any(|pair| pair == ["--locale", "zh-CN"]));
        assert_eq!(env_value(&command, "TZ").as_deref(), Some("Asia/Shanghai"));
        assert_eq!(
            env_value(&command, "LC_ALL").as_deref(),
            Some("zh_CN.UTF-8")
        );
    }
}
