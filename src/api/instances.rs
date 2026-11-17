// Loaded snapshot; configuration changes take effect after daemon restart.
async fn get_instance_configs(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let manager = state.instance_manager.read().await;
    Json(serde_json::json!({"instances": manager.instance_configs(), "apply_mode": "daemon_restart"}))
}

// Static catalog only: registration does not imply installation or safety.
async fn get_applications() -> Json<&'static [crate::instance::ApplicationDefinition]> {
    Json(&crate::instance::APPLICATIONS)
}

#[cfg(test)]
mod registry_api_tests {
    #[tokio::test]
    async fn catalog_serves_canonical_ids_without_claiming_runtime_state() {
        use axum::{body::{to_bytes, Body}, http::{Request, StatusCode}, routing::get, Router};
        use tower::ServiceExt;

        let router = Router::new().route("/api/applications", get(super::get_applications));
        let response = router.oneshot(Request::builder().uri("/api/applications").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16_384).await.unwrap();
        let apps: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let apps = apps.as_array().unwrap();
        assert_eq!(apps.len(), 4);
        for app in apps {
            let kind = crate::instance::InstanceKind::parse(app["id"].as_str().unwrap()).unwrap();
            assert_eq!(app["legacy_alias"], kind.as_str());
            assert!(app.get("installed").is_none());
            assert!(app.get("running").is_none());
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenWebAuthRequest {
    url: String,
}

/// Open a Desktop OAuth URL in the already isolated CIM Web profile. This is
/// intentionally a narrow Web-only endpoint; the adapter allowlists HTTPS
/// authentication hosts and never accepts credentials or cookies.
async fn open_web_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(request): Json<OpenWebAuthRequest>,
) -> Json<serde_json::Value> {
    if !state.proxy_manager.is_healthy() {
        return Json(serde_json::json!({
            "success": false,
            "message": "代理当前不健康，已阻止 Desktop 登录跳转"
        }));
    }
    let firewall = state.proxy_manager.firewall_status();
    if !firewall.installed
        || firewall.mode != crate::firewall::FirewallMode::Allowed
        || !isolation_ready()
    {
        return Json(serde_json::json!({
            "success": false,
            "message": "PF fail-closed 或 CIM 隔离 helper 未就绪，已阻止 Desktop 登录跳转"
        }));
    }

    let manager = state.instance_manager.read().await;
    match manager.open_web_auth_url(&request.url).await {
        Ok(()) => Json(serde_json::json!({
            "success": true,
            "message": "authentication URL opened in CIM Web"
        })),
        Err(error) => Json(serde_json::json!({
            "success": false,
            "message": format!("failed to open authentication URL in CIM Web: {error}")
        })),
    }
}

async fn launch_instance(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let Some(instance_kind) = InstanceKind::parse(&kind) else {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("Unknown instance kind: {}", kind)
        }));
    };

    if !state.proxy_manager.is_healthy() {
        return Json(serde_json::json!({
            "success": false,
            "message": "代理当前不健康，已阻止实例启动"
        }));
    }
    let firewall = state.proxy_manager.firewall_status();
    if !firewall.installed {
        return Json(serde_json::json!({
            "success": false,
            "message": "PF fail-closed 未安装，已阻止实例启动"
        }));
    }
    if firewall.mode != crate::firewall::FirewallMode::Allowed {
        return Json(serde_json::json!({
            "success": false,
            "message": "PF 当前不是 allowed 状态，已阻止实例启动"
        }));
    }
    if !isolation_ready() {
        return Json(serde_json::json!({
            "success": false,
            "message": "cimgrp/root helper 未就绪，已阻止实例启动"
        }));
    }

    let manager = state.instance_manager.read().await;
    match manager.launch(instance_kind).await {
        Ok(_) => {
            // Broadcast update
            let _ = state.tx.send(format!("Instance {} launched", kind));

            Json(serde_json::json!({
                "success": true,
                "message": format!("{} instance launched successfully", kind)
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": format!("Failed to launch {}: {}", kind, e)
        })),
    }
}

async fn stop_instance(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let Some(instance_kind) = InstanceKind::parse(&kind) else {
        return Json(serde_json::json!({
            "success": false,
            "message": format!("Unknown instance kind: {}", kind)
        }));
    };

    let manager = state.instance_manager.read().await;
    match manager.stop(instance_kind).await {
        Ok(_) => {
            // Broadcast update
            let _ = state.tx.send(format!("Instance {} stopped", kind));

            Json(serde_json::json!({
                "success": true,
                "message": format!("{} instance stopped successfully", kind)
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": format!("Failed to stop {}: {}", kind, e)
        })),
    }
}
