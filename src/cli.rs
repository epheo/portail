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

    /// Whether portail manages Gateway/GatewayClass lifecycle status
    /// (Accepted/Programmed/addresses). Set false when running under
    /// portail-operator, which owns that status; portail then only reports
    /// per-listener status and route status.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    #[arg(
        help = "Manage Gateway/GatewayClass lifecycle status (set false under portail-operator)"
    )]
    pub manage_gateway_status: bool,

    /// Port for the readiness endpoint (/readyz) served in Kubernetes mode.
    /// Default 19099: in the conventional proxy-management range, well clear of
    /// common Gateway listener ports (80, 443, 8080, 8081, 8443, …) so it does
    /// not collide with the data plane within the same pod.
    #[arg(long, default_value_t = 19099)]
    #[arg(help = "Port for the /readyz readiness endpoint (Kubernetes mode)")]
    pub readiness_port: u16,

    /// Restrict the K8s controller to a single Gateway (`namespace/name`).
    /// Set by portail-operator (per-Gateway data-plane Deployments); absent =
    /// legacy unscoped mode that watches all Gateways cluster-wide.
    #[arg(long, value_name = "NS/NAME")]
    #[arg(help = "Restrict to a single Gateway (format: namespace/name); operator sets this")]
    pub gateway: Option<String>,

    /// Operator-supplied watch shape: which *gate-able* secondary resources this
    /// single-Gateway data plane needs to watch, so it does not open cluster-wide
    /// watches it will never use. Comma-separated tokens: `tls` (a TLS-terminate
    /// listener → watch TLS Secrets) and `ns-labels` (an `allowedRoutes: Selector`
    /// listener → watch Namespace labels). Route watches (HTTP/TCP/TLS/UDP) are
    /// never gated — any route may parentRef this Gateway and must receive a status
    /// — so no route token exists; unknown tokens are ignored for operator
    /// forward/backward-compatibility. Absent = legacy broad mode (watch every
    /// gate-able resource); present narrows to only the listed extras.
    /// portail-operator computes it from the Gateway's listeners and re-rolls the
    /// pod when the shape changes.
    #[arg(long, value_name = "TOKENS")]
    #[arg(
        help = "Operator-set watch shape (comma tokens); absent watches all secondary resources"
    )]
    pub watch_shape: Option<String>,
}

impl Args {
    /// Validate argument combinations and requirements.
    /// Config file extension/format checking lives in
    /// `PortailConfig::load_from_file` — the single source of truth for
    /// supported formats.
    pub fn validate(&self) -> Result<(), String> {
        // Validate that check-config and validate-only require config file
        if (self.check_config || self.validate_only) && self.config.is_none() {
            return Err(
                "--check-config and --validate-only require --config to be specified".to_string(),
            );
        }

        if self.kubernetes && self.config.is_some() {
            return Err("--kubernetes and --config are mutually exclusive".to_string());
        }

        // Validate --gateway format early so a malformed scope fails fast.
        self.gateway_scope()?;

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

    /// Parse `--gateway namespace/name` into a `(namespace, name)` scope.
    /// Returns `Ok(None)` when unset, `Err` with a clear message on malformed input.
    pub fn gateway_scope(&self) -> Result<Option<(String, String)>, String> {
        match self.gateway.as_deref() {
            None => Ok(None),
            Some(s) => {
                let (ns, name) = s
                    .split_once('/')
                    .ok_or_else(|| format!("--gateway must be 'namespace/name', got {:?}", s))?;
                if ns.is_empty() || name.is_empty() {
                    return Err(format!(
                        "--gateway namespace and name must be non-empty, got {:?}",
                        s
                    ));
                }
                Ok(Some((ns.to_string(), name.to_string())))
            }
        }
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
        };
        assert!(args.validate().is_ok());

        // Extension/format validation is owned by PortailConfig::load_from_file,
        // so an odd extension passes ARGUMENT validation and fails at load time.
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
        };
        assert!(args.validate().is_ok());
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
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
            manage_gateway_status: true,
            readiness_port: 19099,
            gateway: None,
            watch_shape: None,
        };
        assert!(args.validate().is_err());
    }
}
