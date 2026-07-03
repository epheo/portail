//! Example configurations, embedded from `examples/standalone/` at compile
//! time.
//!
//! The committed YAML files are the single source of truth: the CLI's
//! `--generate-config` emits them verbatim, and the round-trip tests in
//! `config/core.rs` parse them through the schema — so drift between the
//! documented examples and the config types fails the test suite instead of
//! silently rotting the docs.

/// `examples/standalone/minimal.yaml` — basic single-service setup.
pub const MINIMAL_YAML: &str = include_str!("../../examples/standalone/minimal.yaml");

/// `examples/standalone/development.yaml` — multi-service development setup:
/// HTTP + HTTPS(Terminate) + TCP listeners, URLRewrite and RequestMirror
/// filters, weighted backends.
pub const DEVELOPMENT_YAML: &str = include_str!("../../examples/standalone/development.yaml");

/// Look up an embedded example by its `--generate-config <name>` CLI name.
pub fn example_yaml(name: &str) -> Option<&'static str> {
    match name {
        "minimal" => Some(MINIMAL_YAML),
        "development" => Some(DEVELOPMENT_YAML),
        _ => None,
    }
}

pub fn print_example_config_info() {
    println!("minimal       - Basic single-service setup with default worker configuration");
    println!(
        "development   - Multi-service development with explicit worker configs (SCP-optimized)"
    );
}
