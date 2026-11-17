// API Server module - HTTP REST + WebSocket endpoints
use crate::geoip::{GeoProfile, SharedGeoProfile};
use crate::instance::{
    isolation_ready, InstanceKind, InstanceManager, RequestLogEntry, ValidationStatus,
};
use crate::proxy::ProxyManager;
use crate::storage::Storage;
use anyhow::{Context, Result};
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    extract::Query,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::services::ServeDir;

// Global state for broadcasting updates
#[derive(Clone)]
pub struct AppState {
    pub tx: broadcast::Sender<String>,
    pub instance_manager: Arc<RwLock<InstanceManager>>,
    pub storage: Arc<Storage>,
    pub log_buffer: Arc<RwLock<Vec<LogEntry>>>,
    pub geo_profile: SharedGeoProfile,
    pub proxy_manager: Arc<ProxyManager>,
}

pub type LogEntry = RequestLogEntry;

const MAX_REQUEST_LOGS: usize = 2_000;
const REQUEST_LOG_RETENTION: ChronoDuration = ChronoDuration::minutes(30);

fn prune_request_logs(logs: &mut Vec<LogEntry>) {
    let cutoff = Utc::now() - REQUEST_LOG_RETENTION;
    logs.retain(|entry| {
        DateTime::parse_from_rfc3339(&entry.timestamp)
            .map(|timestamp| timestamp.with_timezone(&Utc) >= cutoff)
            .unwrap_or(true)
    });
    logs.truncate(MAX_REQUEST_LOGS);
}

pub async fn create_router(
    storage: Arc<Storage>,
    geo_profile: SharedGeoProfile,
    proxy_manager: Arc<ProxyManager>,
    allow_lan: bool,
    instance_configs: crate::config::InstanceConfigs,
) -> Result<(Router, Arc<RwLock<InstanceManager>>)> {
    let (tx, _rx) = broadcast::channel(100);
    let log_buffer: Arc<RwLock<Vec<LogEntry>>> = Arc::new(RwLock::new(Vec::new()));
    let (request_log_tx, mut request_log_rx) = mpsc::channel::<RequestLogEntry>(1024);

    // Keep the in-memory log bounded and fan each browser request out to the
    // dashboard over the existing WebSocket connection.
    let log_buffer_task = log_buffer.clone();
    let tx_task = tx.clone();
    tokio::spawn(async move {
        while let Some(entry) = request_log_rx.recv().await {
            {
                let mut logs = log_buffer_task.write().await;
                prune_request_logs(&mut logs);
                logs.insert(0, entry.clone());
                prune_request_logs(&mut logs);
            }
            let _ = tx_task.send(
                serde_json::json!({
                    "type": "request_log",
                    "data": entry,
                })
                .to_string(),
            );
        }
    });

    let instance_manager = InstanceManager::with_geo_profile(geo_profile.clone())
        .with_instance_configs(instance_configs)?
        .with_allow_lan(allow_lan)
        .with_request_log_sender(request_log_tx)
        .with_storage(storage.clone());
    instance_manager
        .cleanup_orphaned_sessions()
        .await
        .context("failed to clean orphaned instance processes before serving API")?;
    let instance_manager = Arc::new(RwLock::new(instance_manager));

    let shutdown_manager = instance_manager.clone();
    let state = AppState {
        tx,
        instance_manager,
        storage,
        log_buffer,
        geo_profile,
        proxy_manager,
    };

    let router = Router::new()
        .route("/", get(serve_index))
        .route("/health", get(health_check))
        .route("/api/status", get(get_status))
        .route("/api/applications", get(get_applications))
        .route("/api/instances/config", get(get_instance_configs))
        .route("/api/proxy", get(get_proxy_config))
        .route("/api/proxy", post(save_proxy_config))
        .route("/api/proxy/test", post(test_proxy))
        .route("/api/proxy/restart", post(restart_proxy))
        .route("/api/instances/:kind/launch", post(launch_instance))
        .route("/api/instances/:kind/stop", post(stop_instance))
        .route("/api/instances/web/open-auth", post(open_web_auth))
        .route("/api/instances/claude-web/open-auth", post(open_web_auth))
        .route("/api/logs", get(get_logs).delete(clear_logs))
        .route("/api/validation/:kind", get(get_validation_history))
        .route("/api/security/web", get(get_web_security))
        .route("/api/security/claude-web", get(get_web_security))
        .route("/api/security/desktop", get(get_desktop_security))
        .route("/api/security/claude-desktop", get(get_desktop_security))
        .route("/ws", get(websocket_handler))
        .nest_service("/static", ServeDir::new("web"))
        .with_state(Arc::new(state));

    Ok((router, shutdown_manager))
}

include!("status.rs");
include!("proxy.rs");
include!("logs.rs");
include!("security.rs");
include!("instances.rs");
include!("websocket.rs");

#[cfg(test)]
mod security_tests {
    use super::{
        direct_endpoints, evidence_state, fail_closed_state, parse_checked_pids,
        parse_detail_count, web_check_matrix,
    };

    #[test]
    fn parses_network_validation_details() {
        let details =
            "checked 3 pids; proxied=2, local=1, direct=4321:203.0.113.10:443 (ESTABLISHED)";

        assert_eq!(parse_checked_pids(details), 3);
        assert_eq!(parse_detail_count(details, "proxied"), 2);
        assert_eq!(parse_detail_count(details, "local"), 1);
        assert_eq!(
            direct_endpoints(details),
            vec!["4321:203.0.113.10:443 (ESTABLISHED)".to_string()]
        );
    }

    #[test]
    fn empty_direct_count_has_no_endpoints() {
        let details = "checked 2 pids; proxied=4, local=1, direct=0";

        assert_eq!(parse_checked_pids(details), 2);
        assert_eq!(parse_detail_count(details, "direct"), 0);
        assert!(direct_endpoints(details).is_empty());
    }

    #[test]
    fn cage_style_check_states_never_promote_missing_evidence_to_pass() {
        assert_eq!(evidence_state(false, false), "missing");
        assert_eq!(evidence_state(true, false), "expired");
        assert_eq!(evidence_state(true, true), "fresh");
        assert_eq!(fail_closed_state(true, true, "allowed", true), "enforced");
        assert_eq!(fail_closed_state(false, true, "blocked", true), "enforced");
        assert_eq!(
            fail_closed_state(true, false, "allowed", true),
            "unavailable"
        );
        assert_eq!(fail_closed_state(true, true, "blocked", true), "mismatch");
        assert_eq!(
            fail_closed_state(true, true, "allowed", false),
            "unverified"
        );
        assert_eq!(
            fail_closed_state(false, true, "blocked", false),
            "unverified"
        );

        let checks = web_check_matrix(&super::WebCheckInput {
            proxy_healthy: true,
            geo_ready: true,
            firewall_mode: "allowed",
            firewall_installed: true,
            firewall_global_verified: true,
            validation_pass: false,
            direct_zero: true,
            evidence_fresh: false,
            web_running: true,
            has_timestamp: false,
            dns_status: "not_sampled",
            webrtc_status: "not_sampled",
            ipv6_status: "not_sampled",
        });
        assert_eq!(checks["process_connections"]["status"], "pending");
        assert_eq!(checks["evidence"]["status"], "missing");
        assert_eq!(checks["dns_udp53"]["status"], "not_sampled");
    }

    #[test]
    fn active_probe_states_are_exposed_without_inference() {
        let checks = web_check_matrix(&super::WebCheckInput {
            proxy_healthy: true,
            geo_ready: true,
            firewall_mode: "allowed",
            firewall_installed: true,
            firewall_global_verified: true,
            validation_pass: true,
            direct_zero: true,
            evidence_fresh: true,
            web_running: true,
            has_timestamp: true,
            dns_status: "pass",
            webrtc_status: "pass",
            ipv6_status: "pass",
        });
        assert_eq!(checks["dns_udp53"]["status"], "pass");
        assert_eq!(checks["webrtc_public_candidates"]["status"], "pass");
        assert_eq!(checks["ipv6_direct"]["status"], "pass");
    }

    fn probe_fixture() -> serde_json::Value {
        serde_json::json!({
            "root_pid": 42,
            "exit_ip": "203.0.113.7",
            "dns_udp53": {"sampled_at": "2026-11-17T02:00:00Z", "status": "pass", "evidence": "capture summary"},
            "webrtc_public_candidates": {"sampled_at": "2026-11-17T02:05:00Z", "status": "pass", "evidence": "ICE summary"},
            "ipv6_direct": {"sampled_at": "2026-11-17T02:08:00Z", "status": "pass", "evidence": "IPv6 probe summary"},
        })
    }

    fn probe_now(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn probe_ttl_uses_each_sample_time_not_summary_time() {
        let mut payload = probe_fixture();
        // A fresh import timestamp cannot refresh an earlier DNS sample.
        payload["timestamp"] = serde_json::json!("2026-11-17T02:11:00Z");
        let now = probe_now("2026-11-17T02:11:00Z");
        for (key, expected) in [
            ("dns_udp53", "expired"),
            ("webrtc_public_candidates", "pass"),
            ("ipv6_direct", "pass"),
        ] {
            assert_eq!(
                super::active_probe_status(Some(&payload), key, now, Some(42), Some("203.0.113.7")),
                expected
            );
        }
        assert_eq!(
            super::active_probe_status(
                Some(&payload),
                "dns_udp53",
                probe_now("2026-11-17T02:10:00Z"),
                Some(42),
                Some("203.0.113.7")
            ),
            "pass"
        );
    }

    #[test]
    fn probe_rejects_future_sample_including_subsecond_clock_skew() {
        let payload = probe_fixture();
        assert_eq!(
            super::active_probe_status(
                Some(&payload),
                "dns_udp53",
                probe_now("2026-11-17T01:59:59.999Z"),
                Some(42),
                Some("203.0.113.7")
            ),
            "not_sampled"
        );
    }

    #[test]
    fn probe_rejects_previous_instance_and_changed_or_missing_exit() {
        let payload = probe_fixture();
        let now = probe_now("2026-11-17T02:09:00Z");
        for (pid, exit_ip) in [
            (Some(43), Some("203.0.113.7")),
            (Some(42), Some("203.0.113.8")),
            (None, Some("203.0.113.7")),
            (Some(42), None),
            (Some(0), Some("203.0.113.7")),
        ] {
            assert_eq!(
                super::active_probe_status(Some(&payload), "dns_udp53", now, pid, exit_ip),
                "not_sampled"
            );
        }
    }

    #[test]
    fn probe_rejects_missing_malformed_and_legacy_fields() {
        let now = probe_now("2026-11-17T02:09:00Z");
        let status = |value: &serde_json::Value| {
            super::active_probe_status(Some(value), "dns_udp53", now, Some(42), Some("203.0.113.7"))
        };
        for field in ["root_pid", "exit_ip"] {
            let mut payload = probe_fixture();
            payload.as_object_mut().unwrap().remove(field);
            assert_eq!(status(&payload), "not_sampled");
        }
        for field in ["sampled_at", "status", "evidence"] {
            let mut payload = probe_fixture();
            payload["dns_udp53"].as_object_mut().unwrap().remove(field);
            assert_eq!(status(&payload), "not_sampled");
        }
        for timestamp in ["invalid", ""] {
            let mut payload = probe_fixture();
            payload["dns_udp53"]["sampled_at"] = serde_json::json!(timestamp);
            assert_eq!(status(&payload), "not_sampled");
        }
        let legacy = serde_json::json!({
            "timestamp": now.to_rfc3339(),
            "dns_udp53": {"status": "pass", "evidence": "old summary"},
        });
        assert_eq!(status(&legacy), "not_sampled");
        assert_eq!(
            super::active_probe_status(None, "dns_udp53", now, Some(42), Some("203.0.113.7")),
            "not_sampled"
        );
    }

    #[test]
    fn fresh_probe_failure_cannot_be_promoted_to_pass() {
        let mut payload = probe_fixture();
        payload["dns_udp53"]["status"] = serde_json::json!("fail");
        assert_eq!(
            super::active_probe_status(
                Some(&payload),
                "dns_udp53",
                probe_now("2026-11-17T02:09:00Z"),
                Some(42),
                Some("203.0.113.7")
            ),
            "fail"
        );
    }

    #[test]
    fn web_verdict_requires_matching_pf_and_all_fresh_probe_results() {
        let mut input = super::WebCheckInput {
            proxy_healthy: true,
            geo_ready: true,
            firewall_mode: "allowed",
            firewall_installed: true,
            firewall_global_verified: true,
            validation_pass: true,
            direct_zero: true,
            evidence_fresh: true,
            web_running: true,
            has_timestamp: true,
            dns_status: "pass",
            webrtc_status: "pass",
            ipv6_status: "pass",
        };
        let verdict = |input: &super::WebCheckInput<'_>| {
            super::web_security_verdict(input, false, false, true)
        };
        assert_eq!(verdict(&input), "PASS");
        input.firewall_global_verified = false;
        assert_eq!(verdict(&input), "UNKNOWN");
        assert_eq!(
            web_check_matrix(&input)["proxy_fail_closed"]["status"],
            "unverified"
        );
        assert_eq!(
            web_check_matrix(&input)["proxy_fail_closed"]["global_verified"],
            false
        );
        input.firewall_mode = "blocked";
        input.proxy_healthy = false;
        assert_eq!(verdict(&input), "UNKNOWN");
        input.firewall_mode = "allowed";
        input.proxy_healthy = true;
        input.firewall_global_verified = true;
        input.firewall_mode = "blocked";
        assert_eq!(verdict(&input), "FAIL");
        input.proxy_healthy = false;
        assert_eq!(verdict(&input), "BLOCKED");
        input.firewall_installed = false;
        assert_eq!(verdict(&input), "FAIL");
        input.firewall_installed = true;
        input.firewall_mode = "allowed";
        assert_eq!(verdict(&input), "FAIL");
        input.proxy_healthy = true;
        for status in ["expired", "not_sampled", "unknown", "not_applicable"] {
            input.dns_status = status;
            assert_eq!(verdict(&input), "UNKNOWN");
        }
        input.dns_status = "fail";
        assert_eq!(verdict(&input), "FAIL");
        input.dns_status = "pass";
        input.webrtc_status = "not_sampled";
        assert_eq!(verdict(&input), "UNKNOWN");
        input.webrtc_status = "pass";
        input.ipv6_status = "not_sampled";
        assert_eq!(verdict(&input), "UNKNOWN");
        // A real current-session STUN leak must remain FAIL even if the DNS
        // packet capture is still missing and the IPv6 sample has expired.
        input.dns_status = "not_sampled";
        input.webrtc_status = "fail";
        input.ipv6_status = "expired";
        assert_eq!(verdict(&input), "FAIL");
        assert_eq!(
            super::active_probe_overall_status([
                input.dns_status,
                input.webrtc_status,
                input.ipv6_status
            ]),
            "fail"
        );
    }

    #[test]
    fn firewall_gate_for_web_and_desktop_requires_global_evidence() {
        for healthy in [true, false] {
            for mode in ["allowed", "blocked", "unavailable"] {
                assert_eq!(
                    super::firewall_security_verdict(healthy, true, mode, false),
                    Some("UNKNOWN")
                );
            }
        }
        assert_eq!(
            super::firewall_security_verdict(true, true, "allowed", true),
            None
        );
        assert_eq!(
            super::firewall_security_verdict(false, true, "blocked", true),
            Some("BLOCKED")
        );
        assert_eq!(
            super::firewall_security_verdict(true, true, "blocked", true),
            Some("FAIL")
        );
        assert_eq!(
            super::firewall_security_verdict(false, true, "allowed", true),
            Some("FAIL")
        );
        assert_eq!(
            super::firewall_security_verdict(true, false, "allowed", true),
            Some("FAIL")
        );
    }
}
