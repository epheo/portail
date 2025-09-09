use std::collections::HashMap;

/// Simple listener info for zero-overhead protocol detection
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
    
    // Buffer Configuration (only used fields)
    pub standard_buffer_size: usize,
}

impl RuntimeConfig {
    /// Create RuntimeConfig from direct values - zero runtime overhead
    /// All environment variable parsing should be done once at startup
    pub fn new(
        listeners: HashMap<u16, ListenerInfo>,
        standard_buffer_size: usize,
    ) -> Self {
        Self {
            listeners,
            standard_buffer_size,
        }
    }


    /// Get protocol for a port - designed for hot path performance
    #[inline(always)]
    pub fn get_protocol(&self, port: u16) -> Option<&Protocol> {
        self.listeners.get(&port).map(|listener| &listener.protocol)
    }
}