use anyhow::Result;
use tracing::{info, error};

mod routing;
mod parser;
mod controller;
mod runtime_config;
mod cli;
mod config;
mod control_plane;
mod uring_worker;

use control_plane::ControlPlane;
use uring_worker::UringDataPlane;
use runtime_config::RuntimeConfig;
use cli::Args;
use config::UringRessConfig;

fn main() -> Result<()> {
    // Parse CLI arguments once at startup - startup overhead only
    let args = Args::parse_args();
    
    // Validate CLI arguments (before logging init)
    if let Err(error) = args.validate() {
        eprintln!("Invalid arguments: {}", error);
        std::process::exit(1);
    }
    
    // Load configuration first to determine logging settings
    let (runtime_config, use_file_routes) = if let Some(config_path) = &args.config {
        // Load configuration file using zero-overhead parsing
        let uringress_config = UringRessConfig::load_from_file(config_path)
            .map_err(|e| {
                eprintln!("Failed to load configuration file: {}", e);
                std::process::exit(1);
            }).unwrap();
        
        // Initialize logging with config file settings
        init_logging_from_config(&uringress_config.observability.logging, args.verbose);
        
        info!("Loading configuration from file: {:?}", config_path);
        
        // Convert to runtime config - zero overhead after startup
        let runtime_config = uringress_config.performance.to_runtime_config(&uringress_config.gateway);
        info!("Configuration loaded successfully from file");
        
        (runtime_config, Some(uringress_config))
    } else {
        // Initialize basic logging for environment/CLI-only mode
        init_logging(args.verbose);
        
        info!("No configuration file provided - starting with empty routes and environment variables");
        
        // Use environment variable parsing for runtime config, but no routes
        let runtime_config = load_runtime_config_from_env();
        (runtime_config, None)
    };
    
    // Handle validation modes that exit early (after logging is initialized)
    if args.is_validation_mode() {
        return handle_validation_mode(&args);
    }
    
    // Handle example config mode (after logging is initialized)
    if args.example_config {
        return handle_example_config_mode(&args);
    }
    
    // Handle config generation mode (after logging is initialized)
    if args.is_generation_mode() {
        return handle_generation_mode(&args);
    }
    
    // Extract to compile-time equivalent constants - NO function calls
    let num_workers: usize = if let Some(ref config) = use_file_routes {
        config.gateway.worker_threads
    } else {
        args.workers.unwrap_or_else(|| {
            std::env::var("WORKER_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4)
        })
    };
    
    // Verbose level already used for early logging initialization
    
    
    // Runtime config already loaded above from file or environment
    // Logging already initialized early in main()
    
    // Create hybrid runtime architecture: Tokio control plane + raw io_uring data plane
    info!("Starting UringRess with hybrid runtime architecture");
    info!("Control Plane: Tokio runtime for kube-rs compatibility");
    info!("Data Plane: Raw io_uring for maximum performance");
    
    // Start control plane in dedicated Tokio runtime
    let control_plane_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1) // Control plane needs minimal threads
            .thread_name("control-plane")
            .enable_all()
            .build()?;
        
        rt.block_on(async {
            info!("Starting Control Plane with Tokio runtime");
            
            // Create control plane
            let control_plane = if let Some(config) = use_file_routes {
                ControlPlane::new_with_config(config)?
            } else {
                ControlPlane::new()?
            };
            
            // Start control plane
            control_plane.start().await?;
            
            // Get routing table reference for data plane
            let routing_table = control_plane.config_manager.get_routes();
            
            // Start data plane in separate threads with raw io_uring
            let mut data_plane = UringDataPlane::new(num_workers);
            data_plane.start(routing_table, runtime_config)?;
            
            info!("UringRess hybrid architecture ready");
            info!("Control Plane: Tokio runtime (1 thread)");
            info!("Data Plane: Raw io_uring ({} workers)", num_workers);
            
            // Wait for shutdown signal
            control_plane.wait_for_shutdown().await?;
            
            info!("Shutting down UringRess");
            data_plane.shutdown()?;
            
            Result::<()>::Ok(())
        })
    });
    
    // Wait for control plane to complete
    match control_plane_handle.join() {
        Ok(result) => result?,
        Err(e) => {
            error!("Control plane thread panicked: {:?}", e);
            return Err(anyhow::anyhow!("Control plane thread panicked"));
        }
    }
    
    info!("UringRess shutdown complete");
    Ok(())
}

/// Handle validation modes (--validate-only, --check-config)
fn handle_validation_mode(args: &Args) -> Result<()> {
    let config_path = args.config.as_ref().expect("Config path required for validation modes");
    
    if args.validate_only {
        info!("Validating configuration file: {:?}", config_path);
        
        if !config_path.exists() {
            error!("Configuration file not found: {:?}", config_path);
            std::process::exit(1);
        }
        
        // Use actual configuration loading for validation
        match UringRessConfig::load_from_file(config_path) {
            Ok(config) => {
                info!("✓ Configuration file parsed successfully");
                
                // Validate configuration structure
                if let Err(e) = config.validate() {
                    error!("✗ Configuration validation failed: {}", e);
                    std::process::exit(1);
                }
                
                info!("✓ Configuration validation passed");
                
                // Test route table conversion
                match config.to_route_table() {
                    Ok(_) => info!("✓ Route table conversion successful"),
                    Err(e) => {
                        error!("✗ Route table conversion failed: {}", e);
                        std::process::exit(1);
                    }
                }
                
                info!("✓ All validation checks passed");
            }
            Err(e) => {
                error!("✗ Failed to parse configuration file: {}", e);
                std::process::exit(1);
            }
        }
    } else if args.check_config {
        info!("Checking configuration file: {:?}", config_path);
        
        if !config_path.exists() {
            error!("Configuration file not found: {:?}", config_path);
            std::process::exit(1);
        }
        
        // Load and display parsed configuration
        match UringRessConfig::load_from_file(config_path) {
            Ok(config) => {
                println!("Parsed configuration:");
                println!("---");
                
                // Pretty-print the configuration as JSON for readability
                match serde_json::to_string_pretty(&config) {
                    Ok(json) => println!("{}", json),
                    Err(_) => {
                        // Fallback to debug format if JSON serialization fails
                        println!("{:#?}", config);
                    }
                }
                
                println!("---");
                
                // Show summary statistics
                println!();
                println!("Configuration Summary:");
                println!("  HTTP Routes: {}", config.http_routes.len());
                println!("  TCP Routes: {}", config.tcp_routes.len());
                println!("  Worker Threads: {}", config.gateway.worker_threads);
                println!("  HTTP Listeners: {:?}", config.gateway.listeners.iter().filter(|l| matches!(l.protocol, config::Protocol::HTTP | config::Protocol::HTTPS)).map(|l| l.port).collect::<Vec<_>>());
                
                println!("  API Port: {}", config.gateway.api_port);
                
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

/// Initialize logging from config file settings with CLI verbose override
fn init_logging_from_config(logging_config: &config::LoggingConfig, verbose_level: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    use config::{LogLevel, LogFormat, LogOutput};
    
    // CLI verbose flag overrides config file level
    let level_str = if verbose_level > 0 {
        match verbose_level {
            1 => "debug",
            _ => "trace",
        }
    } else {
        // Use config file level
        match logging_config.level {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn", 
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    };
    
    // Allow RUST_LOG to override both config and CLI
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level_str));
    
    // Configure formatter based on config file format and output
    match logging_config.format {
        LogFormat::Json => {
            // For JSON format, use compact with structured output
            let mut subscriber = fmt()
                .with_env_filter(filter)
                .with_target(true)
                .compact();
            
            // Add file/line info for verbose modes or debug level
            if verbose_level > 1 || matches!(logging_config.level, LogLevel::Debug | LogLevel::Trace) {
                subscriber = subscriber.with_file(true).with_line_number(true);
            }
            
            match &logging_config.output {
                LogOutput::Stdout => subscriber.init(),
                LogOutput::Stderr => subscriber.with_writer(std::io::stderr).init(),
                LogOutput::File(path) => {
                    eprintln!("Warning: File logging not yet implemented, using stderr. Requested file: {}", path);
                    subscriber.with_writer(std::io::stderr).init();
                }
            }
        }
        LogFormat::Pretty => {
            let mut subscriber = fmt()
                .with_env_filter(filter)
                .with_target(true)
                .pretty();
            
            // Add file/line info for verbose modes or debug level
            if verbose_level > 1 || matches!(logging_config.level, LogLevel::Debug | LogLevel::Trace) {
                subscriber = subscriber.with_file(true).with_line_number(true);
            }
            
            match &logging_config.output {
                LogOutput::Stdout => subscriber.init(),
                LogOutput::Stderr => subscriber.with_writer(std::io::stderr).init(),
                LogOutput::File(path) => {
                    eprintln!("Warning: File logging not yet implemented, using stderr. Requested file: {}", path);
                    subscriber.with_writer(std::io::stderr).init();
                }
            }
        }
        LogFormat::Compact => {
            let mut subscriber = fmt()
                .with_env_filter(filter)
                .with_target(true)
                .compact();
            
            // Add file/line info for verbose modes or debug level
            if verbose_level > 1 || matches!(logging_config.level, LogLevel::Debug | LogLevel::Trace) {
                subscriber = subscriber.with_file(true).with_line_number(true);
            }
            
            match &logging_config.output {
                LogOutput::Stdout => subscriber.init(),
                LogOutput::Stderr => subscriber.with_writer(std::io::stderr).init(),
                LogOutput::File(path) => {
                    eprintln!("Warning: File logging not yet implemented, using stderr. Requested file: {}", path);
                    subscriber.with_writer(std::io::stderr).init();
                }
            }
        }
    }
}

/// Initialize logging with appropriate level based on verbosity (CLI-only mode)
fn init_logging(verbose_level: u8) {
    if verbose_level == 0 {
        // Fast path: use original simple initialization for default case
        // This is identical to the original code before CLI changes
        tracing_subscriber::fmt::init();
    } else {
        // Only use complex EnvFilter when --verbose specified (rare case)
        use tracing_subscriber::{fmt, EnvFilter};
        
        let default_level = match verbose_level {
            1 => "debug", 
            _ => "trace",
        };
        
        // Allow RUST_LOG to override CLI verbosity
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default_level));
        
        fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_file(verbose_level > 1)
            .with_line_number(verbose_level > 1)
            .init();
    }
}

/// Handle example config mode (--example-config)
fn handle_example_config_mode(_args: &Args) -> Result<()> {
    println!("Available example configurations:");
    println!();
    
    // Print examples info
    config::print_example_config_info();
    
    println!();
    println!("To use an example configuration:");
    println!("  1. Copy an example file to your desired location");
    println!("  2. Edit the configuration to match your environment");
    println!("  3. Run: cargo run -- --config path/to/config.yaml");
    
    println!();
    println!("To generate example configurations:");
    println!("  cargo run -- --generate-config minimal --output minimal.yaml");
    println!("  cargo run -- --generate-config production --output production.json");
    
    Ok(())
}

/// Handle config generation mode (--generate-config)
fn handle_generation_mode(args: &Args) -> Result<()> {
    let config_type = args.generate_config.as_ref().expect("generate_config should be Some");
    
    info!("Generating {} configuration", config_type);
    
    // Generate the appropriate configuration
    let config = match config_type.as_str() {
        "minimal" => UringRessConfig::generate_minimal(),
        "development" => UringRessConfig::generate_development(),
        "production" => UringRessConfig::generate_production(),
        "microservices" => UringRessConfig::generate_microservices(),
        _ => {
            error!("Invalid configuration type: {}", config_type);
            std::process::exit(1);
        }
    };
    
    // Determine output format and content
    let (output_content, format_name) = if let Some(output_path) = &args.output {
        // Auto-detect format from file extension
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
        
        // Write to file
        std::fs::write(output_path, &output_content)
            .map_err(|e| anyhow::anyhow!("Failed to write configuration file '{}': {}", output_path.display(), e))?;
            
        info!("Generated {} configuration written to: {}", config_type, output_path.display());
        return Ok(());
    } else {
        // Default to YAML output to stdout
        let content = config.to_yaml_pretty()
            .map_err(|e| anyhow::anyhow!("Failed to serialize config to YAML: {}", e))?;
        (content, "YAML")
    };
    
    // Output to stdout
    println!("# Generated {} configuration ({})", config_type, format_name);
    println!("# Use --output <file> to save to a file");
    println!();
    print!("{}", output_content);
    
    Ok(())
}

/// Load runtime config from environment variables (fallback mode)
fn load_runtime_config_from_env() -> RuntimeConfig {
    use std::collections::HashMap;
    use runtime_config::{ListenerInfo, Protocol};
    
    let http_port: u16 = std::env::var("HTTP_PORT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(8080);
    let standard_buffer_size: usize = std::env::var("STANDARD_BUFFER_SIZE")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(8192);
    
    // Create default HTTP listener for environment variable mode
    let mut listeners = HashMap::new();
    listeners.insert(http_port, ListenerInfo {
        protocol: Protocol::HTTP,
    });
    
    RuntimeConfig::new(
        listeners,
        standard_buffer_size,
    )
}