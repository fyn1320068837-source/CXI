const SECURITY_EVIDENCE_TTL_SECONDS: i64 = 90;
const SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS: i64 = 30;
const ACTIVE_PROBE_TTL_SECONDS: i64 = 600;

fn active_probe_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cim/evidence/web-active.json")
}

fn load_active_probe() -> Option<serde_json::Value> {
    let bytes = std::fs::read(active_probe_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn active_probe_status(
    payload: Option<&serde_json::Value>,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
    current_root_pid: Option<u32>,
    current_exit_ip: Option<&str>,
) -> &'static str {
    let Some(payload) = payload else {
        return "not_sampled";
    };
    // Evidence belongs to one running Web instance and proxy exit. Never
    // promote a previous instance's result after a restart or exit change.
    let (Some(root_pid), Some(exit_ip)) = (current_root_pid, current_exit_ip) else {
        return "not_sampled";
    };
    if root_pid == 0
        || exit_ip.parse::<std::net::IpAddr>().is_err()
        || payload.get("root_pid").and_then(serde_json::Value::as_u64) != Some(u64::from(root_pid))
        || payload.get("exit_ip").and_then(serde_json::Value::as_str) != Some(exit_ip)
    {
        return "not_sampled";
    }
    let Some(probe) = payload.get(key) else {
        return "not_sampled";
    };
    let Some(sampled_at) = probe.get("sampled_at").and_then(serde_json::Value::as_str) else {
        return "not_sampled";
    };
    let Ok(sampled_at) = chrono::DateTime::parse_from_rfc3339(sampled_at) else {
        return "not_sampled";
    };
    let sampled_at = sampled_at.with_timezone(&chrono::Utc);
    if sampled_at > now {
        return "not_sampled";
    }
    if now - sampled_at > chrono::Duration::seconds(ACTIVE_PROBE_TTL_SECONDS) {
        return "expired";
    }
    if probe
        .get("evidence")
        .and_then(serde_json::Value::as_str)
        .map_or(true, |evidence| evidence.trim().is_empty())
    {
        return "not_sampled";
    }
    match probe.get("status").and_then(serde_json::Value::as_str) {
        Some("pass") => "pass",
        Some("fail") => "fail",
        _ => "not_sampled",
    }
}

fn active_probe_evidence<'a>(payload: Option<&'a serde_json::Value>, key: &str) -> &'a str {
    payload
        .and_then(|value| value.get(key))
        .and_then(|value| value.get("evidence"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("requires active probe evidence")
}

fn active_probe_overall_status(statuses: [&str; 3]) -> &'static str {
    if statuses.contains(&"fail") {
        "fail"
    } else if statuses.iter().all(|status| *status == "pass") {
        "pass"
    } else if statuses.contains(&"expired") {
        "expired"
    } else {
        "not_sampled"
    }
}

fn evidence_state(has_timestamp: bool, fresh: bool) -> &'static str {
    if fresh {
        "fresh"
    } else if has_timestamp {
        "expired"
    } else {
        "missing"
    }
}

fn fail_closed_state(
    proxy_healthy: bool,
    installed: bool,
    mode: &str,
    global_verified: bool,
) -> &'static str {
    if !installed {
        return "unavailable";
    }
    if !global_verified {
        return "unverified";
    }
    if (proxy_healthy && mode == "allowed") || (!proxy_healthy && mode == "blocked") {
        "enforced"
    } else {
        "mismatch"
    }
}

fn firewall_security_verdict(
    proxy_healthy: bool,
    installed: bool,
    mode: &str,
    global_verified: bool,
) -> Option<&'static str> {
    match fail_closed_state(proxy_healthy, installed, mode, global_verified) {
        "enforced" if proxy_healthy => None,
        "enforced" => Some("BLOCKED"),
        "unverified" => Some("UNKNOWN"),
        _ => Some("FAIL"),
    }
}

struct WebCheckInput<'a> {
    proxy_healthy: bool,
    geo_ready: bool,
    firewall_mode: &'a str,
    firewall_installed: bool,
    firewall_global_verified: bool,
    validation_pass: bool,
    direct_zero: bool,
    evidence_fresh: bool,
    web_running: bool,
    has_timestamp: bool,
    dns_status: &'a str,
    webrtc_status: &'a str,
    ipv6_status: &'a str,
}

fn web_check_matrix(input: &WebCheckInput<'_>) -> serde_json::Value {
    let fail_closed = fail_closed_state(
        input.proxy_healthy,
        input.firewall_installed,
        input.firewall_mode,
        input.firewall_global_verified,
    );
    serde_json::json!({
        // These are the checks that can be enforced without a VM. Keep the
        // evidence source explicit so a configured browser flag is not
        // confused with packet-level proof.
        "proxy_and_exit": {
            "status": if input.proxy_healthy && input.geo_ready { "pass" } else if input.proxy_healthy { "pending" } else { "fail" },
            "evidence": "sing-box health + GeoIP profile",
        },
        "proxy_fail_closed": {
            "status": fail_closed,
            "global_verified": input.firewall_global_verified,
            "evidence": "PF global enablement + root anchor linkage + anchor rules + proxy health",
        },
        "process_connections": {
            "status": if !input.web_running { "not_running" } else if input.validation_pass && input.direct_zero { "pass" } else if input.direct_zero { "pending" } else { "fail" },
            "evidence": "recursive lsof process-tree snapshot",
        },
        "browser_policy": {
            "status": "configured",
            "evidence": "SOCKS5, host-resolver, WebRTC and QUIC launch policy",
        },
        "cdp_devtools": {
            "status": "loopback_only",
            "evidence": "random loopback port with restricted Origin; no authentication token",
            "risk": "same-host processes may still probe the DevTools endpoint",
        },
        "workspace_scope": {
            "status": "isolated_profile",
            "evidence": "per-instance/per-session data directory",
        },
        "evidence": {
            "status": evidence_state(input.has_timestamp, input.evidence_fresh),
            "scope": "web_process_tree",
            "source": "lsof",
        },
        "dns_udp53": {
            "status": input.dns_status,
            "evidence": "active tcpdump/UDP 53 probe",
        },
        "webrtc_public_candidates": {
            "status": input.webrtc_status,
            "evidence": "active CDP ICE probe",
        },
        "ipv6_direct": {
            "status": input.ipv6_status,
            "evidence": "active IPv6-only probe",
        },
    })
}

fn web_security_verdict(
    input: &WebCheckInput<'_>,
    has_direct: bool,
    validation_failed: bool,
    has_proxy: bool,
) -> &'static str {
    let probe_status =
        active_probe_overall_status([input.dns_status, input.webrtc_status, input.ipv6_status]);
    if has_direct || validation_failed || probe_status == "fail" {
        "FAIL"
    } else if !input.web_running {
        "UNKNOWN"
    } else if let Some(verdict) = firewall_security_verdict(
        input.proxy_healthy,
        input.firewall_installed,
        input.firewall_mode,
        input.firewall_global_verified,
    ) {
        verdict
    } else if !input.geo_ready
        || !input.evidence_fresh
        || !input.validation_pass
        || !input.direct_zero
        || !has_proxy
        || probe_status != "pass"
    {
        "UNKNOWN"
    } else {
        "PASS"
    }
}

fn parse_detail_count(details: &str, key: &str) -> usize {
    let Some(value) = details
        .split_once(&format!("{key}="))
        .map(|(_, value)| value)
    else {
        return 0;
    };
    value
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}
fn parse_checked_pids(details: &str) -> usize {
    details
        .split_once("checked ")
        .and_then(|(_, value)| value.split_once(" pids"))
        .and_then(|(value, _)| value.parse().ok())
        .unwrap_or(0)
}

fn direct_endpoints(details: &str) -> Vec<String> {
    let Some((_, value)) = details.split_once("direct=") else {
        return Vec::new();
    };
    if value.trim_start().starts_with('0') {
        return Vec::new();
    }
    value
        .split(", ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

async fn get_web_security(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let manager = state.instance_manager.read().await;
    let snapshot = manager.snapshot_all().await;
    let status_map = &snapshot.status;
    let pid_map = &snapshot.pids;
    let validation_map = &snapshot.validation;
    let geo_profile = state.geo_profile.read().await.clone();
    let firewall = state.proxy_manager.firewall_status();
    let proxy_healthy = state.proxy_manager.is_healthy();
    let geo_ready = geo_profile.is_ready();
    let web_running = status_map.get(&InstanceKind::Web).copied().unwrap_or(false);
    let web_pid = pid_map.get(&InstanceKind::Web).copied().flatten();
    let validation = validation_map.get(&InstanceKind::Web);
    let validation_name = validation
        .map(|value| validation_name(&value.status))
        .unwrap_or("unknown");
    let validation_checked_at = validation.and_then(|value| value.checked_at.clone());
    let evidence = state
        .storage
        .recent_validation_evidence_async("web", 1)
        .await
        .ok()
        .and_then(|values| values.into_iter().next());
    let details = evidence
        .as_ref()
        .and_then(|value| value.details.clone())
        .unwrap_or_default();
    let checked_at = evidence
        .as_ref()
        .map(|value| value.timestamp.clone())
        .or(validation_checked_at.clone());
    let age_seconds = checked_at.as_deref().and_then(|timestamp| {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(|value| {
                (Utc::now() - value.with_timezone(&Utc))
                    .num_seconds()
                    .max(0)
            })
    });
    let evidence_matches_validation = match (
        evidence.as_ref().map(|value| value.timestamp.as_str()),
        validation_checked_at.as_deref(),
    ) {
        (Some(evidence_timestamp), Some(validation_timestamp)) => {
            match (
                chrono::DateTime::parse_from_rfc3339(evidence_timestamp),
                chrono::DateTime::parse_from_rfc3339(validation_timestamp),
            ) {
                (Ok(evidence_time), Ok(validation_time)) => {
                    (validation_time - evidence_time).num_seconds().abs()
                        <= SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS
                }
                _ => false,
            }
        }
        _ => false,
    };
    let evidence_fresh = age_seconds
        .map(|age| age <= SECURITY_EVIDENCE_TTL_SECONDS)
        .unwrap_or(false)
        && evidence_matches_validation;
    // Only expose counters from a fresh snapshot. A stopped/restarted Web
    // instance must not inherit the previous process tree's evidence.
    let current_evidence = evidence.as_ref().filter(|_| evidence_fresh);
    let proxied = current_evidence
        .map(|value| value.proxied_count)
        .or_else(|| {
            if evidence_fresh {
                Some(parse_detail_count(&details, "proxied"))
            } else {
                None
            }
        });
    let local = current_evidence.map(|value| value.local_count).or_else(|| {
        if evidence_fresh {
            Some(parse_detail_count(&details, "local"))
        } else {
            None
        }
    });
    let direct = current_evidence
        .map(|value| value.direct_count)
        .or_else(|| {
            if evidence_fresh {
                Some(parse_detail_count(&details, "direct"))
            } else {
                None
            }
        });
    let checked_pids = if evidence_fresh {
        parse_checked_pids(&details)
    } else {
        0
    };
    let direct_items = if evidence_fresh {
        direct_endpoints(&details)
    } else {
        Vec::new()
    };
    let has_direct = direct.map(|value| value > 0).unwrap_or(false);
    let has_proxy = proxied.map(|value| value > 0).unwrap_or(false);
    let firewall_mode = format!("{:?}", firewall.mode).to_lowercase();
    let firewall_global_verified = firewall.global_enabled && firewall.root_anchor_loaded;
    let validation_pass = validation_name == "pass" && evidence_fresh;
    let direct_zero = direct == Some(0);
    let active_probe = load_active_probe();
    let active_probe_ref = active_probe.as_ref();
    let probe_now = Utc::now();
    let probe_status = |key| {
        active_probe_status(
            active_probe_ref,
            key,
            probe_now,
            web_pid.filter(|_| web_running),
            geo_profile.source_ip.as_deref(),
        )
    };
    let dns_status = probe_status("dns_udp53");
    let webrtc_status = probe_status("webrtc_public_candidates");
    let ipv6_status = probe_status("ipv6_direct");
    let active_probe_summary =
        active_probe_overall_status([dns_status, webrtc_status, ipv6_status]);
    let active_probe_complete = active_probe_summary == "pass";
    let check_input = WebCheckInput {
        proxy_healthy,
        geo_ready,
        firewall_mode: &firewall_mode,
        firewall_installed: firewall.installed,
        firewall_global_verified,
        validation_pass,
        direct_zero,
        evidence_fresh,
        web_running,
        has_timestamp: evidence.is_some(),
        dns_status,
        webrtc_status,
        ipv6_status,
    };
    let check_matrix = web_check_matrix(&check_input);

    let mut reasons = Vec::new();
    let verdict = web_security_verdict(
        &check_input,
        has_direct,
        validation_name == "fail",
        has_proxy,
    );
    if verdict == "FAIL" {
        if fail_closed_state(
            proxy_healthy,
            firewall.installed,
            &firewall_mode,
            firewall_global_verified,
        ) != "enforced"
        {
            reasons.push("PF 未安装、全局状态未验证或实际模式与代理健康状态不匹配".to_string());
        } else {
            reasons.push("检测到公网直连或验证失败".to_string());
        }
    } else if verdict == "BLOCKED" {
        reasons.push("代理异常，PF 已阻断实例直连".to_string());
    } else if verdict == "UNKNOWN" {
        if !firewall_global_verified {
            reasons.push("PF 全局启用状态或主规则集 anchor 挂载尚未验证".to_string());
        }
        if !web_running {
            reasons.push("Web 实例未运行，没有新鲜连接样本".to_string());
        }
        if !geo_ready {
            reasons.push("GeoIP 位置资料未就绪".to_string());
        }
        if !evidence_fresh {
            reasons.push("网络验证证据不存在或已过期".to_string());
        }
        if !has_proxy {
            reasons.push(if proxied.is_some() {
                "没有观测到代理连接".to_string()
            } else {
                "没有新鲜代理连接样本".to_string()
            });
        }
        if !active_probe_complete {
            reasons.push(
                "DNS、WebRTC 或 IPv6 主动探测证据缺失、失效或不属于当前实例/出口".to_string(),
            );
        }
    }
    let label = match verdict {
        "PASS" => "安全 · 代理、PF、进程及主动泄漏检查均通过",
        "FAIL" => "风险 · 发现直连或验证失败",
        "BLOCKED" => "已阻断 · 代理故障期间 PF 生效",
        _ => "待确认 · 证据不足或已过期",
    };

    Json(serde_json::json!({
        "success": true,
        "verdict": verdict,
        "label": label,
        "checked_at": checked_at,
        "age_seconds": age_seconds,
        "evidence_ttl_seconds": SECURITY_EVIDENCE_TTL_SECONDS,
        "proxy": {
            "healthy": proxy_healthy,
            "geo_ready": geo_ready,
            "geo_status": if geo_ready { "ready" } else if geo_profile.source_ip.is_some() { "pending" } else { "unavailable" },
            "exit_ip": geo_profile.source_ip,
        },
        "firewall": {
            "installed": firewall.installed,
            "mode": firewall_mode,
            "global_enabled": firewall.global_enabled,
            "root_anchor_loaded": firewall.root_anchor_loaded,
            "global_verified": firewall_global_verified,
        },
        "web": {
            "running": web_running,
            "pid": web_pid,
            "validation": validation_name,
            "validation_checked_at": validation_checked_at,
            "checked_pids": checked_pids,
            "proxied": proxied,
            "local": local,
            "direct": direct,
            "direct_endpoints": direct_items,
        },
        "scope": "web_process_tree",
        "assurance": "process_tree",
        "active_probe_status": active_probe_summary,
        "active_probe": {
            "path": active_probe_path(),
            "ttl_seconds": ACTIVE_PROBE_TTL_SECONDS,
            "root_pid": active_probe_ref.and_then(|value| value.get("root_pid")),
            "exit_ip": active_probe_ref.and_then(|value| value.get("exit_ip")),
            "dns_udp53": {"status": dns_status, "sampled_at": active_probe_ref.and_then(|value| value.pointer("/dns_udp53/sampled_at")), "evidence": active_probe_evidence(active_probe_ref, "dns_udp53")},
            "webrtc_public_candidates": {"status": webrtc_status, "sampled_at": active_probe_ref.and_then(|value| value.pointer("/webrtc_public_candidates/sampled_at")), "evidence": active_probe_evidence(active_probe_ref, "webrtc_public_candidates")},
            "ipv6_direct": {"status": ipv6_status, "sampled_at": active_probe_ref.and_then(|value| value.pointer("/ipv6_direct/sampled_at")), "evidence": active_probe_evidence(active_probe_ref, "ipv6_direct")},
        },
        "check_matrix": check_matrix,
        "checks": {
            "proxy_healthy": proxy_healthy,
            "geo_ready": geo_ready,
            "pf_fail_closed": fail_closed_state(proxy_healthy, firewall.installed, &firewall_mode, firewall_global_verified) == "enforced",
            "pf_global_verified": firewall_global_verified,
            "validation_pass": validation_pass,
            "direct_zero": direct_zero,
            "udp53": dns_status,
            "webrtc_public_candidates": webrtc_status,
            "ipv6_direct": ipv6_status,
        },
        "reasons": reasons,
        "limitations": [
            "结论基于 Web 进程树 lsof 观测，不是全局抓包保证",
            "主动探测按各项真实采样时间在 10 分钟后过期；实例 PID 或出口 IP 变化需重新验证",
            "请求 URL 日志只能说明访问目标，不能作为直连判定依据",
        ],
    }))
}

/// Desktop Lite has a deliberately narrower conclusion than Web. A fresh
/// process-level network PASS is useful evidence, but it does not prove that
/// Claude Desktop's native location service or every Electron setting was
/// overridden. Keep those dimensions explicit so the console cannot turn a
/// network-only result into a blanket privacy claim.
async fn get_desktop_security(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let manager = state.instance_manager.read().await;
    let snapshot = manager.snapshot_all().await;
    let status_map = &snapshot.status;
    let pid_map = &snapshot.pids;
    let validation_map = &snapshot.validation;
    let firewall = state.proxy_manager.firewall_status();
    let proxy_healthy = state.proxy_manager.is_healthy();
    let isolation_helper_ready = crate::instance::isolation_ready();
    let desktop_running = status_map
        .get(&InstanceKind::Desktop)
        .copied()
        .unwrap_or(false);
    let desktop_pid = pid_map.get(&InstanceKind::Desktop).copied().flatten();
    let validation = validation_map.get(&InstanceKind::Desktop);
    let validation_name = validation
        .map(|value| validation_name(&value.status))
        .unwrap_or("unknown");
    let validation_checked_at = validation.and_then(|value| value.checked_at.clone());
    let evidence = state
        .storage
        .recent_validation_evidence_async("desktop", 1)
        .await
        .ok()
        .and_then(|values| values.into_iter().next());
    let details = evidence
        .as_ref()
        .and_then(|value| value.details.clone())
        .or_else(|| validation.map(|value| value.details.clone()))
        .unwrap_or_default();
    let evidence_timestamp = evidence.as_ref().map(|value| value.timestamp.clone());
    let checked_at = evidence_timestamp.clone().or(validation_checked_at.clone());
    let age_seconds = checked_at.as_deref().and_then(|timestamp| {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(|value| {
                (Utc::now() - value.with_timezone(&Utc))
                    .num_seconds()
                    .max(0)
            })
    });
    let evidence_matches_validation = match (
        evidence_timestamp.as_deref(),
        validation_checked_at.as_deref(),
    ) {
        (Some(evidence_timestamp), Some(validation_timestamp)) => match (
            chrono::DateTime::parse_from_rfc3339(evidence_timestamp),
            chrono::DateTime::parse_from_rfc3339(validation_timestamp),
        ) {
            (Ok(evidence_time), Ok(validation_time)) => {
                (validation_time - evidence_time).num_seconds().abs()
                    <= SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS
            }
            _ => false,
        },
        _ => false,
    };
    let evidence_fresh = age_seconds
        .map(|age| age <= SECURITY_EVIDENCE_TTL_SECONDS)
        .unwrap_or(false)
        && evidence_matches_validation;
    let current_evidence = evidence.as_ref().filter(|_| evidence_fresh);
    let proxied = current_evidence
        .map(|value| value.proxied_count)
        .or_else(|| evidence_fresh.then(|| parse_detail_count(&details, "proxied")));
    let local = current_evidence
        .map(|value| value.local_count)
        .or_else(|| evidence_fresh.then(|| parse_detail_count(&details, "local")));
    let direct = current_evidence
        .map(|value| value.direct_count)
        .or_else(|| evidence_fresh.then(|| parse_detail_count(&details, "direct")));
    let checked_pids = if evidence_fresh {
        parse_checked_pids(&details)
    } else {
        0
    };
    let direct_items = if evidence_fresh {
        direct_endpoints(&details)
    } else {
        Vec::new()
    };
    let firewall_mode = format!("{:?}", firewall.mode).to_lowercase();
    let has_direct = direct.map(|value| value > 0).unwrap_or(false);
    let has_proxy = proxied.map(|value| value > 0).unwrap_or(false);
    let firewall_global_verified = firewall.global_enabled && firewall.root_anchor_loaded;
    let network_verdict = if has_direct || validation_name == "fail" {
        "FAIL"
    } else if !desktop_running {
        "UNKNOWN"
    } else if let Some(verdict) = firewall_security_verdict(
        proxy_healthy,
        firewall.installed,
        &firewall_mode,
        firewall_global_verified,
    ) {
        verdict
    } else if !evidence_fresh || validation_name != "pass" || !has_proxy {
        "UNKNOWN"
    } else {
        "PASS"
    };
    let mut reasons = Vec::new();
    let verdict = match network_verdict {
        "FAIL" => {
            reasons.push("Desktop 进程树存在直连或网络验证失败".to_string());
            "FAIL"
        }
        "BLOCKED" => {
            reasons.push("代理异常，PF 已阻断 Desktop 直连".to_string());
            "BLOCKED"
        }
        "PASS" => {
            reasons.push("网络连接均通过代理，未发现直连证据".to_string());
            "PARTIAL"
        }
        _ => {
            if !desktop_running {
                reasons.push("Desktop Lite 未运行，没有新鲜连接样本".to_string());
            } else if !firewall_global_verified {
                reasons.push("PF 全局启用状态或主规则集 anchor 挂载尚未验证".to_string());
            } else {
                reasons.push("Desktop Lite 网络证据不存在或已过期".to_string());
            }
            "UNKNOWN"
        }
    };
    let label = match verdict {
        "PARTIAL" => "部分安全 · 网络通过，位置未覆盖",
        "FAIL" => "风险 · 发现直连或验证失败",
        "BLOCKED" => "已阻断 · 代理故障期间 PF 生效",
        _ => "待确认 · 证据不足或已过期",
    };

    Json(serde_json::json!({
        "success": true,
        "verdict": verdict,
        "label": label,
        "network_verdict": network_verdict,
        "checked_at": checked_at,
        "age_seconds": age_seconds,
        "evidence_ttl_seconds": SECURITY_EVIDENCE_TTL_SECONDS,
        "proxy": {
            "healthy": proxy_healthy,
        },
        "firewall": {
            "installed": firewall.installed,
            "mode": firewall_mode,
            "global_enabled": firewall.global_enabled,
            "root_anchor_loaded": firewall.root_anchor_loaded,
            "global_verified": firewall_global_verified,
        },
        "desktop": {
            "running": desktop_running,
            "pid": desktop_pid,
            "validation": validation_name,
            "validation_checked_at": validation_checked_at,
            "checked_pids": checked_pids,
            "proxied": proxied,
            "local": local,
            "direct": direct,
            "direct_endpoints": direct_items,
        },
        "environment": {
            "status": "partial",
            "timezone": "best_effort",
            "language": "best_effort",
            "details": "Desktop Lite 注入 TZ/LANG/LC_ALL，但原生 Electron 模块仍需实测",
        },
        "location": {
            "status": "unsupported",
            "details": "无法可靠覆盖 Claude Desktop 的 macOS CoreLocation",
        },
        "authentication": {
            "status": "watch_available",
            "browser": "cim_web",
            "automatic": false,
            "watch_command": "cim desktop-auth --watch",
            "details": "Claude Desktop 使用系统外部浏览器完成 OAuth；运行 `cim desktop-auth --watch` 可在 120 秒内自动转交 CIM Web，也可复制一次性 OAuth URL 后运行 `cim desktop-auth`。CIM 不复制 Cookie、Keychain 或 token。",
        },
        "checks": {
            "proxy_healthy": proxy_healthy,
            "isolation_helper": isolation_helper_ready,
            "pf_fail_closed": fail_closed_state(proxy_healthy, firewall.installed, &firewall_mode, firewall_global_verified) == "enforced",
            "pf_global_verified": firewall_global_verified,
            "validation_pass": network_verdict == "PASS" && evidence_fresh,
            "direct_zero": direct == Some(0),
            "udp53": "not_sampled",
            "webrtc_public_candidates": "not_applicable",
            "ipv6_direct": "not_sampled",
        },
        "reasons": reasons,
        "limitations": [
            "网络结论基于 Desktop 进程树 lsof 观测，不是全局抓包保证",
            "时区和语言是 best-effort，CoreLocation 位置保持不支持",
            "UDP 53、IPv6-only 和原生 Electron 模块需要单独主动测试",
        ],
    }))
}
