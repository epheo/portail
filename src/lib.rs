pub mod routing;
pub mod runtime_config;
pub mod config;
pub mod control_plane;
pub mod uring_worker;
pub mod uring_data_plane;
pub mod ebpf;
pub mod backend_pool;
// Removed: pub mod connection; (simplified - no longer needed)

// Re-export native buffer ring types for binary access
pub use uring_worker::native_buffer_ring::NativeBufferRing;

// Re-export backend pool types for binary access
pub use backend_pool::TcpKeepAliveConfig;