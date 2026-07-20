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
    /// Specify configuration file path (supports .json, .yaml, .yml extensions)
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Parse and validate configuration file, then exit
    #[arg(long)]
    #[arg(conflicts_with_all = ["check_config"])]
    pub validate_only: bool,

    /// Parse configuration file, display values in human-readable format, then exit
    #[arg(long)]
    #[arg(conflicts_with_all = ["validate_only"])]
    pub check_config: bool,

    /// Enable verbose logging output
    #[arg(short, long)]
    #[arg(action = ArgAction::Count)]
    pub verbose: u8,

    /// Display paths to example configuration files and exit
    #[arg(long)]
    pub example_config: bool,

    /// Generate example configuration file
    #[arg(long, value_name = "TYPE")]
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(
        portail::config::examples::EXAMPLES.iter().map(|(name, _, _)| *name)
    ))]
    pub generate_config: Option<String>,

    /// Output file path for generated configuration (stdout if not specified)
    #[arg(long, value_name = "FILE")]
    #[arg(requires = "generate_config")]
    pub output: Option<PathBuf>,

    /// Directory for TLS certificate files ({name}.crt / {name}.key pairs)
    #[arg(long, value_name = "DIR")]
    pub cert_dir: Option<PathBuf>,

    /// Watch Kubernetes Gateway API resources instead of loading a config file
    #[arg(long)]
    pub kubernetes: bool,

    /// Controller name to match against GatewayClass spec.controllerName
    #[arg(long, default_value = "portail.epheo.eu/gateway-controller")]
    pub controller_name: String,

    /// Print supported features (for conformance test integration) and exit
    #[arg(long)]
    pub supported_features: bool,

    /// Manage Gateway/GatewayClass lifecycle status (set false under portail-operator)
    ///
    /// Covers Accepted/Programmed/addresses. Set false when running under
    /// portail-operator, which owns that status; portail then only reports
    /// per-listener status and route status.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub manage_gateway_status: bool,

    /// Port for the /readyz + /metrics admin endpoint (Kubernetes mode)
    ///
    /// Default 19099: in the conventional proxy-management range, well clear
    /// of common Gateway listener ports (80, 443, 8080, 8081, 8443, ...) so
    /// it does not collide with the data plane within the same pod.
    #[arg(long, default_value_t = 19099)]
    pub readiness_port: u16,

    /// Serve /metrics + /readyz on this port in standalone mode (off by default)
    ///
    /// Opt-in because standalone mode has no readinessProbe needing one.
    /// Readiness reports ready once listeners are up.
    #[arg(long, value_name = "PORT")]
    pub metrics_port: Option<u16>,

    /// Write a JSON access log line per HTTP response to PATH ("-" = stdout)
    ///
    /// A sink slower than the request rate sheds lines (counted in
    /// portail_access_log_dropped_total) rather than slowing the data path.
    #[arg(long, value_name = "PATH")]
    pub access_log: Option<String>,

    /// Enable HTTP/2 on TLS-terminate listeners (ALPN h2 + http/1.1)
    ///
    /// Opt-in front end: TLS-terminate listeners advertise h2 via ALPN and
    /// bridge each stream through the HTTP/1.1 engine. Equivalent to
    /// `performance.http2: true`; the flag is the Kubernetes-mode path,
    /// where no config file exists.
    #[arg(long)]
    pub http2: bool,

    /// Restrict to a single Gateway (format: namespace/name); operator sets this
    ///
    /// Set by portail-operator (per-Gateway data-plane Deployments); absent =
    /// legacy unscoped mode that watches all Gateways cluster-wide.
    #[arg(long, value_name = "NS/NAME")]
    pub gateway: Option<String>,

    /// Operator-set watch shape (comma tokens); absent watches all secondary resources
    ///
    /// Which *gate-able* secondary resources this single-Gateway data plane
    /// needs to watch, so it does not open cluster-wide watches it will never
    /// use. Tokens: `tls` (a TLS-terminate listener means watch TLS Secrets)
    /// and `ns-labels` (an `allowedRoutes: Selector` listener means watch
    /// Namespace labels). Route watches (HTTP/TCP/TLS/UDP) are never gated:
    /// any route may parentRef this Gateway and must receive a status, so no
    /// route token exists; unknown tokens are ignored for operator
    /// forward/backward-compatibility. Absent = legacy broad mode (watch
    /// every gate-able resource); present narrows to only the listed extras.
    /// portail-operator computes it from the Gateway's listeners and re-rolls
    /// the pod when the shape changes.
    #[arg(long, value_name = "TOKENS")]
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

    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn test_config_file_validation() {
        let args = parse(&["portail", "--config", "config.json"]);
        assert!(args.validate().is_ok());

        let args = parse(&["portail", "--config", "config.yaml"]);
        assert!(args.validate().is_ok());

        // Extension/format validation is owned by PortailConfig::load_from_file,
        // so an odd extension passes ARGUMENT validation and fails at load time.
        let args = parse(&["portail", "--config", "config.txt"]);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validation_mode_requires_config() {
        let args = parse(&["portail", "--validate-only"]);
        assert!(args.validate().is_err());

        let args = parse(&["portail", "--check-config"]);
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_kubernetes_and_config_mutually_exclusive() {
        let args = parse(&["portail", "--kubernetes", "--config", "config.yaml"]);
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_validate_only_conflicts_with_check_config() {
        assert!(Args::try_parse_from(["portail", "--validate-only", "--check-config"]).is_err());
    }

    #[test]
    fn test_generate_config_rejects_unknown_example() {
        assert!(Args::try_parse_from(["portail", "--generate-config", "nope"]).is_err());
        for (name, _, _) in portail::config::examples::EXAMPLES {
            assert!(Args::try_parse_from(["portail", "--generate-config", name]).is_ok());
        }
    }
}
