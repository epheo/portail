use std::time::Duration;

/// Zero-runtime configuration struct following KISS server pattern
/// All fields are accessed directly for true zero overhead
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    // Connection Management (direct field access - zero overhead)
    pub connection_pool_size: usize,
    pub max_requests_per_connection: u32,
    
    // Buffer Configuration
    pub standard_buffer_size: usize,
    pub large_buffer_size: usize,
    pub header_buffer_size: usize,
    
    // Pre-computed Duration objects (created once at startup)
    pub connection_max_age: Duration,
    pub backend_response_timeout: Duration,
    pub circuit_breaker_timeout: Duration,
    pub keep_alive_timeout: Duration,
    
    // Circuit Breaker Configuration
    pub circuit_breaker_failure_threshold: u32,
    
    // Retry Configuration
    pub max_retries: u32,
}

impl RuntimeConfig {
    /// Create RuntimeConfig from direct values - zero runtime overhead
    /// All environment variable parsing should be done once at startup
    pub fn new(
        connection_pool_size: usize,
        max_requests_per_connection: u32,
        standard_buffer_size: usize,
        large_buffer_size: usize,
        header_buffer_size: usize,
        connection_max_age: Duration,
        backend_response_timeout: Duration,
        circuit_breaker_timeout: Duration,
        keep_alive_timeout: Duration,
        circuit_breaker_failure_threshold: u32,
        max_retries: u32,
    ) -> Self {
        Self {
            connection_pool_size,
            max_requests_per_connection,
            standard_buffer_size,
            large_buffer_size,
            header_buffer_size,
            connection_max_age,
            backend_response_timeout,
            circuit_breaker_timeout,
            keep_alive_timeout,
            circuit_breaker_failure_threshold,
            max_retries,
        }
    }

}