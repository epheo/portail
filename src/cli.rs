use clap::{ArgAction, Parser, Subcommand};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use crate::sync::GatewayKey;

/// Command-line interface for Portail Gateway Controller
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(name = "portail")]
#[command(about = "High-Performance Kubernetes Gateway API Controller")]
#[command(
    long_about = "Portail is a Kubernetes Gateway API Controller. It provides sub-100μs P99 latency and >1M RPS throughput for HTTP and TCP proxying.\n\nWithout a subcommand it runs standalone, from --config or its built-in default config."
)]
// The flags below are standalone mode's: a subcommand takes its own.
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

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
    #[arg(short, long, global = true)]
    #[arg(action = ArgAction::Count)]
    pub verbose: u8,

    /// List the built-in example configurations and exit
    #[arg(long)]
    pub example_config: bool,

    /// Generate example configuration file
    #[arg(long, value_name = "TYPE")]
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(
        crate::config::examples::EXAMPLES.iter().map(|(name, _, _)| *name)
    ))]
    pub generate_config: Option<String>,

    /// Output file path for generated configuration (stdout if not specified)
    #[arg(long, value_name = "FILE")]
    #[arg(requires = "generate_config")]
    pub output: Option<PathBuf>,

    /// Directory for TLS certificate files ({name}.crt / {name}.key pairs)
    #[arg(long, value_name = "DIR")]
    pub cert_dir: Option<PathBuf>,

    /// Print supported features (for conformance test integration) and exit
    #[arg(long)]
    pub supported_features: bool,

    /// Print portail's CRD manifests (GatewayParameters, RateLimitPolicy) as YAML and exit
    ///
    /// The Rust types are the source of truth; split this into
    /// deploy/crds/ when they change (a test enforces sync).
    #[arg(long)]
    pub print_crd: bool,

    /// Serve /metrics + /readyz on this port (off by default)
    ///
    /// Opt-in because standalone mode has no readinessProbe needing one.
    /// Readiness reports ready once listeners are up.
    #[arg(long, value_name = "PORT")]
    pub metrics_port: Option<u16>,

    /// Bind address of the admin endpoint (/livez, /readyz, /metrics)
    ///
    /// Default 0.0.0.0, every interface.
    #[arg(long, value_name = "IP", default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    pub admin_address: IpAddr,

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
    /// `performance.http2: true`.
    #[arg(long)]
    pub http2: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Watch Gateway API resources cluster-wide, serve each Gateway's data
    /// plane its configuration, provision data planes and write status
    Controller(ControllerArgs),
    /// Serve one Gateway from the configuration the controller sends
    Dataplane(DataplaneArgs),
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Controller(_) => "controller",
            Command::Dataplane(_) => "dataplane",
        }
    }
}

/// The admin endpoint of both Kubernetes modes: /livez, /readyz, /metrics.
#[derive(clap::Args, Debug, Clone)]
pub struct AdminArgs {
    /// Port of the admin endpoint
    ///
    /// Default 19099: in the conventional proxy-management range, well clear
    /// of common Gateway listener ports (80, 443, 8080, 8081, 8443, ...) so
    /// it does not collide with the data plane within the same pod.
    #[arg(long, default_value_t = 19099)]
    pub readiness_port: u16,

    /// Bind address of the admin endpoint
    ///
    /// Default 0.0.0.0, every interface: kubelet probes reach the pod IP,
    /// so a loopback bind would fail readiness. The pod IP (downward API
    /// status.podIP) keeps /metrics off public VIP and Multus interfaces.
    #[arg(long, value_name = "IP", default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    pub admin_address: IpAddr,
}

#[derive(clap::Args, Debug, Clone)]
pub struct ControllerArgs {
    /// Controller name to match against GatewayClass spec.controllerName
    #[arg(long, default_value = "portail.epheo.eu/gateway-controller")]
    pub controller_name: String,

    /// Namespace of the controller's lease, CA Secret and ConfigMap, and Service
    #[arg(long, env = "POD_NAMESPACE")]
    pub namespace: String,

    /// The ClusterIP Service data planes dial; `<service>.<namespace>.svc`
    /// is also the audience of their tokens
    #[arg(long, default_value = "portail-controller")]
    pub service_name: String,

    /// Port of the sync endpoint
    ///
    /// Default 9443, the controller-runtime webhook port convention, apart
    /// from the admin port.
    #[arg(long, default_value_t = 9443)]
    pub sync_port: u16,

    /// Name of the leader election Lease
    #[arg(long, default_value = "portail-controller")]
    pub leader_election_id: String,

    /// Data-plane image of Managed Gateways without a GatewayParameters image
    ///
    /// Default: the image this binary was published as.
    #[arg(long, value_name = "IMAGE", default_value_t = default_dataplane_image())]
    pub dataplane_image: String,

    #[command(flatten)]
    pub admin: AdminArgs,
}

/// Where a data plane reads the controller CA's PEM when
/// `--controller-ca-file` is not given: managed pods get it in their template.
pub const CONTROLLER_CA_ENV: &str = "PORTAIL_CONTROLLER_CA";

#[derive(clap::Args, Debug, Clone)]
pub struct DataplaneArgs {
    /// The Gateway this data plane serves
    #[arg(long, value_name = "NS/NAME", value_parser = parse_gateway_key)]
    pub gateway: GatewayKey,

    /// The controller's sync endpoint, e.g. https://portail-controller.portail-system.svc:9443
    #[arg(long, value_name = "URL")]
    pub controller_address: String,

    /// PEM file of the CA the controller's serving cert chains to; default:
    /// the PEM in env PORTAIL_CONTROLLER_CA
    #[arg(long, value_name = "FILE")]
    pub controller_ca_file: Option<PathBuf>,

    /// Projected ServiceAccount token presented to the controller, re-read
    /// per request (kubelet rotates it)
    #[arg(
        long,
        value_name = "FILE",
        default_value = "/var/run/secrets/portail.epheo.eu/controller/token"
    )]
    pub token_file: PathBuf,

    /// On SIGTERM, keep serving this long before draining
    ///
    /// Default 5s: the rendered readinessProbe period. Endpoint removal
    /// reaches kube-proxy in about a second and LB health checks commonly
    /// run on a 5s cadence.
    #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = parse_duration)]
    pub shutdown_delay: Duration,

    #[command(flatten)]
    pub admin: AdminArgs,

    /// Write a JSON access log line per HTTP response to PATH ("-" = stdout)
    #[arg(long, value_name = "PATH")]
    pub access_log: Option<String>,

    /// Enable HTTP/2 on TLS-terminate listeners (ALPN h2 + http/1.1)
    #[arg(long)]
    pub http2: bool,
}

/// The data-plane image a controller renders by default: the reference the
/// image was published under, baked in at build time (publish.yml passes
/// `PORTAIL_IMAGE`), else this version's release tag. Cargo's version only
/// moves at release, so a :main build without the bake would render the
/// previous release's data plane.
pub fn default_dataplane_image() -> String {
    match option_env!("PORTAIL_IMAGE") {
        Some(image) if !image.is_empty() => image.to_string(),
        _ => format!("ghcr.io/epheo/portail:{}", env!("CARGO_PKG_VERSION")),
    }
}

fn parse_gateway_key(s: &str) -> Result<GatewayKey, String> {
    s.parse()
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    crate::config::parsing::parse_duration(s).map_err(|e| e.to_string())
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
    fn test_validate_only_conflicts_with_check_config() {
        assert!(Args::try_parse_from(["portail", "--validate-only", "--check-config"]).is_err());
    }

    #[test]
    fn test_admin_address_defaults_to_every_interface() {
        let any: IpAddr = "0.0.0.0".parse().unwrap();
        assert_eq!(parse(&["portail"]).admin_address, any);
        let pod_ip: IpAddr = "10.128.0.7".parse().unwrap();
        let args = parse(&["portail", "--admin-address", "10.128.0.7"]);
        assert_eq!(args.admin_address, pod_ip);
        assert!(Args::try_parse_from(["portail", "--admin-address", "pod-ip"]).is_err());
    }

    #[test]
    fn test_generate_config_rejects_unknown_example() {
        assert!(Args::try_parse_from(["portail", "--generate-config", "nope"]).is_err());
        for (name, _, _) in crate::config::examples::EXAMPLES {
            assert!(Args::try_parse_from(["portail", "--generate-config", name]).is_ok());
        }
    }

    #[test]
    fn no_subcommand_runs_standalone() {
        let args = parse(&["portail"]);
        assert!(args.command.is_none());
        assert!(args.validate().is_ok());
    }

    #[test]
    fn controller_takes_its_flags_with_their_defaults() {
        let Some(Command::Controller(c)) =
            parse(&["portail", "controller", "--namespace", "ps"]).command
        else {
            panic!("not the controller");
        };
        assert_eq!(c.controller_name, "portail.epheo.eu/gateway-controller");
        assert_eq!(c.namespace, "ps");
        assert_eq!(c.service_name, "portail-controller");
        assert_eq!(c.sync_port, 9443);
        assert_eq!(c.leader_election_id, "portail-controller");
        assert_eq!(c.dataplane_image, default_dataplane_image());
        assert_eq!(c.admin.readiness_port, 19099);

        let Some(Command::Controller(c)) = parse(&[
            "portail",
            "controller",
            "--namespace=ps",
            "--controller-name=example.com/gw",
            "--service-name=ctl",
            "--sync-port=8443",
            "--leader-election-id=lease",
            "--dataplane-image=registry.local/portail:dev",
            "--readiness-port=9000",
            "--admin-address=10.0.0.5",
            "-v",
        ])
        .command
        else {
            panic!("not the controller");
        };
        assert_eq!(
            (
                c.sync_port,
                c.admin.readiness_port,
                c.dataplane_image.as_str()
            ),
            (8443, 9000, "registry.local/portail:dev")
        );
    }

    #[test]
    fn dataplane_takes_its_flags_with_their_defaults() {
        let Some(Command::Dataplane(d)) = parse(&[
            "portail",
            "dataplane",
            "--gateway",
            "portail-system/public",
            "--controller-address",
            "https://portail-controller.portail-system.svc:9443",
        ])
        .command
        else {
            panic!("not the data plane");
        };
        assert_eq!(d.gateway, GatewayKey::new("portail-system", "public"));
        assert_eq!(d.shutdown_delay, Duration::from_secs(5));
        assert_eq!(
            d.token_file,
            PathBuf::from("/var/run/secrets/portail.epheo.eu/controller/token")
        );
        assert!(d.controller_ca_file.is_none() && !d.http2 && d.access_log.is_none());

        let Some(Command::Dataplane(d)) = parse(&[
            "portail",
            "dataplane",
            "--gateway=ns/gw",
            "--controller-address=https://ctl:9443",
            "--controller-ca-file=/ca.crt",
            "--token-file=/token",
            "--shutdown-delay=0s",
            "--readiness-port=9000",
            "--admin-address=10.0.0.5",
            "--access-log=-",
            "--http2",
        ])
        .command
        else {
            panic!("not the data plane");
        };
        assert_eq!(d.shutdown_delay, Duration::ZERO);
        assert!(d.http2 && d.access_log.as_deref() == Some("-"));
    }

    #[test]
    fn a_dataplane_needs_its_gateway_and_controller() {
        let base = ["portail", "dataplane"];
        assert!(Args::try_parse_from(base).is_err());
        assert!(Args::try_parse_from([&base[..], &["--gateway", "ns/gw"]].concat()).is_err());
        assert!(Args::try_parse_from(
            [
                &base[..],
                &["--gateway", "gw", "--controller-address", "https://c"]
            ]
            .concat()
        )
        .is_err());
    }

    /// The legacy Kubernetes mode is gone, and standalone flags do not
    /// leak into the subcommands.
    #[test]
    fn removed_flags_are_rejected() {
        for argv in [
            &["portail", "--kubernetes"][..],
            &["portail", "--gateway", "ns/gw"],
            &["portail", "--watch-shape", "tls"],
            &["portail", "--manage-gateway-status", "false"],
            &["portail", "--controller-name", "x"],
            &["portail", "--readiness-port", "19099"],
            &[
                "portail",
                "--config",
                "c.yaml",
                "controller",
                "--namespace",
                "ps",
            ],
            &[
                "portail",
                "dataplane",
                "--gateway=ns/gw",
                "--controller-address=x",
                "--config=c",
            ],
            &[
                "portail",
                "dataplane",
                "--gateway=ns/gw",
                "--controller-address=x",
                "--transport=direct",
            ],
        ] {
            assert!(Args::try_parse_from(argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn the_baked_image_defaults_to_this_release() {
        if option_env!("PORTAIL_IMAGE").is_none_or(str::is_empty) {
            assert_eq!(
                default_dataplane_image(),
                format!("ghcr.io/epheo/portail:{}", env!("CARGO_PKG_VERSION"))
            );
        }
    }
}
