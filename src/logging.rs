//! Centralized logging module
//!
//! error!, warn!, and info! are always active.
//! debug!/trace! become no-ops in release builds.

use anyhow::Result;
use std::fs::OpenOptions;
use std::path::Path;

pub use tracing::{error, warn, info};

#[cfg(debug_assertions)]
pub use tracing::debug;
#[cfg(debug_assertions)]
#[allow(unused_imports)]
pub use tracing::trace;

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! logging_debug {
    ($($arg:tt)*) => {()};
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! logging_trace {
    ($($arg:tt)*) => {()};
}

#[cfg(not(debug_assertions))]
pub use logging_debug as debug;
#[cfg(not(debug_assertions))]
#[allow(unused_imports)]
pub use logging_trace as trace;

fn create_log_file_writer(path: &str) -> Result<std::fs::File> {
    let log_path = Path::new(path);

    if let Some(parent) = log_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create log directory {}: {}", parent.display(), e))?;
        }
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| anyhow::anyhow!("Failed to open log file {}: {}", path, e))
}

/// Initialize logging. If a LoggingConfig is provided, uses its settings for format/output/level.
/// CLI verbose flag always overrides the config level.
pub fn init_logging(verbose_level: u8, config: Option<&crate::config::LoggingConfig>) {
    use tracing_subscriber::{fmt, EnvFilter};
    use crate::config::{LogLevel, LogFormat, LogOutput};

    let level_str = if let Some(logging_config) = config {
        if verbose_level > 0 {
            match verbose_level {
                1 => "debug",
                _ => "trace",
            }
        } else {
            match logging_config.level {
                LogLevel::Error => "error",
                LogLevel::Warn => "warn",
                LogLevel::Info => "info",
                LogLevel::Debug => "debug",
                LogLevel::Trace => "trace",
            }
        }
    } else {
        #[cfg(debug_assertions)]
        {
            match verbose_level {
                0 => "info",
                1 => "debug",
                _ => "trace",
            }
        }
        #[cfg(not(debug_assertions))]
        {
            match verbose_level {
                0 => "warn",
                1 => "info",
                2 => "debug",
                _ => "trace",
            }
        }
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level_str));

    let show_details = if let Some(lc) = config {
        verbose_level > 1 || matches!(lc.level, LogLevel::Debug | LogLevel::Trace)
    } else {
        verbose_level > 1
    };

    if let Some(logging_config) = config {
        macro_rules! init_subscriber {
            ($subscriber:expr, $output:expr) => {
                match $output {
                    LogOutput::Stdout => $subscriber.init(),
                    LogOutput::Stderr => $subscriber.with_writer(std::io::stderr).init(),
                    LogOutput::File(path) => {
                        match create_log_file_writer(path) {
                            Ok(file) => $subscriber.with_writer(file).init(),
                            Err(e) => {
                                eprintln!("Fatal: Failed to create log file '{}': {}", path, e);
                                std::process::exit(1);
                            }
                        }
                    }
                }
            };
        }

        match logging_config.format {
            LogFormat::Json => {
                let s = fmt().with_env_filter(filter).with_target(true).json();
                init_subscriber!(s, &logging_config.output);
            }
            LogFormat::Compact => {
                let mut s = fmt().with_env_filter(filter).with_target(true).compact();
                if show_details {
                    s = s.with_file(true).with_line_number(true);
                }
                init_subscriber!(s, &logging_config.output);
            }
            LogFormat::Pretty => {
                let mut s = fmt().with_env_filter(filter).with_target(true).pretty();
                if show_details {
                    s = s.with_file(true).with_line_number(true);
                }
                init_subscriber!(s, &logging_config.output);
            }
        }
    } else if verbose_level == 0 && cfg!(not(debug_assertions)) {
        fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
    } else {
        fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_file(show_details)
            .with_line_number(show_details)
            .init();
    }
}
