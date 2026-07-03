use anyhow::Result;
use portail::logging::{error, info};
use portail::*;

mod cli;

use arc_swap::ArcSwap;
use clap::Parser;
use cli::Args;
use config::PortailConfig;
use proxy::data_plane::DataPlane;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn main() -> Result<()> {
    // Install rustls CryptoProvider early — needed by kube client in K8s mode
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let args = Args::parse();

    if let Err(error) = args.validate() {
        eprintln!("Invalid arguments: {}", error);
        std::process::exit(1);
    }

    // Print supported features and exit
    if args.supported_features {
        println!("{}", kubernetes::features::SUPPORTED_FEATURES.join(","));
        return Ok(());
    }

    // Config generation uses minimal runtime
    if args.is_generation_mode() {
        logging::init_logging(args.verbose, None);
        return handle_generation_mode(&args);
    }

    // Validate and example modes use minimal runtime
    if args.is_validation_mode() || args.example_config {
        let portail_config = if let Some(config_path) = &args.config {
            PortailConfig::load_from_file(config_path)
                .map_err(|e| {
                    eprintln!("Failed to load configuration file: {}", e);
                    std::process::exit(1);
                })
                .unwrap()
        } else {
            PortailConfig::default()
        };
        logging::init_logging(args.verbose, Some(&portail_config.observability.logging));
        if args.is_validation_mode() {
            return handle_validation_mode(&args);
        }
        return handle_example_config_mode(&args);
    }

    let portail_config = if let Some(config_path) = &args.config {
        PortailConfig::load_from_file(config_path)
            .map_err(|e| {
                eprintln!("Failed to load configuration file: {}", e);
                std::process::exit(1);
            })
            .unwrap()
    } else {
        PortailConfig::default()
    };

    logging::init_logging(args.verbose, Some(&portail_config.observability.logging));

    // Size the Tokio runtime to the POD, not the NODE. The default multi-thread
    // runtime eagerly spawns one worker per `available_parallelism()` (the node's
    // CPU count), but each operator-provisioned data plane serves a single
    // Gateway. Packing many per-Gateway pods onto one node would otherwise spawn
    // (pods × node_cpus) worker threads and exhaust the node's PID/thread budget
    // long before its CPU or memory — a hard ceiling on Gateway density. Cap to a
    // small default; high-traffic gateways or standalone use scale up via
    // PORTAIL_WORKER_THREADS. (The operator should also set CPU/mem requests+limits
    // and can stamp PORTAIL_WORKER_THREADS to match the pod's CPU allocation.)
    let worker_threads = std::env::var("PORTAIL_WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(2)
        });
    let max_blocking_threads = std::env::var("PORTAIL_MAX_BLOCKING_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(32);
    info!(
        "Tokio runtime: {} worker thread(s), {} max blocking thread(s)",
        worker_threads, max_blocking_threads
    );

    // Raise CAP_NET_BIND_SERVICE to effective *before* the runtime spawns its
    // worker threads, so they inherit it and direct privileged binds (80/443 in
    // multi-network/UDN and host-network modes, where no Service remaps the port)
    // succeed under restricted Pod Security. A no-op in service-mode. See
    // src/privileges.rs.
    privileges::raise_net_bind_service();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(max_blocking_threads)
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build Tokio runtime: {}", e))?;

    rt.block_on(async_main(args, portail_config))
}

async fn async_main(args: Args, portail_config: PortailConfig) -> Result<()> {
    let performance_config = portail_config.performance.clone();
    // Kubernetes mode binds nothing up front: every listener comes from a
    // reconciled Gateway. The default config's example :8080 listener is a
    // standalone-only convenience — binding it here made k8s-mode startup
    // fail outright whenever anything else on the host held that port.
    let listeners = if args.kubernetes {
        Vec::new()
    } else {
        portail_config.gateway.listeners.clone()
    };
    let cert_dir = args
        .cert_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("certs"));

    info!(
        "Configuration loaded: {} listeners, {} HTTP routes, {} TCP routes, {} UDP routes",
        portail_config.gateway.listeners.len(),
        portail_config.http_routes.len(),
        portail_config.tcp_routes.len(),
        portail_config.udp_routes.len()
    );

    // Build initial route table from configuration. Strict: in standalone
    // mode a backend that fails DNS resolution should fail startup loudly,
    // not silently serve 5xx. (Kubernetes mode starts from an empty default
    // config, so strictness costs nothing there.)
    let route_table = portail_config
        .to_route_table_strict()
        .map_err(|e| anyhow::anyhow!("Failed to convert configuration to route table: {}", e))?;
    let routes = Arc::new(ArcSwap::from_pointee(route_table));
    info!(
        "Routes loaded: {} HTTP routes, {} TCP routes",
        portail_config.http_routes.len(),
        portail_config.tcp_routes.len()
    );

    let data_plane = std::sync::Arc::new(std::sync::Mutex::new(DataPlane::new(
        &listeners,
        &performance_config,
        &cert_dir,
    )?));

    // Start initial listeners from config file
    data_plane.lock().unwrap().start(routes.clone());
    info!("Portail ready: {} listeners", listeners.len());

    // In Kubernetes mode, spawn the Gateway API controller with DataPlane access
    let shutdown_token = CancellationToken::new();
    if args.kubernetes {
        let k8s_routes = routes.clone();
        let controller_name = args.controller_name.clone();
        let token = shutdown_token.clone();
        let k8s_data_plane = data_plane.clone();
        let k8s_perf = performance_config.clone();
        let manage_gateway_status = args.manage_gateway_status;
        let gateway_scope = args
            .gateway_scope()
            .map_err(|e| anyhow::anyhow!("--gateway: {}", e))?;
        let watch_shape = kubernetes::controller::WatchShape::parse(args.watch_shape.as_deref());

        // Readiness flag — flipped true by the reconciler once the data plane
        // has bound its listener ports. Served on the readiness endpoint so the
        // operator's readinessProbe gates LB traffic and the Programmed status.
        let ready_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(admin::serve(
            args.readiness_port,
            ready_flag.clone(),
            shutdown_token.clone(),
        ));

        tokio::spawn(async move {
            if let Err(e) = kubernetes::controller::run_controller(
                k8s_routes,
                controller_name,
                token,
                k8s_data_plane,
                k8s_perf,
                manage_gateway_status,
                ready_flag,
                gateway_scope,
                watch_shape,
            )
            .await
            {
                error!("Kubernetes controller failed: {}", e);
            }
        });
        info!("Kubernetes Gateway API controller started");
    }

    // In standalone mode, the admin endpoint (/metrics + /readyz) is opt-in —
    // there is no readinessProbe to feed, but benches and dashboards may want
    // the scrape. Listeners are already up at this point, so ready = true.
    if !args.kubernetes {
        if let Some(metrics_port) = args.metrics_port {
            let ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
            tokio::spawn(admin::serve(metrics_port, ready, shutdown_token.clone()));
        }
    }

    // In standalone mode, watch the config file for SIGHUP-triggered reloads
    if !args.kubernetes {
        if let Some(config_path) = args.config.clone() {
            let reload_routes = routes;
            let reload_health = data_plane.lock().unwrap().health().clone();
            let reload_shutdown = shutdown_token.clone();
            tokio::spawn(async move {
                config_watcher::watch_config(
                    config_path,
                    reload_routes,
                    reload_health,
                    reload_shutdown,
                )
                .await;
            });
        }
    }

    // Wait for shutdown: SIGINT (^C) or SIGTERM. Handling SIGTERM is not
    // optional here — portail runs as PID 1 in its container, and the kernel
    // applies NO default action to signals PID 1 has no handler for. With
    // only the ctrl_c (SIGINT) handler, kubelet's graceful stop was silently
    // ignored: every pod sat out its full terminationGracePeriod and died by
    // SIGKILL, holding its (host-network) ports for those 30s and stalling
    // every rollout behind EADDRINUSE retries.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    info!("Received shutdown signal");
    shutdown_token.cancel();

    info!("Shutting down Portail");
    if let Ok(dp) = std::sync::Arc::try_unwrap(data_plane) {
        dp.into_inner().unwrap().shutdown().await;
    }

    info!("Portail shutdown complete");
    Ok(())
}

/// Handle validation modes (--validate-only, --check-config)
fn handle_validation_mode(args: &Args) -> Result<()> {
    let config_path = args
        .config
        .as_ref()
        .expect("Config path required for validation modes");

    if args.validate_only {
        info!("Validating configuration file: {:?}", config_path);

        match PortailConfig::load_from_file(config_path) {
            // load_from_file parses AND validates; a separate validate()
            // call here would be redundant.
            Ok(config) => {
                info!("Configuration file parsed and validated successfully");

                match config.to_route_table_strict() {
                    Ok(_) => info!("Route table conversion successful"),
                    Err(e) => {
                        error!("Route table conversion failed: {}", e);
                        std::process::exit(1);
                    }
                }

                info!("All validation checks passed");
            }
            Err(e) => {
                error!("Failed to parse configuration file: {}", e);
                std::process::exit(1);
            }
        }
    } else if args.check_config {
        info!("Checking configuration file: {:?}", config_path);

        match PortailConfig::load_from_file(config_path) {
            Ok(config) => {
                println!("Parsed configuration:");
                println!("---");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config).expect("Failed to serialize own config")
                );
                println!("---");

                println!();
                println!("Configuration Summary:");
                println!("  HTTP Routes: {}", config.http_routes.len());
                println!("  TCP Routes: {}", config.tcp_routes.len());

                println!(
                    "  HTTP Listeners: {:?}",
                    config
                        .gateway
                        .listeners
                        .iter()
                        .filter(|l| matches!(
                            l.protocol,
                            config::Protocol::HTTP | config::Protocol::HTTPS
                        ))
                        .map(|l| l.port)
                        .collect::<Vec<_>>()
                );

                info!("Configuration check completed");
            }
            Err(e) => {
                error!("Failed to load configuration file: {}", e);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Handle example config mode (--example-config)
fn handle_example_config_mode(_args: &Args) -> Result<()> {
    println!("Available example configurations:");
    println!();

    config::print_example_config_info();

    println!();
    println!("To use an example configuration:");
    println!("  1. Copy an example file to your desired location");
    println!("  2. Edit the configuration to match your environment");
    println!("  3. Run: cargo run -- --config path/to/config.yaml");

    println!();
    println!("To generate example configurations:");
    println!("  cargo run -- --generate-config minimal --output minimal.yaml");
    println!("  cargo run -- --generate-config development --output development.json");

    Ok(())
}

/// Handle config generation mode (--generate-config)
fn handle_generation_mode(args: &Args) -> Result<()> {
    let config_type = args
        .generate_config
        .as_ref()
        .expect("generate_config should be Some");

    info!("Generating {} configuration", config_type);

    // Examples are static YAML committed under examples/standalone/ and
    // embedded at compile time — --generate-config emits them verbatim.
    let yaml = match config::examples::example_yaml(config_type) {
        Some(yaml) => yaml,
        None => {
            error!("Invalid configuration type: {}", config_type);
            std::process::exit(1);
        }
    };

    if let Some(output_path) = &args.output {
        let output_content = match output_path.extension().and_then(|ext| ext.to_str()) {
            // JSON output re-parses the embedded YAML through the schema —
            // which also validates the committed example at point of use.
            Some("json") => serde_yaml::from_str::<PortailConfig>(yaml)
                .map_err(|e| anyhow::anyhow!("Embedded example failed to parse: {}", e))?
                .to_json_pretty()?,
            Some("yaml") | Some("yml") => yaml.to_string(),
            Some(ext) => {
                error!(
                    "Unsupported output file extension: .{}. Supported formats: .json, .yaml, .yml",
                    ext
                );
                std::process::exit(1);
            }
            None => {
                error!("Output file must have an extension (.json, .yaml, .yml)");
                std::process::exit(1);
            }
        };

        std::fs::write(output_path, &output_content).map_err(|e| {
            anyhow::anyhow!(
                "Failed to write configuration file '{}': {}",
                output_path.display(),
                e
            )
        })?;

        info!(
            "Generated {} configuration written to: {}",
            config_type,
            output_path.display()
        );
        return Ok(());
    }

    println!("# Generated {} configuration (YAML)", config_type);
    println!("# Use --output <file> to save to a file");
    println!();
    print!("{}", yaml);

    Ok(())
}
