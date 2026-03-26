use clap::{ArgAction, Parser};
use std::path::PathBuf;

/// Command-line interface for Portail Gateway Controller
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(name = "portail")]
#[command(about = "High-Performance Kubernetes Gateway API Controller")]
#[command(
    long_about = "Portail is a Kubernetes Gateway API Controller. It provides sub-100μs P99 latency and >1M RPS throughput for HTTP and TCP proxying."
)]
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

    /// Enable verbose logging
    #[arg(short, long)]
    #[arg(help = "Enable verbose logging output")]
    #[arg(action = ArgAction::Count)]
    pub verbose: u8,

    /// Show example configuration file locations
    #[arg(long)]
    #[arg(help = "Display paths to example configuration files and exit")]
    pub example_config: bool,

    /// Generate example configuration file
    #[arg(long, value_name = "TYPE")]
    #[arg(help = "Generate example configuration file (minimal, development)")]
    #[arg(value_parser = ["minimal", "development"])]
    pub generate_config: Option<String>,

    /// Output file for generated configuration
    #[arg(long, value_name = "FILE")]
    #[arg(help = "Output file path for generated configuration (stdout if not specified)")]
    #[arg(requires = "generate_config")]
    pub output: Option<PathBuf>,

    /// Directory containing TLS certificate files ({name}.crt / {name}.key)
    #[arg(long, value_name = "DIR")]
    #[arg(help = "Directory for TLS certificate files (default: /etc/portail/certs)")]
    pub cert_dir: Option<PathBuf>,

    /// Run as Kubernetes Gateway API controller
    #[arg(long)]
    #[arg(help = "Watch Kubernetes Gateway API resources instead of loading a config file")]
    pub kubernetes: bool,

    /// Controller name for GatewayClass matching
    #[arg(long, default_value = "portail.epheo.eu/gateway-controller")]
    #[arg(help = "Controller name to match against GatewayClass spec.controllerName")]
    pub controller_name: String,

    /// Print supported Gateway API features as a comma-separated list and exit
    #[arg(long)]
    #[arg(help = "Print supported features (for conformance test integration) and exit")]
    pub supported_features: bool,
}

impl Args {
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
                        return Err(
                            "Configuration file must have a valid extension (.json, .yaml, .yml)"
                                .to_string(),
                        );
                    }
                }
            } else {
                return Err(
                    "Configuration file must have an extension (.json, .yaml, .yml)".to_string(),
                );
            }
        }

        // Validate that check-config and validate-only require config file
        if (self.check_config || self.validate_only) && self.config.is_none() {
            return Err(
                "--check-config and --validate-only require --config to be specified".to_string(),
            );
        }

        if self.kubernetes && self.config.is_some() {
            return Err("--kubernetes and --config are mutually exclusive".to_string());
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

            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: false,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_ok());

        let args = Args {
            config: Some(PathBuf::from("config.yaml")),
            validate_only: false,
            check_config: false,
            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: false,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_ok());

        let args = Args {
            config: Some(PathBuf::from("config.txt")),
            validate_only: false,
            check_config: false,
            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: false,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validation_mode_requires_config() {
        let args = Args {
            config: None,
            validate_only: true,
            check_config: false,
            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: false,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_err());

        let args = Args {
            config: None,
            validate_only: false,
            check_config: true,
            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: false,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_kubernetes_and_config_mutually_exclusive() {
        let args = Args {
            config: Some(PathBuf::from("config.yaml")),
            validate_only: false,
            check_config: false,
            verbose: 0,
            example_config: false,
            generate_config: None,
            output: None,
            cert_dir: None,
            kubernetes: true,
            controller_name: "portail.epheo.eu/gateway-controller".to_string(),
            supported_features: false,
        };
        assert!(args.validate().is_err());
    }
}
