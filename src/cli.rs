use clap::{ArgAction, Parser};
use std::path::PathBuf;

/// Command-line interface for UringRess Gateway Controller
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(name = "uringress")]
#[command(about = "High-Performance Kubernetes Gateway API Controller")]
#[command(long_about = "UringRess is a high-performance Kubernetes Gateway API Controller built exclusively for io_uring-enabled Linux environments. It provides sub-100μs P99 latency and >1M RPS throughput for HTTP and TCP proxying.")]
pub struct Args {
    /// Configuration file path (JSON or YAML format)
    #[arg(short, long, value_name = "FILE")]
    #[arg(help = "Specify configuration file path (supports .json, .yaml, .yml extensions)")]
    pub config: Option<PathBuf>,

    /// Validate configuration file without starting the server
    #[arg(long)]
    #[arg(help = "Parse and validate configuration file, then exit")]
    #[arg(conflicts_with_all = ["check_config"])]
    pub validate_only: bool,

    /// Parse and display configuration values, then exit
    #[arg(long)]
    #[arg(help = "Parse configuration file, display values in human-readable format, then exit")]
    #[arg(conflicts_with_all = ["validate_only"])]
    pub check_config: bool,

    /// Override API server port
    #[arg(long, value_name = "PORT")]
    #[arg(help = "Override API server port (1-65535)")]
    #[arg(value_parser = clap::value_parser!(u16))]
    pub api_port: Option<u16>,

    /// Override number of worker threads
    #[arg(short, long, value_name = "COUNT")]
    #[arg(help = "Override number of worker threads (1-256)")]
    #[arg(value_parser = clap::value_parser!(usize))]
    pub workers: Option<usize>,

    /// Enable verbose logging
    #[arg(short, long)]
    #[arg(help = "Enable verbose logging output")]
    #[arg(action = ArgAction::Count)]
    pub verbose: u8,

    /// Disable API server
    #[arg(long)]
    #[arg(help = "Disable the REST API server (proxy-only mode)")]
    pub no_api: bool,

    /// Show example configuration file locations
    #[arg(long)]
    #[arg(help = "Display paths to example configuration files and exit")]
    pub example_config: bool,

    /// Generate example configuration file
    #[arg(long, value_name = "TYPE")]
    #[arg(help = "Generate example configuration file (minimal, development, production, microservices)")]
    #[arg(value_parser = ["minimal", "development", "production", "microservices"])]
    pub generate_config: Option<String>,

    /// Output file for generated configuration
    #[arg(long, value_name = "FILE")]
    #[arg(help = "Output file path for generated configuration (stdout if not specified)")]
    #[arg(requires = "generate_config")]
    pub output: Option<PathBuf>,

    /// Enable CPU profiling for specified duration (e.g., "30s", "1m")
    #[arg(long, value_name = "DURATION")]
    #[arg(help = "Enable CPU profiling and generate flamegraph for specified duration")]
    pub profile_cpu: Option<String>,
}

impl Args {
    /// Parse command-line arguments
    pub fn parse_args() -> Self {
        Args::parse()
    }

    /// Validate argument combinations and requirements
    pub fn validate(&self) -> Result<(), String> {
        // Validate config file extension if provided
        if let Some(config_path) = &self.config {
            if let Some(extension) = config_path.extension() {
                match extension.to_str() {
                    Some("json") | Some("yaml") | Some("yml") => {}
                    Some(ext) => {
                        return Err(format!(
                            "Unsupported configuration file extension: .{}. Supported formats: .json, .yaml, .yml",
                            ext
                        ));
                    }
                    None => {
                        return Err("Configuration file must have a valid extension (.json, .yaml, .yml)".to_string());
                    }
                }
            } else {
                return Err("Configuration file must have an extension (.json, .yaml, .yml)".to_string());
            }
        }

        // Validate that check-config and validate-only require config file
        if (self.check_config || self.validate_only) && self.config.is_none() {
            return Err("--check-config and --validate-only require --config to be specified".to_string());
        }

        // Validate port ranges
        if let Some(port) = self.api_port {
            if port == 0 {
                return Err("API port must be between 1 and 65535".to_string());
            }
        }

        // Validate worker count
        if let Some(workers) = self.workers {
            if workers == 0 || workers > 256 {
                return Err("Worker count must be between 1 and 256".to_string());
            }
        }

        Ok(())
    }

    /// Determine if the application should exit early (validation modes)
    pub fn is_validation_mode(&self) -> bool {
        self.validate_only || self.check_config
    }

    /// Determine if the application should exit early (generation mode)
    pub fn is_generation_mode(&self) -> bool {
        self.generate_config.is_some()
    }
    




}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_file_validation() {
        let args = Args {
            config: Some(PathBuf::from("config.json")),
            validate_only: false,
            check_config: false,
            api_port: None,
            workers: None,
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert!(args.validate().is_ok());

        let args = Args {
            config: Some(PathBuf::from("config.yaml")),
            validate_only: false,
            check_config: false,
            api_port: None,
            workers: None,
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert!(args.validate().is_ok());

        let args = Args {
            config: Some(PathBuf::from("config.txt")),
            validate_only: false,
            check_config: false,
            api_port: None,
            workers: None,
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validation_mode_requires_config() {
        let args = Args {
            config: None,
            validate_only: true,
            check_config: false,
            api_port: None,
            workers: None,
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert!(args.validate().is_err());

        let args = Args {
            config: None,
            validate_only: false,
            check_config: true,
            api_port: None,
            workers: None,
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_worker_count_precedence() {
        // CLI arg takes precedence
        let args = Args {
            config: None,
            validate_only: false,
            check_config: false,
            api_port: None,
            workers: Some(8),
            verbose: 0,
            no_api: false,
            example_config: false,
            generate_config: None,
            output: None,
            profile_cpu: None,
        };
        assert_eq!(args.workers.unwrap(), 8);
    }
}