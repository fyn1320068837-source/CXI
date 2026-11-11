mod api;
mod proxy;
mod instance;
mod verifier;
mod geoip;
mod storage;
mod config;

use anyhow::Result;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    info!("Starting CIM v{}", env!("CARGO_PKG_VERSION"));

    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("daemon") => run_daemon().await?,
        Some("init-db") => init_database().await?,
        Some("install-firewall") => install_firewall().await?,
        Some("uninstall") => uninstall().await?,
        Some("stop-all") => stop_all_instances().await?,
        Some("status") => show_status().await?,
        _ => {
            println!("CIM - Claude Instance Manager v{}", env!("CARGO_PKG_VERSION"));
            println!();
            println!("用法:");
            println!("  cim daemon              启动守护进程");
            println!("  cim init-db             初始化数据库");
            println!("  cim install-firewall    安装 PF 防火墙规则");
            println!("  cim uninstall           卸载 CIM");
            println!("  cim stop-all            停止所有实例");
            println!("  cim status              显示状态");
            println!();
            println!("控制台: http://localhost:8080");
        }
    }

    Ok(())
}

async fn run_daemon() -> Result<()> {
    use std::path::PathBuf;

    info!("Starting daemon...");

    // Load configuration
    let config = config::Config::load()?;
    info!("Configuration loaded from {:?}", config::Config::config_path());

    // Initialize database
    let db_path = PathBuf::from(config::Config::expand_path(&config.daemon.database_path));
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let storage = std::sync::Arc::new(storage::Storage::new(&db_path)?);
    info!("Database initialized at {:?}", db_path);

    // Initialize and start proxy manager
    let proxy_config = proxy::ProxyConfig {
        binary_path: PathBuf::from(config::Config::expand_path(&config.proxy.sing_box_binary)),
        config_path: PathBuf::from(config::Config::expand_path(&config.proxy.sing_box_config)),
        health_check_interval: config.proxy.health_check_interval,
        health_check_timeout: config.proxy.health_check_timeout,
        exit_ip_endpoints: config.proxy.exit_ip_endpoints.clone(),
    };

    let mut proxy_manager = proxy::ProxyManager::new(proxy_config.clone());

    // Start sing-box
    if let Err(e) = proxy_manager.start().await {
        warn!("Failed to start proxy manager: {}", e);
        warn!("Continuing in soft-isolation mode");
    } else {
        info!("Proxy manager started successfully");

        // Spawn health check loop in background
        tokio::spawn(async move {
            let mut pm = proxy::ProxyManager::new(proxy_config);
            if let Err(e) = pm.health_check_loop().await {
                error!("Health check loop error: {}", e);
            }
        });
    }

    // Create API router with storage
    let router = api::create_router(storage).await;

    // Start HTTP server
    let listener = tokio::net::TcpListener::bind(&config.daemon.listen_addr).await?;
    info!("Listening on http://{}", config.daemon.listen_addr);

    axum::serve(listener, router).await?;

    Ok(())
}

async fn init_database() -> Result<()> {
    use std::path::PathBuf;

    let home = std::env::var("HOME")?;
    let db_path = PathBuf::from(home).join(".cim/cim.db");

    std::fs::create_dir_all(db_path.parent().unwrap())?;

    let _storage = storage::Storage::new(&db_path)?;

    println!("✓ Database initialized at {:?}", db_path);

    Ok(())
}

async fn install_firewall() -> Result<()> {
    println!("Installing PF firewall rules...");
    println!("This requires sudo privileges.");
    println!();
    println!("Not yet implemented - see docs/firewall-setup.md");

    Ok(())
}

async fn uninstall() -> Result<()> {
    println!("Uninstalling CIM...");
    println!("Run the uninstall.sh script instead.");

    Ok(())
}

async fn stop_all_instances() -> Result<()> {
    println!("Stopping all instances...");
    println!("Not yet implemented");

    Ok(())
}

async fn show_status() -> Result<()> {
    println!("CIM Status");
    println!("Not yet implemented - open http://localhost:8080");

    Ok(())
}
