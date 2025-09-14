use anyhow::Result;
use tracing::{info, error};
use std::time::Duration;
use std::fs::OpenOptions;
use std::path::Path;

mod routing;
mod runtime_config;
mod cli;
mod config;
mod control_plane;
mod uring_worker;
mod uring_data_plane;
mod ebpf;
mod backend_pool;

use control_plane::ControlPlane;
use uring_data_plane::UringDataPlane;
use runtime_config::RuntimeConfig;
use cli::Args;
use config::UringRessConfig;

/// Parse duration string (e.g., "30s", "2m", "1h") to Duration
fn parse_duration(duration_str: &str) -> Result<Duration> {
    let duration_str = duration_str.trim();
    
    if duration_str.is_empty() {
        return Err(anyhow::anyhow!("Duration cannot be empty"));
    }
    
    // Extract number and unit
    let (number_part, unit_part) = if let Some(pos) = duration_str.find(|c: char| c.is_alphabetic()) {
        duration_str.split_at(pos)
    } else {
        // No unit, assume seconds
        (duration_str, "s")
    };
    
    let number: u64 = number_part.parse()
        .map_err(|_| anyhow::anyhow!("Invalid number in duration: {}", number_part))?;
    
    let duration = match unit_part {
        "s" | "sec" | "second" | "seconds" => Duration::from_secs(number),
        "m" | "min" | "minute" | "minutes" => Duration::from_secs(number * 60),
        "h" | "hour" | "hours" => Duration::from_secs(number * 3600),
        _ => return Err(anyhow::anyhow!("Invalid duration unit: {}. Use s, m, or h", unit_part)),
    };
    
    if duration.as_secs() == 0 {
        return Err(anyhow::anyhow!("Duration must be greater than 0"));
    }
    
    if duration.as_secs() > 3600 {
        return Err(anyhow::anyhow!("Duration cannot exceed 1 hour"));
    }
    
    Ok(duration)
}

fn main() -> Result<()> {
    // Parse CLI arguments once at startup - startup overhead only
    let args = Args::parse_args();
    
    // Validate CLI arguments (before logging init)
    if let Err(error) = args.validate() {
        eprintln!("Invalid arguments: {}", error);
        std::process::exit(1);
    }
    
    // Handle config generation mode BEFORE eBPF validation - no eBPF needed for config generation
    if args.is_generation_mode() {
        // Initialize minimal logging for generation mode
        init_logging(args.verbose);
        return handle_generation_mode(&args);
    }
    
    // FIRST: Validate eBPF system requirements - fail fast if not met
    // Single codepath architecture: either works completely or fails completely
    if let Err(e) = ebpf::validate_system_requirements() {
        eprintln!("eBPF system requirements not met: {}", e);
        eprintln!("UringRess uses single codepath architecture with no fallback behavior.");
        std::process::exit(1);
    }
    
    // Load configuration first to determine logging settings
    let (runtime_config, use_file_routes, performance_config) = if let Some(config_path) = &args.config {
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
        let runtime_config = uringress_config.performance.to_runtime_config(&uringress_config.gateway)?;
        info!("Configuration loaded successfully from file");
        
        (runtime_config, Some(uringress_config.clone()), uringress_config.performance)
    } else {
        use crate::config::PerformanceConfig;
        // Initialize basic logging for environment/CLI-only mode
        init_logging(args.verbose);
        
        info!("No configuration file provided - starting with empty routes and environment variables");
        
        // Use environment variable parsing for runtime config, but no routes
        let runtime_config = load_runtime_config_from_env();
        // Create default performance config for environment mode
        let performance_config = PerformanceConfig::default();
        (runtime_config, None, performance_config)
    };
    
    // Handle validation modes that exit early (after logging is initialized)
    if args.is_validation_mode() {
        return handle_validation_mode(&args);
    }
    
    // Handle example config mode (after logging is initialized)
    if args.example_config {
        return handle_example_config_mode(&args);
    }
    

    // Handle CPU profiling mode
    let profile_duration = if let Some(duration_str) = &args.profile_cpu {
        Some(parse_duration(duration_str)?)
    } else {
        None
    };
    
    // Extract worker configurations - use dynamic configs if available, fallback to default generation
    let worker_configs = if let Some(ref config) = use_file_routes {
        config.gateway.get_worker_configs()
    } else {
        // Generate default worker configs for environment-based setup
        let num_workers = args.workers.unwrap_or_else(|| {
            std::env::var("WORKER_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8)
        });
        
        use crate::config::{WorkerConfig, Protocol};
        
        // Create balanced HTTP/TCP worker distribution for environment setup
        let mut configs = Vec::with_capacity(num_workers);
        let http_workers = (num_workers + 1) / 2; // Round up for HTTP
        
        // HTTP workers (first half)
        for id in 0..http_workers {
            configs.push(WorkerConfig {
                id,
                protocol: Protocol::HTTP,
                buffer_size: 8192,  // 8KB for HTTP
                buffer_count: 256,  // Increased for high concurrent load
                tcp_nodelay: false,
            });
        }
        
        // TCP workers (second half)
        for id in http_workers..num_workers {
            configs.push(WorkerConfig {
                id,
                protocol: Protocol::TCP,
                buffer_size: 65536, // 64KB for TCP bulk transfers
                buffer_count: 16,
                tcp_nodelay: true,
            });
        }
        
        configs
    };
    
    // Validate worker configurations  
    if use_file_routes.is_none() {
        // Validate environment-generated worker configs
        use crate::config::GatewayConfig;
        let temp_gateway = GatewayConfig {
            name: "temp".to_string(),
            listeners: vec![],
            workers: Some(worker_configs.clone()),
            worker_threads: worker_configs.len(),
            api_port: 8081,
            api_enabled: false,
            io_uring_queue_depth: 256,
            connection_pool_size: 64,
            buffer_pool_size: 64 * 1024 * 1024,
            network_interface: None,
        };
        
        if let Err(e) = temp_gateway.validate_worker_configs() {
            eprintln!("Worker configuration validation failed: {}", e);
            std::process::exit(1);
        }
        
        info!("Worker configurations validated successfully");
    }
    
    // Verbose level already used for early logging initialization
    
    
    // Runtime config already loaded above from file or environment
    // Logging already initialized early in main()
    
    // Initialize CPU profiling if requested
    let profiler_guard = if let Some(duration) = profile_duration {
        info!("Starting CPU profiling for {} seconds", duration.as_secs());
        info!("Flamegraph will be generated automatically after profiling");
        
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let output_dir = std::path::Path::new("tests/integration/reports/cpu");
        std::fs::create_dir_all(output_dir)?;
        let flamegraph_path = output_dir.join(format!("uringress_runtime_{}.svg", timestamp));
        
        // Start pprof CPU profiler - keep guard alive in main thread
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(1000) // 1000 Hz sampling
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to start profiler: {}", e))?;
            
        info!("CPU profiler started successfully");
        
        // Schedule flamegraph generation after profiling duration
        let guard_clone = std::sync::Arc::new(std::sync::Mutex::new(Some(guard)));
        let guard_for_thread = guard_clone.clone();
        let flamegraph_path_for_thread = flamegraph_path.clone();
        
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            info!("Profiling duration completed, generating flamegraph...");
            
            if let Ok(mut guard_opt) = guard_for_thread.lock() {
                if let Some(guard) = guard_opt.take() {
                    info!("Building profiling report...");
                    match guard.report().build() {
                        Ok(report) => {
                            // Get sample count for debugging
                            let samples = report.data.keys().len();
                            info!("Profiling report built with {} unique stack traces", samples);
                            
                            if samples == 0 {
                                info!("⚠ No samples collected - application may have been idle or profiling duration too short");
                            } else {
                                // Generate flamegraph
                                let mut flamegraph_data = Vec::new();
                                match report.flamegraph(&mut flamegraph_data) {
                                    Ok(_) => {
                                        info!("Flamegraph data generated, size: {} bytes", flamegraph_data.len());
                                        
                                        // Write to file
                                        match std::fs::write(&flamegraph_path_for_thread, &flamegraph_data) {
                                            Ok(_) => {
                                                info!("✓ Flamegraph saved to: {}", flamegraph_path_for_thread.display());
                                                info!("🔥 CPU profiling complete! 🔥");
                                            }
                                            Err(e) => error!("Failed to write flamegraph file: {}", e),
                                        }
                                    }
                                    Err(e) => error!("Failed to generate flamegraph: {}", e),
                                }
                            }
                        }
                        Err(e) => error!("Failed to build profiling report: {}", e),
                    }
                }
            }
        });
        
        Some((guard_clone, flamegraph_path, duration))
    } else {
        None
    };

    // Create hybrid runtime architecture: Tokio control plane + raw io_uring data plane
    info!("Starting UringRess with hybrid runtime architecture");
    info!("Control Plane: Tokio runtime for kube-rs compatibility");
    info!("Data Plane: Raw io_uring for maximum performance");
    
    // Capture performance config before moving into control plane thread
    let performance_config_for_thread = performance_config.clone();
    
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
            let mut control_plane = if let Some(ref config) = use_file_routes {
                ControlPlane::new_with_config(config.clone())?
            } else {
                ControlPlane::new()?
            };
            
            // Start control plane with eBPF integration
            control_plane.start().await?;
            
            // Get routing table reference for data plane
            let routing_table = control_plane.get_routes();
            
            // Start data plane in separate threads with raw io_uring and eBPF awareness
            let mut data_plane = UringDataPlane::new(worker_configs.clone());
            // Get backend pool config - use default if no config file
            let backend_pool_config = if let Some(ref config) = use_file_routes {
                &config.backend_pool
            } else {
                &crate::config::BackendPoolConfig::default()
            };
            data_plane.start(routing_table, runtime_config, performance_config_for_thread, backend_pool_config)?;
            
            // Attach SO_REUSEPORT eBPF programs to worker sockets
            if let Some(listener_fds) = data_plane.get_listener_fds() {
                info!("Attaching SO_REUSEPORT eBPF programs to {} worker sockets", listener_fds.len());
                control_plane.attach_worker_reuseport_programs(&listener_fds)?;
                info!("SO_REUSEPORT eBPF programs attached successfully - address-aware distribution enabled");
                
                // Release workers to start event loops after eBPF attachment
                data_plane.start_workers_after_ebpf_attachment()?;
                info!("Worker event loops started - ready to accept connections");
            } else {
                return Err(anyhow::anyhow!(
                    "No listener FDs collected from data plane. \
                    \nSingle codepath architecture requires eBPF socket attachment. \
                    \nCause: Workers failed to create listeners or coordination error. \
                    \nSolution: Check worker initialization logs above."
                ));
            }
            
            info!("UringRess hybrid architecture ready");
            info!("Control Plane: Tokio runtime (1 thread)");
            info!("Data Plane: Raw io_uring ({} workers)", worker_configs.len());
            
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
    
    // Profiling guard cleanup (flamegraph is generated in background thread now)
    if let Some((_guard, _flamegraph_path, _duration)) = profiler_guard {
        info!("CPU profiling completed in background thread");
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

/// Create a file writer for logging, creating parent directories if needed
fn create_log_file_writer(path: &str) -> Result<std::fs::File> {
    let log_path = Path::new(path);
    
    // Create parent directories if they don't exist
    if let Some(parent) = log_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create log directory {}: {}", parent.display(), e))?;
        }
    }
    
    // Open file with create and append options
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| anyhow::anyhow!("Failed to open log file {}: {}", path, e))
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
                    match create_log_file_writer(path) {
                        Ok(file) => subscriber.with_writer(file).init(),
                        Err(e) => {
                            eprintln!("Error: Failed to create log file '{}': {}", path, e);
                            eprintln!("Falling back to stderr for logging");
                            subscriber.with_writer(std::io::stderr).init();
                        }
                    }
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
                    match create_log_file_writer(path) {
                        Ok(file) => subscriber.with_writer(file).init(),
                        Err(e) => {
                            eprintln!("Error: Failed to create log file '{}': {}", path, e);
                            eprintln!("Falling back to stderr for logging");
                            subscriber.with_writer(std::io::stderr).init();
                        }
                    }
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
                    match create_log_file_writer(path) {
                        Ok(file) => subscriber.with_writer(file).init(),
                        Err(e) => {
                            eprintln!("Error: Failed to create log file '{}': {}", path, e);
                            eprintln!("Falling back to stderr for logging");
                            subscriber.with_writer(std::io::stderr).init();
                        }
                    }
                }
            }
        }
    }
}

/// Initialize logging with appropriate level based on verbosity (CLI-only mode)
fn init_logging(verbose_level: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    
    // In release mode, default to WARN level for performance
    // In debug mode, default to INFO level
    #[cfg(debug_assertions)]
    let default_level = match verbose_level {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    
    #[cfg(not(debug_assertions))]
    let default_level = match verbose_level {
        0 => "warn",  // Release mode: only warnings and errors
        1 => "info",  // Release mode with -v: include info
        2 => "debug", // Release mode with -vv: include debug
        _ => "trace", // Release mode with -vvv: include trace
    };
    
    // Allow RUST_LOG to override CLI verbosity
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    
    if verbose_level == 0 && cfg!(not(debug_assertions)) {
        // Fast path for release builds without verbosity
        fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
    } else {
        
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
    
    // Create default HTTP listener for environment variable mode
    let mut listeners = HashMap::new();
    listeners.insert(http_port, ListenerInfo {
        protocol: Protocol::HTTP,
        address: None, // Default to wildcard for environment variable mode
        interface: None, // No interface binding for environment variable mode
    });
    
    RuntimeConfig::new(
        listeners,
    )
}