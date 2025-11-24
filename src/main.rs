use anyhow::Result;
use logging::{info, error};

mod logging;
mod routing;
mod cli;
mod config;
mod control_plane;
mod http_parser;
mod request_processor;
mod backend_pool;
mod health;
mod worker;
mod udp_worker;
mod http_filters;
mod tls;
mod data_plane;
mod ebpf;
mod kubernetes;
mod config_watcher;

use control_plane::ControlPlane;
use data_plane::DataPlane;
use cli::Args;
use clap::Parser;
use config::PortailConfig;
use tokio_util::sync::CancellationToken;

fn main() -> Result<()> {
    let args = Args::parse();

    if let Err(error) = args.validate() {
        eprintln!("Invalid arguments: {}", error);
        std::process::exit(1);
    }

    // Config generation needs no eBPF — use a minimal single-threaded runtime
    if args.is_generation_mode() {
        logging::init_logging(args.verbose, None);
        return handle_generation_mode(&args);
    }

    // Validate and example modes also use minimal runtime
    if args.is_validation_mode() || args.example_config {
        let portail_config = if let Some(config_path) = &args.config {
            PortailConfig::load_from_file(config_path)
                .map_err(|e| {
                    eprintln!("Failed to load configuration file: {}", e);
                    std::process::exit(1);
                }).unwrap()
        } else {
            PortailConfig::default()
        };
        logging::init_logging(args.verbose, Some(&portail_config.observability.logging));
        if args.is_validation_mode() {
            return handle_validation_mode(&args);
        }
        return handle_example_config_mode(&args);
    }

    // Validate eBPF requirements — fail fast
    if let Err(e) = ebpf::validate_system_requirements() {
        eprintln!("eBPF system requirements not met: {}", e);
        eprintln!("Portail uses single codepath architecture with no fallback behavior.");
        std::process::exit(1);
    }

    let portail_config = if let Some(config_path) = &args.config {
        PortailConfig::load_from_file(config_path)
            .map_err(|e| {
                eprintln!("Failed to load configuration file: {}", e);
                std::process::exit(1);
            }).unwrap()
    } else {
        PortailConfig::default()
    };

    logging::init_logging(args.verbose, Some(&portail_config.observability.logging));

    let worker_count = portail_config.gateway.worker_threads;

    // Build Tokio runtime with thread count matching the configured worker count
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_count)
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build Tokio runtime: {}", e))?;

    info!("Tokio runtime started with {} threads", worker_count);

    rt.block_on(async_main(args, portail_config, worker_count))
}

async fn async_main(args: Args, portail_config: PortailConfig, worker_count: usize) -> Result<()> {
    let performance_config = portail_config.performance.clone();
    let listeners = portail_config.gateway.listeners.clone();
    let cert_dir = args.cert_dir.clone().unwrap_or_else(|| std::path::PathBuf::from("certs"));

    info!("Configuration loaded: {} listeners, {} HTTP routes, {} TCP routes, {} UDP routes",
          portail_config.gateway.listeners.len(),
          portail_config.http_routes.len(),
          portail_config.tcp_routes.len(),
          portail_config.udp_routes.len());

    // Single Tokio runtime for both control and data planes
    let mut control_plane = ControlPlane::new(portail_config)?;
    control_plane.start().await?;

    let routes = control_plane.get_routes();

    // In Kubernetes mode, spawn the Gateway API controller
    let shutdown_token = CancellationToken::new();
    if args.kubernetes {
        let k8s_routes = routes.clone();
        let controller_name = args.controller_name.clone();
        let token = shutdown_token.clone();
        tokio::spawn(async move {
            if let Err(e) = kubernetes::controller::run_controller(k8s_routes, controller_name, token).await {
                error!("Kubernetes controller failed: {}", e);
            }
        });
        info!("Kubernetes Gateway API controller started");
    }

    let mut data_plane = DataPlane::new(worker_count, &listeners, &performance_config, &cert_dir)?;

    // Attach eBPF before starting workers
    let listener_fds = data_plane.get_listener_fds();
    info!("Attaching SO_REUSEPORT eBPF programs to {} worker sockets", listener_fds.len());
    control_plane.attach_worker_reuseport_programs(&listener_fds)?;
    info!("eBPF programs attached — starting worker tasks");

    data_plane.start(routes.clone());

    info!("Portail ready: {} workers × {} listeners", worker_count, listeners.len());

    // In standalone mode, watch the config file for SIGHUP-triggered reloads
    if !args.kubernetes {
        if let Some(config_path) = args.config.clone() {
            let reload_routes = routes;
            let reload_shutdown = shutdown_token.clone();
            tokio::spawn(async move {
                config_watcher::watch_config(config_path, reload_routes, reload_shutdown).await;
            });
        }
    }

    // Wait for shutdown signal
    control_plane.wait_for_shutdown().await?;
    shutdown_token.cancel();

    info!("Shutting down Portail");
    data_plane.shutdown().await;

    info!("Portail shutdown complete");
    Ok(())
}

/// Handle validation modes (--validate-only, --check-config)
fn handle_validation_mode(args: &Args) -> Result<()> {
    let config_path = args.config.as_ref().expect("Config path required for validation modes");

    if args.validate_only {
        info!("Validating configuration file: {:?}", config_path);

        match PortailConfig::load_from_file(config_path) {
            Ok(config) => {
                info!("Configuration file parsed successfully");

                if let Err(e) = config.validate() {
                    error!("Configuration validation failed: {}", e);
                    std::process::exit(1);
                }

                info!("Configuration validation passed");

                match config.to_route_table() {
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
                println!("{}", serde_json::to_string_pretty(&config).expect("Failed to serialize own config"));
                println!("---");

                println!();
                println!("Configuration Summary:");
                println!("  HTTP Routes: {}", config.http_routes.len());
                println!("  TCP Routes: {}", config.tcp_routes.len());
                println!("  Worker Threads: {}", config.gateway.worker_threads);
                println!("  HTTP Listeners: {:?}", config.gateway.listeners.iter().filter(|l| matches!(l.protocol, config::Protocol::HTTP | config::Protocol::HTTPS)).map(|l| l.port).collect::<Vec<_>>());

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
    let config_type = args.generate_config.as_ref().expect("generate_config should be Some");

    info!("Generating {} configuration", config_type);

    let config = match config_type.as_str() {
        "minimal" => PortailConfig::generate_minimal(),
        "development" => PortailConfig::generate_development(),
        _ => {
            error!("Invalid configuration type: {}", config_type);
            std::process::exit(1);
        }
    };

    let (output_content, format_name) = if let Some(output_path) = &args.output {
        let (output_content, _format_name) = match output_path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => {
                let content = config.to_json_pretty()
                    .map_err(|e| anyhow::anyhow!("Failed to serialize config to JSON: {}", e))?;
                (content, "JSON")
            }
            Some("yaml") | Some("yml") => {
                let content = config.to_yaml_pretty()
                    .map_err(|e| anyhow::anyhow!("Failed to serialize config to YAML: {}", e))?;
                (content, "YAML")
            }
            Some(ext) => {
                error!("Unsupported output file extension: .{}. Supported formats: .json, .yaml, .yml", ext);
                std::process::exit(1);
            }
            None => {
                error!("Output file must have an extension (.json, .yaml, .yml)");
                std::process::exit(1);
            }
        };

        std::fs::write(output_path, &output_content)
            .map_err(|e| anyhow::anyhow!("Failed to write configuration file '{}': {}", output_path.display(), e))?;

        info!("Generated {} configuration written to: {}", config_type, output_path.display());
        return Ok(());
    } else {
        let content = config.to_yaml_pretty()
            .map_err(|e| anyhow::anyhow!("Failed to serialize config to YAML: {}", e))?;
        (content, "YAML")
    };

    println!("# Generated {} configuration ({})", config_type, format_name);
    println!("# Use --output <file> to save to a file");
    println!();
    print!("{}", output_content);

    Ok(())
}
