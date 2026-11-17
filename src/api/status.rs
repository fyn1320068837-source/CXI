async fn serve_index() -> impl IntoResponse {
    let html = include_str!("../../web/index.html");
    Html(html)
}

async fn health_check() -> &'static str {
    "OK"
}

#[derive(Serialize)]
struct StatusResponse {
    proxy: ProxyStatus,
    instances: Vec<InstanceStatus>,
    firewall: FirewallStatus,
}

#[derive(Serialize)]
struct ProxyStatus {
    healthy: bool,
    geo_ready: bool,
    geo_status: String,
    exit_ip: Option<String>,
    latency_ms: Option<u64>,
    geo: Option<GeoInfo>,
}

#[derive(Serialize)]
struct GeoInfo {
    source_ip: Option<String>,
    country_code: Option<String>,
    country: String,
    city: Option<String>,
    timezone: Option<String>,
    language: Option<String>,
    locale: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

fn geo_info_from_profile(profile: &GeoProfile) -> GeoInfo {
    GeoInfo {
        source_ip: profile.source_ip.clone(),
        country_code: profile.country_code.clone(),
        country: profile.country.clone().unwrap_or_default(),
        city: profile.city.clone(),
        timezone: profile.timezone.clone(),
        language: profile.language.clone(),
        locale: profile.locale.clone(),
        latitude: profile.latitude,
        longitude: profile.longitude,
    }
}

#[derive(Serialize)]
struct InstanceStatus {
    network_scope: &'static str,
    runtime_id: &'static str,
    kind: String,
    status: String,
    pid: Option<u32>,
    validation: String,
    validation_details: String,
    validation_reason: Option<String>,
    validation_direct_endpoints: Vec<String>,
    validation_checked_at: Option<String>,
}

#[derive(Serialize)]
struct FirewallStatus {
    installed: bool,
    mode: String,
    global_enabled: bool,
    root_anchor_loaded: bool,
    global_verified: bool,
    status: String,
    anchor: String,
    isolation_group: String,
    last_error: Option<String>,
}

fn validation_name(status: &ValidationStatus) -> &'static str {
    match status {
        ValidationStatus::Pass => "pass",
        ValidationStatus::Fail => "fail",
        ValidationStatus::Unknown => "unknown",
        ValidationStatus::Unsupported => "unsupported",
    }
}

async fn get_status(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<StatusResponse> {
    let manager = state.instance_manager.read().await;
    let snapshot = manager.snapshot_all().await;
    let status_map = &snapshot.status;
    let pid_map = &snapshot.pids;
    let validation_map = &snapshot.validation;
    let geo_profile = state.geo_profile.read().await.clone();
    let firewall = state.proxy_manager.firewall_status();
    // Tunnel health and GeoIP readiness are separate signals. The proxy can
    // carry traffic while the GeoIP provider is temporarily unavailable; Web
    // launches are gated separately by InstanceManager until the profile is
    // ready, so a transient GeoIP outage never causes a host-locale fallback.
    let proxy_healthy = state.proxy_manager.is_healthy();
    let geo_ready = geo_profile.is_ready();
    let geo_status = if geo_ready {
        "ready"
    } else if geo_profile.source_ip.is_some() {
        "pending"
    } else {
        "unavailable"
    };

    let validation_for = |kind: InstanceKind| {
        validation_map
            .get(&kind)
            .map(|value| {
                (
                    validation_name(&value.status).to_string(),
                    value.details.clone(),
                    value.reason.clone(),
                    value.direct_endpoints.clone(),
                    value.checked_at.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    "unknown".to_string(),
                    "validation unavailable".to_string(),
                    Some("validation_unavailable".to_string()),
                    Vec::new(),
                    None,
                )
            })
    };
    let instances = crate::instance::APPLICATIONS.iter().map(|app| {
        let kind = app.kind;
        let (validation, validation_details, validation_reason, validation_direct_endpoints, validation_checked_at) = validation_for(kind);
        InstanceStatus {
            network_scope: if matches!(kind, InstanceKind::Web | InstanceKind::GeminiWeb) { "shared_browser_process_tree" } else { "instance_process_tree" },
            runtime_id: if matches!(kind, InstanceKind::Web | InstanceKind::GeminiWeb) { "managed-browser" } else { kind.as_str() },
            kind: kind.as_str().to_string(),
            status: if status_map.get(&kind).copied().unwrap_or(false) { "running" } else { "stopped" }.to_string(),
            pid: pid_map.get(&kind).copied().flatten(),
            validation, validation_details, validation_reason, validation_direct_endpoints, validation_checked_at,
        }
    }).collect();

    Json(StatusResponse {
        proxy: ProxyStatus {
            healthy: proxy_healthy,
            geo_ready,
            geo_status: geo_status.to_string(),
            exit_ip: geo_profile.source_ip.clone(),
            latency_ms: None,
            geo: if geo_profile.has_geo() {
                Some(geo_info_from_profile(&geo_profile))
            } else {
                None
            },
        },
        instances,
        firewall: FirewallStatus {
            installed: firewall.installed,
            mode: format!("{:?}", firewall.mode).to_lowercase(),
            global_enabled: firewall.global_enabled,
            root_anchor_loaded: firewall.root_anchor_loaded,
            global_verified: firewall.global_enabled && firewall.root_anchor_loaded,
            status: firewall.status,
            anchor: firewall.anchor,
            isolation_group: firewall.isolation_group,
            last_error: firewall.last_error,
        },
    })
}
