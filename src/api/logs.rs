#[derive(Debug, Deserialize)]
struct LogQuery {
    limit: Option<usize>,
}

async fn get_logs(
    Query(query): Query<LogQuery>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let limit = query
        .limit
        .unwrap_or(MAX_REQUEST_LOGS)
        .min(MAX_REQUEST_LOGS);
    let mut logs = state.log_buffer.write().await;
    prune_request_logs(&mut logs);
    let entries = logs.iter().take(limit).cloned().collect::<Vec<_>>();
    Json(serde_json::json!({
        "logs": entries,
        "limit": limit,
        "retention_minutes": REQUEST_LOG_RETENTION.num_minutes(),
    }))
}

async fn clear_logs(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let cleared = {
        let mut logs = state.log_buffer.write().await;
        prune_request_logs(&mut logs);
        let count = logs.len();
        logs.clear();
        count
    };

    // Keep every open dashboard in sync with the server-side buffer.
    let _ = state.tx.send(
        serde_json::json!({
            "type": "logs_cleared",
            "count": cleared,
        })
        .to_string(),
    );

    Json(serde_json::json!({
        "success": true,
        "cleared": cleared,
    }))
}

async fn get_validation_history(
    axum::extract::Path(kind): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let allowed = matches!(kind.as_str(), "code" | "desktop" | "vscode" | "web");
    if !allowed {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("Unknown instance kind: {}", kind),
        }));
    }

    match state.storage.recent_validation_evidence(&kind, 50) {
        Ok(evidence) => Json(serde_json::json!({
            "success": true,
            "instance": kind,
            "evidence": evidence,
        })),
        Err(error) => Json(serde_json::json!({
            "success": false,
            "instance": kind,
            "error": error.to_string(),
        })),
    }
}
