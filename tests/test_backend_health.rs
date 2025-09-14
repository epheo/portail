//! Backend health tests using current TcpKeepAliveConfig
//!
//! These tests validate backend connection configuration:
//! - TCP keep-alive settings
//! - Configuration validation
//! - Default value handling

use std::time::Duration;
use uringress::backend_pool::TcpKeepAliveConfig;

#[test]
fn test_tcp_keepalive_config_defaults() {
    let config = TcpKeepAliveConfig::default();
    
    assert!(config.enabled);
    assert_eq!(config.idle_time, Duration::from_secs(600));
    assert_eq!(config.probe_interval, Duration::from_secs(60));
    assert_eq!(config.probe_count, 3);
}

#[test]
fn test_tcp_keepalive_config_custom() {
    let config = TcpKeepAliveConfig {
        enabled: true,
        idle_time: Duration::from_secs(30),
        probe_interval: Duration::from_secs(5),
        probe_count: 9,
    };
    
    assert!(config.enabled);
    assert_eq!(config.idle_time, Duration::from_secs(30));
    assert_eq!(config.probe_interval, Duration::from_secs(5));
    assert_eq!(config.probe_count, 9);
}

#[test]
fn test_tcp_keepalive_config_disabled() {
    let config = TcpKeepAliveConfig {
        enabled: false,
        idle_time: Duration::from_secs(600),
        probe_interval: Duration::from_secs(60),
        probe_count: 3,
    };
    
    assert!(!config.enabled);
    assert_eq!(config.idle_time, Duration::from_secs(600));
    assert_eq!(config.probe_interval, Duration::from_secs(60));
    assert_eq!(config.probe_count, 3);
}

#[test]
fn test_tcp_keepalive_config_variations() {
    // Test various TCP keep-alive configurations
    let configs = vec![
        TcpKeepAliveConfig {
            enabled: false,
            idle_time: Duration::from_secs(60),
            probe_interval: Duration::from_secs(30),
            probe_count: 3,
        },
        TcpKeepAliveConfig {
            enabled: true,
            idle_time: Duration::from_secs(10),
            probe_interval: Duration::from_secs(5),
            probe_count: 5,
        },
    ];
    
    for (i, config) in configs.into_iter().enumerate() {
        // All configurations should be valid and constructible
        if i == 0 {
            assert!(!config.enabled);
        } else {
            assert!(config.enabled);
        }
    }
}