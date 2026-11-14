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
