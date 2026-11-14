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
    let status_map = manager.status_all().await;
    let pid_map = manager.pid_all().await;
    let validation_map = manager.validation_all().await;
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
    let (
        code_validation,
        code_validation_details,
        code_validation_reason,
        code_direct_endpoints,
        code_checked_at,
    ) = validation_for(InstanceKind::Code);
    let (
        desktop_validation,
        desktop_validation_details,
        desktop_validation_reason,
        desktop_direct_endpoints,
        desktop_checked_at,
    ) = validation_for(InstanceKind::Desktop);
    let (
        vscode_validation,
        vscode_validation_details,
        vscode_validation_reason,
        vscode_direct_endpoints,
        vscode_checked_at,
    ) = validation_for(InstanceKind::VsCode);
    let (
        web_validation,
        web_validation_details,
        web_validation_reason,
        web_direct_endpoints,
        web_checked_at,
    ) = validation_for(InstanceKind::Web);

    let instances = vec![
        InstanceStatus {
            kind: "code".to_string(),
            status: if status_map
                .get(&InstanceKind::Code)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Code).copied().flatten(),
            validation: code_validation,
            validation_details: code_validation_details,
            validation_reason: code_validation_reason,
            validation_direct_endpoints: code_direct_endpoints,
            validation_checked_at: code_checked_at,
        },
        InstanceStatus {
            kind: "desktop".to_string(),
            status: if status_map
                .get(&InstanceKind::Desktop)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Desktop).copied().flatten(),
            validation: desktop_validation,
            validation_details: desktop_validation_details,
            validation_reason: desktop_validation_reason,
            validation_direct_endpoints: desktop_direct_endpoints,
            validation_checked_at: desktop_checked_at,
        },
        InstanceStatus {
            kind: "vscode".to_string(),
            status: if status_map
                .get(&InstanceKind::VsCode)
                .copied()
                .unwrap_or(false)
            {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::VsCode).copied().flatten(),
            validation: vscode_validation,
            validation_details: vscode_validation_details,
            validation_reason: vscode_validation_reason,
            validation_direct_endpoints: vscode_direct_endpoints,
            validation_checked_at: vscode_checked_at,
        },
        InstanceStatus {
            kind: "web".to_string(),
            status: if status_map.get(&InstanceKind::Web).copied().unwrap_or(false) {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            pid: pid_map.get(&InstanceKind::Web).copied().flatten(),
            validation: web_validation,
            validation_details: web_validation_details,
            validation_reason: web_validation_reason,
            validation_direct_endpoints: web_direct_endpoints,
            validation_checked_at: web_checked_at,
        },
    ];

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
            status: firewall.status,
            anchor: firewall.anchor,
            isolation_group: firewall.isolation_group,
            last_error: firewall.last_error,
        },
    })
}
