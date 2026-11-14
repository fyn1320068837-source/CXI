use super::*;

impl ProxyManager {
    /// Generate sing-box config with real upstream proxy
    pub(super) async fn generate_config(&self) -> Result<()> {
        use std::fs;

        // 读取 config.toml 中的代理设置
        let home = std::env::var("HOME").unwrap_or_default();
        let toml_path = format!("{}/.cim/config.toml", home);
        let toml_content = fs::read_to_string(&toml_path).unwrap_or_default();

        let upstream_host = extract_toml_val(&toml_content, "host").unwrap_or_default();
        let upstream_port: u16 = extract_toml_val(&toml_content, "port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let upstream_user = extract_toml_val(&toml_content, "username").unwrap_or_default();

        // Prefer the already generated 0600 sing-box config. Reading Keychain
        // through `/usr/bin/security` can trigger a macOS authorization prompt
        // on every launchd restart when the binary is rebuilt or unsigned. The
        // config is written with 0600 permissions below, so Keychain is only
        // needed for the first migration or when the config has no password.
        let upstream_pass =
            get_existing_upstream_password(&self.config.config_path, &upstream_host)
                .or_else(|| get_keychain_password("com.cim.proxy", &upstream_host))
                .unwrap_or_default();

        // 决定出站类型
        let outbound = if !upstream_host.is_empty() && upstream_port > 0 {
            info!(
                "Configuring sing-box upstream: socks5://{}:{}",
                upstream_host, upstream_port
            );
            if !upstream_user.is_empty() {
                serde_json::json!({
                    "type": "socks",
                    "tag": "upstream",
                    "server": upstream_host,
                    "server_port": upstream_port,
                    "username": upstream_user,
                    "password": upstream_pass,
                    "version": "5"
                })
            } else {
                serde_json::json!({
                    "type": "socks",
                    "tag": "upstream",
                    "server": upstream_host,
                    "server_port": upstream_port,
                    "version": "5"
                })
            }
        } else {
            anyhow::bail!(
                "No upstream SOCKS5 proxy configured; refusing to start a direct outbound"
            );
        };

        let config = serde_json::json!({
            "log": { "level": "warn" },
            // Resolve sing-box and health-check domains through DoH over the
            // upstream SOCKS tunnel. Browser DNS is separately constrained by
            // Chrome's host-resolver rules and SOCKS5 remote resolution.
            "dns": {
                "servers": [{
                    "tag": "remote",
                    "address": "https://1.1.1.1/dns-query",
                    "detour": "upstream"
                }],
                "strategy": "ipv4_only",
                "final": "remote"
            },
            "inbounds": [
                { "type": "http",  "tag": "http-in",  "listen": "127.0.0.1", "listen_port": 7890 },
                { "type": "socks", "tag": "socks-in", "listen": "127.0.0.1", "listen_port": 7891 }
            ],
            "outbounds": [outbound],
            "route": { "final": "upstream" }
        });

        if let Some(parent) = self.config.config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write config
        fs::write(&self.config.config_path, config.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.config.config_path, fs::Permissions::from_mode(0o600))?;
        }
        info!("Generated sing-box config at {:?}", self.config.config_path);

        Ok(())
    }
}
