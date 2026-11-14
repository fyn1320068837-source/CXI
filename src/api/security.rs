const SECURITY_EVIDENCE_TTL_SECONDS: i64 = 90;
const SECURITY_EVIDENCE_MATCH_WINDOW_SECONDS: i64 = 30;

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
    let status_map = manager.status_all().await;
    let pid_map = manager.pid_all().await;
    let validation_map = manager.validation_all().await;
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

    let mut reasons = Vec::new();
    let verdict = if has_direct || validation_name == "fail" {
        reasons.push("检测到公网直连或验证失败".to_string());
        "FAIL"
    } else if !web_running {
        reasons.push("Web 实例未运行，没有新鲜连接样本".to_string());
        "UNKNOWN"
    } else if !proxy_healthy && firewall_mode != "blocked" {
        reasons.push("代理异常但 PF 未处于阻断态".to_string());
        "FAIL"
    } else if !proxy_healthy && firewall_mode == "blocked" {
        reasons.push("代理异常，PF 已阻断实例直连".to_string());
        "BLOCKED"
    } else if !geo_ready || !evidence_fresh || validation_name != "pass" || !has_proxy {
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
        "UNKNOWN"
    } else {
        "PASS"
    };
    let label = match verdict {
        "PASS" => "安全 · 进程级观测通过",
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
        "checks": {
            "proxy_healthy": proxy_healthy,
            "geo_ready": geo_ready,
            "validation_pass": validation_name == "pass" && evidence_fresh,
            "direct_zero": direct == Some(0),
            "udp53": "not_sampled",
            "webrtc_public_candidates": "not_sampled",
            "ipv6_direct": "not_sampled",
        },
        "reasons": reasons,
        "limitations": [
            "结论基于 Web 进程树 lsof 观测，不是全局抓包保证",
            "UDP 53、WebRTC 公网候选和 IPv6-only 需要单独主动测试",
            "请求 URL 日志只能说明访问目标，不能作为直连判定依据",
        ],
    }))
}
