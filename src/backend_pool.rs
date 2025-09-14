//! TCP Keep-Alive Configuration for Backend Connections
//!
//! This module provides configuration for TCP keep-alive settings used by
//! thread-local backend connection pools in UringWorker instances.

use std::time::Duration;

/// TCP Keep-Alive configuration
#[derive(Debug, Clone)]
pub struct TcpKeepAliveConfig {
    /// Enable TCP keep-alive
    pub enabled: bool,
    /// Time before sending first keep-alive probe
    pub idle_time: Duration,
    /// Interval between keep-alive probes
    pub probe_interval: Duration,
    /// Number of failed probes before considering connection dead
    pub probe_count: u32,
}

impl Default for TcpKeepAliveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_time: Duration::from_secs(600),    // 10 minutes
            probe_interval: Duration::from_secs(60), // 1 minute
            probe_count: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_tcp_keepalive_config_default() {
        let config = TcpKeepAliveConfig::default();
        assert!(config.enabled);
        assert_eq!(config.idle_time, Duration::from_secs(600));
        assert_eq!(config.probe_interval, Duration::from_secs(60));
        assert_eq!(config.probe_count, 3);
    }
}