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
mod worker;
mod udp_worker;
mod http_filters;
mod data_plane;
mod ebpf;

use control_plane::ControlPlane;
use data_plane::DataPlane;
use cli::Args;
use clap::Parser;
use config::UringRessConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Err(error) = args.validate() {
        eprintln!("Invalid arguments: {}", error);
        std::process::exit(1);
    }

    // Config generation needs no eBPF
    if args.is_generation_mode() {
        logging::init_logging(args.verbose, None);
        return handle_generation_mode(&args);
    }

    // Validate eBPF requirements — fail fast
    if let Err(e) = ebpf::validate_system_requirements() {
        eprintln!("eBPF system requirements not met: {}", e);
        eprintln!("UringRess uses single codepath architecture with no fallback behavior.");
        std::process::exit(1);
    }

    let uringress_config = if let Some(config_path) = &args.config {
        UringRessConfig::load_from_file(config_path)
            .map_err(|e| {
                eprintln!("Failed to load configuration file: {}", e);
                std::process::exit(1);
            }).unwrap()
    } else {
        UringRessConfig::default()
    };

    logging::init_logging(args.verbose, Some(&uringress_config.observability.logging));

    let listener_protocols: Vec<(u16, config::Protocol)> = uringress_config.gateway.listeners.iter()
        .map(|l| (l.port, l.protocol.clone()))
        .collect();
    let performance_config = uringress_config.performance.clone();
    let worker_count = uringress_config.gateway.worker_threads;

    info!("Configuration loaded: {} listeners, {} HTTP routes, {} TCP routes, {} UDP routes",
          uringress_config.gateway.listeners.len(),
          uringress_config.http_routes.len(),
          uringress_config.tcp_routes.len(),
          uringress_config.udp_routes.len());

    if args.is_validation_mode() {
        return handle_validation_mode(&args);
    }

    if args.example_config {
        return handle_example_config_mode(&args);
    }

    // Single Tokio runtime for both control and data planes
    let mut control_plane = ControlPlane::new(uringress_config)?;
    control_plane.start().await?;

    let routes = control_plane.get_routes();

    let mut data_plane = DataPlane::new(worker_count, &listener_protocols, &performance_config)?;

    // Attach eBPF before starting workers
    let listener_fds = data_plane.get_listener_fds();
    info!("Attaching SO_REUSEPORT eBPF programs to {} worker sockets", listener_fds.len());
    control_plane.attach_worker_reuseport_programs(&listener_fds)?;
    info!("eBPF programs attached — starting worker tasks");

    data_plane.start(routes);

    info!("UringRess ready: {} workers × {} listeners", worker_count, listener_protocols.len());

    // Wait for shutdown signal
    control_plane.wait_for_shutdown().await?;

    info!("Shutting down UringRess");
    data_plane.shutdown().await;

    info!("UringRess shutdown complete");
    Ok(())
}

/// Handle validation modes (--validate-only, --check-config)
fn handle_validation_mode(args: &Args) -> Result<()> {
    let config_path = args.config.as_ref().expect("Config path required for validation modes");

    if args.validate_only {
        info!("Validating configuration file: {:?}", config_path);

        match UringRessConfig::load_from_file(config_path) {
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

        match UringRessConfig::load_from_file(config_path) {
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
        "minimal" => UringRessConfig::generate_minimal(),
        "development" => UringRessConfig::generate_development(),
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
