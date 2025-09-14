use std::collections::HashMap;

/// Simple listener info for zero-overhead port-based routing
#[derive(Debug, Clone)]
pub struct ListenerInfo {
    pub protocol: Protocol,
    pub address: Option<std::net::IpAddr>,
    pub interface: Option<String>,
}

/// Protocol types for runtime use (simplified from config::Protocol)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    HTTP,
    HTTPS,
    TCP,
    UDP,
}

/// Zero-runtime configuration struct following KISS server pattern
/// All fields are accessed directly for true zero overhead
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    // Gateway Configuration (zero overhead listener lookup)
    pub listeners: HashMap<u16, ListenerInfo>,
    
}

impl RuntimeConfig {
    /// Create RuntimeConfig from direct values - zero runtime overhead
    /// All environment variable parsing should be done once at startup
    pub fn new(
        listeners: HashMap<u16, ListenerInfo>,
    ) -> Self {
        Self {
            listeners,
        }
    }


}