use anyhow::{Context, Result};

use crate::{config, daemon, firewall, instance};

pub(crate) async fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("daemon") => daemon::run_daemon().await?,
        Some("init-db") => daemon::init_database().await?,
        Some("install-firewall") => install_firewall().await?,
        Some("uninstall") => uninstall().await?,
        Some("stop-all") => stop_all_instances().await?,
        Some("status") => show_status().await?,
        Some("desktop-auth") => {
            if args.get(1).map(String::as_str) == Some("--watch") {
                watch_desktop_auth().await?
            } else {
                open_desktop_auth().await?
            }
        }
        _ => print_help(),
    }
    Ok(())
}

fn print_help() {
    println!(
        "CIM - Claude Instance Manager v{}",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("用法:");
    println!("  cim daemon              启动守护进程");
    println!("  cim init-db             初始化数据库");
    println!("  cim install-firewall    安装 PF 防火墙规则");
    println!("  cim uninstall           卸载 CIM");
    println!("  cim stop-all            停止所有实例");
    println!("  cim status              显示状态");
    println!("  cim desktop-auth       将剪贴板中的 Desktop OAuth URL 交给 CIM Web");
    println!("  cim desktop-auth --watch  自动等待系统浏览器 OAuth 页并转交 CIM Web");
    println!();
    println!("控制台: http://localhost:8080");
}

/// Hand off the one-time Desktop OAuth URL copied from the system browser to
/// the isolated CIM Web profile. Reading from stdin/clipboard keeps the URL
/// out of the shell command line and therefore out of process listings and
/// shell history. The daemon validates the host and never logs query values.
async fn open_desktop_auth() -> Result<()> {
    let output = std::process::Command::new("pbpaste")
        .output()
        .context("failed to read the macOS clipboard with pbpaste")?;
    if !output.status.success() {
        anyhow::bail!("pbpaste failed; copy the complete Desktop OAuth URL first");
    }
    let url = String::from_utf8(output.stdout)
        .context("clipboard does not contain valid UTF-8")?
        .trim()
        .to_string();
    if url.is_empty() {
        anyhow::bail!("clipboard is empty; copy the complete Desktop OAuth URL first");
    }

    post_desktop_auth_url(&url).await
}

async fn post_desktop_auth_url(url: &str) -> Result<()> {
    let base_url = daemon_base_url()?;
    // The first handoff may start CIM Web and wait for Chromium DevTools to
    // become ready. Keep the short five-second timeout for ordinary CLI
    // status calls, but allow this one-time browser startup to settle.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .context("failed to create Desktop auth HTTP client")?;
    let response = client
        .post(format!("{base_url}/api/instances/web/open-auth"))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .context("failed to contact the CIM daemon")?;
    let payload = response
        .json::<serde_json::Value>()
        .await
        .context("CIM daemon returned invalid Desktop auth response")?;
    if payload.get("success").and_then(|value| value.as_bool()) != Some(true) {
        let message = payload
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("CIM rejected the Desktop auth URL");
        anyhow::bail!("{message}");
    }
    println!("✓ Desktop OAuth URL 已转交 CIM Web；不要在系统默认浏览器中输入账号");
    Ok(())
}

/// Watch the active tab of common macOS browsers for a short, explicit login
/// window. This removes the copy/paste step without changing LaunchServices
/// or the system default browser. The AppleScript is static and the URL is
/// only sent to the daemon after the local auth-host allowlist check.
async fn watch_desktop_auth() -> Result<()> {
    use std::time::{Duration, Instant};

    const WATCH_TIMEOUT: Duration = Duration::from_secs(120);
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    let browsers = [
        (
            "Microsoft Edge",
            r#"tell application "Microsoft Edge"
repeat with w in windows
    repeat with t in tabs of w
        set u to URL of t
        if (u contains "accounts.google.com") or (u contains "accounts.googleusercontent.com") or (u contains "claude.ai/login/") or (u contains "claude.ai/oauth/") or (u contains "auth.anthropic.com/oauth/") or (u contains "auth.anthropic.com/authorize") then return u
    end repeat
end repeat
return ""
end tell"#,
        ),
        (
            "Google Chrome",
            r#"tell application "Google Chrome"
repeat with w in windows
    repeat with t in tabs of w
        set u to URL of t
        if (u contains "accounts.google.com") or (u contains "accounts.googleusercontent.com") or (u contains "claude.ai/login/") or (u contains "claude.ai/oauth/") or (u contains "auth.anthropic.com/oauth/") or (u contains "auth.anthropic.com/authorize") then return u
    end repeat
end repeat
return ""
end tell"#,
        ),
        (
            "Safari",
            r#"tell application "Safari"
repeat with w in windows
    repeat with t in tabs of w
        set u to URL of t
        if (u contains "accounts.google.com") or (u contains "accounts.googleusercontent.com") or (u contains "claude.ai/login/") or (u contains "claude.ai/oauth/") or (u contains "auth.anthropic.com/oauth/") or (u contains "auth.anthropic.com/authorize") then return u
    end repeat
end repeat
return ""
end tell"#,
        ),
    ];

    println!("等待系统浏览器中的 Desktop OAuth 页面（最多 120 秒）…");
    let deadline = Instant::now() + WATCH_TIMEOUT;
    let mut automation_denied = false;
    while Instant::now() < deadline {
        for (browser, script) in browsers {
            match read_browser_url(browser, script).await {
                Ok(Some(url)) if is_auth_url_candidate(&url) => {
                    return post_desktop_auth_url(&url).await;
                }
                Ok(_) => {}
                Err(error) if error.to_string() == "automation_permission" => {
                    automation_denied = true;
                }
                Err(_) => {}
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    if automation_denied {
        anyhow::bail!(
            "macOS 未允许终端控制浏览器；请在‘系统设置 → 隐私与安全性 → 自动化’中允许终端控制 Edge/Chrome，然后重试 `cim desktop-auth --watch`"
        );
    }
    anyhow::bail!("120 秒内没有发现 Claude/Google OAuth 页面；请重新点击 Desktop 登录后重试");
}

async fn read_browser_url(browser: &str, script: &str) -> Result<Option<String>> {
    let output = tokio::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .await
        .with_context(|| format!("failed to query {browser}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("not authorized") || stderr.contains("not permitted") {
            anyhow::bail!("automation_permission");
        }
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        Ok(None)
    } else {
        Ok(Some(url))
    }
}

fn is_auth_url_candidate(raw_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "accounts.google.com" | "accounts.googleusercontent.com"
    ) {
        return true;
    }
    let path = url.path().to_ascii_lowercase();
    matches!(host.as_str(), "claude.ai" | "www.claude.ai")
        && (path.starts_with("/login/") || path.starts_with("/oauth/") || path.starts_with("/sso"))
        || host == "auth.anthropic.com"
            && (path.starts_with("/oauth/") || path.starts_with("/authorize"))
}

async fn install_firewall() -> Result<()> {
    let config = config::Config::load()?;
    let controller = firewall::FirewallController::new(config.firewall.clone());
    controller.install()?;
    println!("PF fail-closed rules installed.");
    println!(
        "Add the CIM anchor to /etc/pf.conf, then start cim daemon to enable recovery/block transitions:"
    );
    println!("  anchor \"{}\"", config.firewall.anchor_name);
    println!(
        "  load anchor \"{}\" from \"{}\"",
        config.firewall.anchor_name,
        controller.block_rules_path().display()
    );
    println!(
        "  healthy-state rules: \"{}\" (proxy ports only; direct egress remains blocked)",
        controller.allow_rules_path().display()
    );

    Ok(())
}

async fn uninstall() -> Result<()> {
    println!("Uninstalling CIM...");
    println!("Run the uninstall.sh script instead.");

    Ok(())
}

async fn stop_all_instances() -> Result<()> {
    let base_url = daemon_base_url()?;
    let client = daemon_client()?;
    let mut failures = Vec::new();

    for kind in instance::InstanceKind::ALL {
        let url = format!("{base_url}/api/instances/{}/stop", kind.as_str());
        match client.post(url).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<serde_json::Value>().await {
                    Ok(payload)
                        if payload.get("success").and_then(|value| value.as_bool())
                            == Some(false) =>
                    {
                        failures.push(format!("{}: {}", kind.as_str(), payload));
                    }
                    Ok(_) => println!("✓ {} stopped", kind.as_str()),
                    Err(error) => failures.push(format!(
                        "{}: invalid daemon response: {error}",
                        kind.as_str()
                    )),
                },
                Err(error) => failures.push(format!("{}: {error}", kind.as_str())),
            },
            Err(error) => failures.push(format!("{}: {error}", kind.as_str())),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "failed to stop instances: {}",
            failures.join("; ")
        ))
    }
}

async fn show_status() -> Result<()> {
    let base_url = daemon_base_url()?;
    let client = daemon_client()?;
    let response = client
        .get(format!("{base_url}/api/status"))
        .send()
        .await
        .context("CIM daemon is unavailable; start `cim daemon` first")?
        .error_for_status()
        .context("CIM daemon returned an error while reading status")?;
    let payload = response
        .json::<serde_json::Value>()
        .await
        .context("CIM daemon returned invalid status JSON")?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn daemon_base_url() -> Result<String> {
    let config = config::Config::load()?;
    // CLI commands can carry OAuth URLs or control instances. Apply the same
    // loopback-only boundary as the daemon itself before making any request.
    daemon::validate_loopback_listen_addr(&config.daemon.listen_addr)?;
    Ok(daemon_url_for_listen_addr(&config.daemon.listen_addr))
}

fn daemon_url_for_listen_addr(listen_addr: &str) -> String {
    format!("http://{listen_addr}")
}

fn daemon_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to create local daemon HTTP client")
}

#[cfg(test)]
mod cli_tests {
    use anyhow::Result;

    #[test]
    fn daemon_url_uses_config_listen_address() {
        assert_eq!(
            super::daemon_url_for_listen_addr("127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            super::daemon_url_for_listen_addr("[::1]:8080"),
            "http://[::1]:8080"
        );
    }

    #[test]
    fn daemon_listen_address_must_be_loopback() -> Result<()> {
        super::daemon::validate_loopback_listen_addr("127.0.0.1:8080")?;
        super::daemon::validate_loopback_listen_addr("[::1]:8080")?;
        assert!(super::daemon::validate_loopback_listen_addr("0.0.0.0:8080").is_err());
        assert!(super::daemon::validate_loopback_listen_addr("192.168.1.10:8080").is_err());
        Ok(())
    }

    #[test]
    fn auth_watch_only_accepts_https_login_hosts() {
        assert!(super::is_auth_url_candidate(
            "https://accounts.google.com/o/oauth2/auth?state=redacted"
        ));
        assert!(super::is_auth_url_candidate(
            "https://claude.ai/login/popup-google-auth?state=redacted"
        ));
        assert!(!super::is_auth_url_candidate(
            "http://accounts.google.com/login"
        ));
        assert!(!super::is_auth_url_candidate("https://evil.example/login"));
        assert!(!super::is_auth_url_candidate(
            "https://user:secret@accounts.google.com/login"
        ));
        assert!(!super::is_auth_url_candidate("https://claude.ai/chat"));
    }
}
