use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{api, config, firewall, geoip, proxy, storage};

const MAX_DAEMON_LOG_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(unix)]
fn secure_directory(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect runtime directory {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("runtime path is not a directory: {}", path.display());
        }
    } else {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create runtime directory {}", path.display()))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure runtime directory {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create runtime directory {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("runtime file is not a regular file: {}", path.display());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure runtime file {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn harden_runtime_permissions() -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let root = std::path::PathBuf::from(home).join(".cim");
    secure_directory(&root)?;

    let logs = root.join("logs");
    secure_directory(&logs)?;
    for relative in [
        "bin",
        "instances",
        "instances/code",
        "instances/desktop",
        "instances/vscode",
        "instances/web",
        "instances/web/sessions",
        "sing-box",
    ] {
        let path = root.join(relative);
        if path.exists() {
            secure_directory(&path)?;
        }
    }

    for relative in [
        "config.toml",
        "cim.db",
        "sing-box/config.json",
        "codesign-identity",
        "logs/daemon.log",
    ] {
        secure_file(&root.join(relative))?;
    }

    let log = root.join("logs/daemon.log");
    if let Ok(metadata) = std::fs::metadata(&log) {
        if metadata.len() > MAX_DAEMON_LOG_BYTES {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&log)
                .with_context(|| format!("failed to open {} for rotation", log.display()))?;
            file.set_len(0)
                .with_context(|| format!("failed to rotate {}", log.display()))?;
            info!(path = %log.display(), "Rotated oversized CIM daemon log");
        }
    }
    Ok(())
}

pub(crate) async fn run_daemon() -> Result<()> {
    use std::path::PathBuf;

    info!("Starting daemon...");

    // Load configuration
    let config = config::Config::load()?;
    harden_runtime_permissions()?;
    std::env::set_var("CIM_ISOLATION_GROUP", &config.firewall.isolation_group);
    info!(
        "Configuration loaded from {:?}",
        config::Config::config_path()
    );

    // Initialize database
    let db_path = PathBuf::from(config::Config::expand_path(&config.daemon.database_path));
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let storage = open_storage(db_path.clone()).await?;
    info!("Database initialized at {:?}", db_path);

    // Initialize and start proxy manager
    let proxy_config = proxy::ProxyConfig {
        binary_path: PathBuf::from(config::Config::expand_path(&config.proxy.sing_box_binary)),
        config_path: PathBuf::from(config::Config::expand_path(&config.proxy.sing_box_config)),
        health_check_interval: config.proxy.health_check_interval,
        health_check_timeout: config.proxy.health_check_timeout,
        exit_ip_endpoints: config.proxy.exit_ip_endpoints.clone(),
    };

    let geo_profile = geoip::shared_profile();
    let firewall = std::sync::Arc::new(firewall::FirewallController::new(config.firewall.clone()));
    let proxy_manager = std::sync::Arc::new(proxy::ProxyManager::with_geo_profile_and_firewall(
        proxy_config,
        geo_profile.clone(),
        firewall,
    ));

    // Keep the PF anchor fail-closed while the daemon removes unmanaged
    // Claude/Web sessions. Do not expose an allowed PF state until cleanup
    // and the first proxy/GeoIP check have both completed.
    proxy_manager.mark_unhealthy();

    // Build the manager before starting the proxy. Router construction also
    // removes orphaned instance processes, so that cleanup happens while PF
    // is still blocked after a daemon restart.
    let (router, instance_manager) = api::create_router(
        storage,
        geo_profile,
        proxy_manager.clone(),
        config.verification.allow_lan,
        config.instances.clone(),
    )
    .await?;

    // Start sing-box
    if let Err(e) = proxy_manager.start().await {
        warn!("Failed to start proxy manager: {}", e);
        warn!("Continuing in soft-isolation mode");
    } else {
        info!("Proxy manager started successfully");

        // Resolve the first exit profile before the API starts serving launch
        // requests, so new instances inherit the correct locale immediately.
        match proxy_manager.refresh_geo_profile().await {
            Ok(_) => proxy_manager.mark_healthy(),
            Err(error) => {
                warn!("Initial GeoIP resolution failed: {}", error);
                // A working tunnel is still safe to expose as healthy. Web
                // launch is gated on a ready GeoProfile, while the health
                // loop keeps retrying GeoIP independently.
                match proxy_manager.check_exit_ip().await {
                    Ok(_) => proxy_manager.mark_healthy(),
                    Err(proxy_error) => {
                        proxy_manager.mark_unhealthy();
                        warn!("Initial proxy health check failed: {}", proxy_error);
                    }
                }
            }
        }
    }

    // Keep the health loop alive even when the first start failed. A later
    // API restart can then recover the proxy and immediately resume checks.
    let health_proxy_manager = proxy_manager.clone();
    let health_task = tokio::spawn(async move {
        health_proxy_manager.health_check_loop().await;
    });

    // A failed PF transition must not leave an already-running instance in a
    // stale allowed state. Poll the authoritative proxy/PF state and stop
    // managed instances unless PF has explicitly confirmed the blocked mode.
    let fail_closed_proxy = proxy_manager.clone();
    let fail_closed_instances = instance_manager.clone();
    let fail_closed_task = tokio::spawn(async move {
        let mut check = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            check.tick().await;
            if fail_closed_proxy.is_healthy() {
                continue;
            }
            let firewall = fail_closed_proxy.firewall_status();
            if firewall.mode == crate::firewall::FirewallMode::Blocked {
                continue;
            }
            let running = fail_closed_instances.read().await.any_running().await;
            if !running {
                continue;
            }
            warn!(
                mode = ?firewall.mode,
                "Proxy unhealthy without confirmed PF blocked state; stopping managed instances"
            );
            if let Err(error) = fail_closed_instances.write().await.stop_all().await {
                warn!(error = %error, "Failed to stop all instances during fail-closed recovery");
            }
        }
    });

    // One startup attempt per configured instance; never bypass launch gates.
    // Failures are logged, not retried indefinitely or converted into direct launches.
    for kind in crate::instance::InstanceKind::ALL {
        if !config.instances.get(kind).autostart {
            continue;
        }
        let firewall = proxy_manager.firewall_status();
        if !proxy_manager.is_healthy()
            || !firewall.installed
            || firewall.mode != crate::firewall::FirewallMode::Allowed
            || !crate::instance::isolation_ready()
        {
            warn!(
                instance = kind.definition().id,
                "Autostart skipped: proxy/PF/helper not ready"
            );
            continue;
        }
        if let Err(error) = instance_manager.read().await.launch(kind).await {
            warn!(instance = kind.definition().id, error = %error, "Instance autostart failed");
        }
    }

    // The daemon API has no account authentication and controls PF/instance
    // state, so never expose it on a LAN address. Keep the boundary explicit
    // even if a hand-edited config changes the default.
    validate_loopback_listen_addr(&config.daemon.listen_addr)?;

    // Start HTTP server
    let listener = tokio::net::TcpListener::bind(&config.daemon.listen_addr).await?;
    info!("Listening on http://{}", config.daemon.listen_addr);

    let server = axum::serve(listener, router);
    tokio::select! {
        result = server => result?,
        _ = shutdown_signal() => {
            info!("Shutdown signal received, stopping instances before proxy");
            // Stop the periodic health task before changing the firewall. It
            // can otherwise race shutdown and briefly restore the allowed
            // rules after the proxy has already started going away.
            health_task.abort();
            let _ = health_task.await;
            fail_closed_task.abort();
            let _ = fail_closed_task.await;
            proxy_manager.mark_unhealthy();
            if let Err(error) = instance_manager.write().await.stop_all().await {
                warn!(error = %error, "Some instances failed to stop during daemon shutdown");
            }
            proxy_manager.stop().await?;
        }
    }

    Ok(())
}

pub(crate) fn validate_loopback_listen_addr(value: &str) -> Result<()> {
    let address: std::net::SocketAddr = value
        .parse()
        .with_context(|| format!("daemon listen address must be a socket address: {value}"))?;
    if !address.ip().is_loopback() {
        anyhow::bail!("daemon API must listen on loopback; refusing unsafe address {value}");
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub(crate) async fn init_database() -> Result<()> {
    use std::path::PathBuf;

    let home = std::env::var("HOME")?;
    let db_path = PathBuf::from(home).join(".cim/cim.db");

    std::fs::create_dir_all(db_path.parent().unwrap())?;
    harden_runtime_permissions()?;

    let _storage = open_storage(db_path.clone()).await?;

    println!("✓ Database initialized at {:?}", db_path);

    Ok(())
}

pub(crate) async fn open_storage(
    db_path: std::path::PathBuf,
) -> Result<std::sync::Arc<storage::Storage>> {
    Ok(std::sync::Arc::new(
        tokio::task::spawn_blocking(move || storage::Storage::new(&db_path))
            .await
            .context("database initialization task failed")??,
    ))
}
