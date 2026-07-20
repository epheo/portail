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

/// Every embedded example as (CLI name, YAML, blurb). Single source for the
/// clap value_parser, `--generate-config` lookup, and `--example-config`
/// listing, so adding an example is one entry here.
pub const EXAMPLES: &[(&str, &str, &str)] = &[
    (
        "minimal",
        MINIMAL_YAML,
        "Basic single-service setup with default worker configuration",
    ),
    (
        "development",
        DEVELOPMENT_YAML,
        "Multi-service development with explicit worker configs (SCP-optimized)",
    ),
];

/// Look up an embedded example by its `--generate-config <name>` CLI name.
pub fn example_yaml(name: &str) -> Option<&'static str> {
    EXAMPLES
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, yaml, _)| *yaml)
}

pub fn print_example_config_info() {
    for (name, _, blurb) in EXAMPLES {
        println!("{:<14}- {}", name, blurb);
    }
}
