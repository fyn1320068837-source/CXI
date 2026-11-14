async fn restart_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let _ = state.tx.send(r#"{"type":"proxy_restarting"}"#.to_string());

    match state.proxy_manager.restart().await {
        Ok(()) => {
            match state.proxy_manager.refresh_geo_profile().await {
                Ok(_) => state.proxy_manager.mark_healthy(),
                Err(error) => {
                    tracing::warn!("GeoIP refresh after proxy restart failed: {}", error);
                    match state.proxy_manager.check_exit_ip().await {
                        Ok(_) => state.proxy_manager.mark_healthy(),
                        Err(proxy_error) => {
                            state.proxy_manager.mark_unhealthy();
                            tracing::warn!(
                                "Proxy health check after restart failed: {}",
                                proxy_error
                            );
                        }
                    }
                }
            }
            if state.proxy_manager.is_healthy() {
                let _ = state.tx.send(r#"{"type":"proxy_restarted"}"#.to_string());
                Json(serde_json::json!({
                    "success": true,
                    "message": "sing-box 已重启，新配置生效"
                }))
            } else {
                Json(serde_json::json!({
                    "success": false,
                    "message": "sing-box 已启动，但 fail-closed 尚未解除"
                }))
            }
        }
        Err(error) => Json(serde_json::json!({
            "success": false,
            "message": format!("重启失败: {}", error)
        })),
    }
}

async fn get_proxy_config(
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 从配置文件读取（不含密码）
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = format!("{}/.cim/config.toml", home);

    // 尝试解析 config.toml 中的代理设置
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        // 简单提取 host/port/username（生产中用 toml 库解析）
        let host = extract_toml_str(&content, "host").unwrap_or_default();
        let port = extract_toml_num(&content, "port").unwrap_or(0);
        let username = extract_toml_str(&content, "username");
        let configured = !host.is_empty() && port > 0;

        return Json(serde_json::json!({
            "configured": configured,
            "host": host,
            "port": port,
            "username": username,
        }));
    }

    Json(serde_json::json!({
        "configured": false,
        "host": "",
        "port": 0,
        "username": null,
    }))
}

fn extract_toml_str(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{} =", key)) || trimmed.starts_with(&format!("{}=", key)) {
            if let Some((_, val)) = trimmed.split_once('=') {
                let val = val.trim().trim_matches('"').to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}

fn extract_toml_num(content: &str, key: &str) -> Option<u16> {
    extract_toml_str(content, key)?.parse().ok()
}

#[derive(Deserialize)]
struct SaveProxyRequest {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

async fn save_proxy_config(
    axum::extract::State(_state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<SaveProxyRequest>,
) -> Json<serde_json::Value> {
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = format!("{}/.cim/config.toml", home);

    // 读取现有配置
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();

    // 更新或添加 [proxy] 下的 host/port/username
    let new_content =
        update_proxy_in_toml(&content, &payload.host, payload.port, &payload.username);

    if let Err(e) = std::fs::write(&config_path, &new_content) {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("写入配置失败: {}", e)
        }));
    }

    // 密码保存到 Keychain（macOS）
    if let Some(ref pwd) = payload.password {
        if !pwd.is_empty() {
            #[cfg(target_os = "macos")]
            {
                use crate::storage::keychain;
                let _ = keychain::save_password("com.cim.proxy", &payload.host, pwd);
            }
        }
    }

    Json(serde_json::json!({
        "success": true,
        "message": "代理配置已保存"
    }))
}

fn update_proxy_in_toml(content: &str, host: &str, port: u16, username: &Option<String>) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut in_proxy = false;
    let mut host_set = false;
    let mut port_set = false;
    let mut user_set = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed == "[proxy]" {
            in_proxy = true;
            continue;
        }
        if trimmed.starts_with('[') && trimmed != "[proxy]" {
            in_proxy = false;
        }

        if in_proxy {
            if trimmed.starts_with("host") {
                *line = format!("host = \"{}\"", host);
                host_set = true;
            } else if trimmed.starts_with("port") {
                *line = format!("port = {}", port);
                port_set = true;
            } else if trimmed.starts_with("username") {
                *line = match username {
                    Some(u) if !u.is_empty() => format!("username = \"{}\"", u),
                    _ => "username = \"\"".to_string(),
                };
                user_set = true;
            }
        }
    }

    // 如果 [proxy] 区域没有这些字段，追加
    if in_proxy && (!host_set || !port_set || !user_set) {
        if !host_set {
            lines.push(format!("host = \"{}\"", host));
        }
        if !port_set {
            lines.push(format!("port = {}", port));
        }
        if !user_set {
            if let Some(u) = username {
                lines.push(format!("username = \"{}\"", u));
            }
        }
    }

    lines.join("\n")
}

async fn test_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let start = std::time::Instant::now();

    // 用 curl + socks5，和健康检查保持一致，避免 HTTP 代理走直连
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-x",
            "socks5://127.0.0.1:7891",
            "--max-time",
            "8",
            "https://ipv4.icanhazip.com",
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let latency_ms = start.elapsed().as_millis() as u64;
            if !ip.is_empty() {
                let parsed_ip = match ip.parse::<std::net::IpAddr>() {
                    Ok(parsed_ip) => parsed_ip,
                    Err(error) => {
                        return Json(serde_json::json!({
                            "healthy": false,
                            "error": "PROXY_INVALID_EXIT_IP",
                            "detail": error.to_string()
                        }));
                    }
                };
                state.proxy_manager.mark_healthy();
                if !state.proxy_manager.is_healthy() {
                    return Json(serde_json::json!({
                        "healthy": false,
                        "error": "FAIL_CLOSED_NOT_READY",
                        "detail": "PF fail-closed 未安装或未处于放行态"
                    }));
                }
                let geo = match state.proxy_manager.refresh_geo_profile_for_ip(&ip).await {
                    Ok(profile) => Some(geo_info_from_profile(&profile)),
                    Err(error) => {
                        tracing::warn!("GeoIP resolution failed for {}: {}", parsed_ip, error);
                        None
                    }
                };
                Json(serde_json::json!({
                    "healthy": true,
                    "exit_ip": ip,
                    "latency_ms": latency_ms,
                    "geo": geo
                }))
            } else {
                Json(serde_json::json!({
                    "healthy": false,
                    "error": "PROXY_NO_RESPONSE"
                }))
            }
        }
        Ok(out) => {
            state.proxy_manager.mark_unhealthy();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let reason = if stderr.contains("timed out") || stderr.contains("Operation timed out") {
                "PROXY_TIMEOUT"
            } else if stderr.contains("Authentication") || stderr.contains("407") {
                "PROXY_AUTH_FAILED"
            } else if stderr.contains("Connection refused") {
                "PROXY_NOT_RUNNING"
            } else {
                "PROXY_ERROR"
            };
            Json(serde_json::json!({
                "healthy": false,
                "error": reason,
                "detail": stderr
            }))
        }
        Err(e) => {
            state.proxy_manager.mark_unhealthy();
            Json(serde_json::json!({
                "healthy": false,
                "error": "CURL_NOT_FOUND",
                "detail": e.to_string()
            }))
        }
    }
}
