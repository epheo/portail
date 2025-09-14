//! User Data Bit Manipulation Module
//!
//! This module provides a clean API for managing the 64-bit user_data layout used throughout
//! the io_uring worker pipeline. All functions are marked #[inline(always)] to ensure
//! zero runtime overhead while providing better code organization.

use std::os::unix::io::RawFd;
use crate::uring_worker::unified_http_parser::ConnectionType;

// =====================================================================================
// ====                    64-BIT USER_DATA LAYOUT ARCHITECTURE                     ====
// =====================================================================================
// Ultra-simplified layout eliminates bit overlap corruption and reduces complexity.
// Ownership logic distinguishes operations: Worker (client), IoUringRead (backend).
// All essential data encoded in 64 bits with realistic capacity limits and 2 spare bits.
// =====================================================================================
//
// Bits 0-19:   Client FD (20 bits)     - 1M max FDs (realistic maximum)
// Bits 20-28:  Pool Index (9 bits)     - 512 backend pools (expanded from 8 bits)
// Bit 29:      Keep-alive (1 bit)      - Connection state
// Bits 30-45:  Backend FD (16 bits)    - 65K backend connections
// Bits 46-57:  Server Port (12 bits)   - 4096 max port (enough for listeners)
// Bit 58:      Connection Owner (1 bit) - Universal ownership tracking
// Bit 59:      SENDZC Flag (1 bit)     - SENDZC operation identification
// Bits 60-63:  Operation Flags (4 bits) - TIMEOUT/CLIENT_OP/BACKEND_CONNECT/ACCEPT
//
// =====================================================================================

// Operation flags (bits 59-63) - io_uring operation dispatch
pub(crate) const ACCEPT_FLAG: u64       = 1u64 << 63;  // Bit 63 - Accept operations
pub(crate) const BACKEND_CONNECT_FLAG: u64 = 1u64 << 62;  // Bit 62 - Backend connect operations
pub(crate) const CLIENT_OP_FLAG: u64    = 1u64 << 61;  // Bit 61 - Client operations (vs backend operations)
pub(crate) const TIMEOUT_FLAG: u64      = 1u64 << 60;  // Bit 60 - Timeout operations
pub(crate) const POOL_MONITOR_FLAG: u64 = 1u64 << 59;  // Bit 59 - Pool connection monitoring (was SENDZC, now in io_operations)

// Connection ownership (bit 58) - Universal ownership tracking  
const OWNER_SHIFT: u64 = 58;
const OWNER_MASK: u64 = 0x1;                // 1 bit = 2 ownership states


// Backend pool index (bits 20-27: 8 bits = 256 pools) - reduced to make room for pooling bit  
const POOL_INDEX_SHIFT: u64 = 20;
const POOL_INDEX_MASK: u64 = 0xFF;             // 8 bits = 256 pools

// Backend pooling flag (bit 28) - indicates if backend should be returned to pool after sendzc
pub(crate) const BACKEND_POOLING_BIT: u64 = 1u64 << 28;   // Bit 28 - Backend pooling enabled

// Keep-alive flag (bit 29)
pub(crate) const KEEPALIVE_ENABLED: u64 = 1u64 << 29;     // Bit 29 - Keep-alive enabled

// Backend FD (bits 30-45: 16 bits = 65K backend connections)
const BACKEND_FD_SHIFT: u64 = 30;
const BACKEND_FD_MASK: u64 = 0xFFFF;            // 16 bits = 65K backend connections

// Server Port (bits 46-57: 12 bits = 4096 max port)
const SERVER_PORT_SHIFT: u64 = 46;
const SERVER_PORT_MASK: u64 = 0xFFF;            // 12 bits = 4096 max port value

// Client FD (bits 0-19: 20 bits = 1M max FDs)
const CLIENT_FD_SHIFT: u64 = 0;
const CLIENT_FD_MASK: u64 = 0xFFFFF;            // 20 bits = 1M concurrent clients

/// Universal connection ownership states for race-free lifecycle management
/// Embedded in user_data bit 58 for zero-overhead ownership tracking
/// Reduced to 2 states after SENDZC moved to bit 59
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionOwner {
    Worker = 0,         // Worker thread owns connection (initial state, includes cleanup)
    IoUringRead = 1,    // io_uring read operation owns connection
    // Note: Cleanup merged with Worker state since SENDZC uses Worker ownership
}

impl ConnectionOwner {
    #[inline(always)]
    fn from_bits(bits: u64) -> Self {
        match (bits >> OWNER_SHIFT) & OWNER_MASK {
            0 => ConnectionOwner::Worker,
            1 => ConnectionOwner::IoUringRead,
            _ => unreachable!(),
        }
    }
}

/// Encode basic connection data into user_data format (without server port)
#[inline(always)]
pub(crate) fn encode_connection_data(client_fd: RawFd, pool_index: u8, backend_fd: RawFd) -> u64 {
    encode_connection_data_with_port(client_fd, pool_index, backend_fd, 0)
}

/// Encode connection data with server port into user_data format
#[inline(always)]
pub(crate) fn encode_connection_data_with_port(client_fd: RawFd, pool_index: u8, backend_fd: RawFd, server_port: u16) -> u64 {
    let mut user_data = 0u64;
    
    // Client FD (bits 0-19)
    user_data |= (client_fd as u64 & CLIENT_FD_MASK) << CLIENT_FD_SHIFT;
    
    // Pool index (bits 20-28)
    user_data |= (pool_index as u64 & POOL_INDEX_MASK) << POOL_INDEX_SHIFT;
    
    // Backend FD (bits 30-45) 
    user_data |= (backend_fd as u64 & BACKEND_FD_MASK) << BACKEND_FD_SHIFT;
    
    // Server port (bits 46-57)
    user_data |= (server_port as u64 & SERVER_PORT_MASK) << SERVER_PORT_SHIFT;
    
    user_data
}

/// Set keepalive bit based on HTTP Connection header
#[inline(always)]
pub(crate) fn set_keepalive_bit(user_data: u64, connection_type: &ConnectionType) -> u64 {
    match connection_type {
        ConnectionType::KeepAlive => user_data | KEEPALIVE_ENABLED,
        ConnectionType::Close => user_data & !KEEPALIVE_ENABLED,
        ConnectionType::Default => user_data | KEEPALIVE_ENABLED, // Default to keep-alive for HTTP/1.1
    }
}

/// Check if keepalive is enabled for this connection
#[inline(always)]
pub(crate) fn is_keepalive_enabled(user_data: u64) -> bool {
    (user_data & KEEPALIVE_ENABLED) != 0
}

/// Extract client file descriptor from user_data
#[inline(always)]
pub(crate) fn extract_client_fd(user_data: u64) -> RawFd {
    ((user_data >> CLIENT_FD_SHIFT) & CLIENT_FD_MASK) as RawFd
}

/// Extract backend file descriptor from user_data  
#[inline(always)]
pub(crate) fn extract_backend_fd(user_data: u64) -> RawFd {
    ((user_data >> BACKEND_FD_SHIFT) & BACKEND_FD_MASK) as RawFd
}

/// Extract pool index from user_data
#[inline(always)]
pub(crate) fn extract_pool_index(user_data: u64) -> u8 {
    ((user_data >> POOL_INDEX_SHIFT) & POOL_INDEX_MASK) as u8
}


/// Extract server port from user_data
#[inline(always)]
pub(crate) fn extract_server_port(user_data: u64) -> u16 {
    ((user_data >> SERVER_PORT_SHIFT) & SERVER_PORT_MASK) as u16
}

/// Extract connection owner from user_data
#[inline(always)]
pub(crate) fn extract_owner(user_data: u64) -> ConnectionOwner {
    ConnectionOwner::from_bits(user_data)
}

/// Extract connection owner from user_data (alias for compatibility)
#[inline(always)]
pub(crate) fn extract_connection_owner(user_data: u64) -> ConnectionOwner {
    extract_owner(user_data)
}

/// Check if keep-alive should continue for connection
#[inline(always)]
pub(crate) fn should_continue_connection(user_data: u64) -> bool {
    is_keepalive_enabled(user_data)
}

/// Mark user_data as accept operation
#[inline(always)]
pub(crate) fn mark_as_accept_operation(user_data: u64) -> u64 {
    user_data | ACCEPT_FLAG
}

/// Check if user_data represents an accept operation
#[inline(always)]
pub(crate) fn is_accept_operation(user_data: u64) -> bool {
    (user_data & ACCEPT_FLAG) != 0
}


/// Single codepath connection cleanup check
#[inline(always)]
pub(crate) fn should_cleanup_connection(user_data: u64) -> bool {
    extract_connection_owner(user_data) == ConnectionOwner::Worker
}


/// Encode pool monitor operation user_data
/// Used to track when backend connections close while in pool
#[inline(always)]
pub(crate) fn encode_pool_monitor(fd: RawFd, backend_port: u16) -> u64 {
    // Encode fd in client_fd position (bits 0-19)
    // Encode backend port for identification (bits 46-57)
    // Set POOL_MONITOR_FLAG
    let mut user_data = (fd as u64) & CLIENT_FD_MASK;
    user_data |= ((backend_port as u64) & SERVER_PORT_MASK) << SERVER_PORT_SHIFT;
    user_data |= POOL_MONITOR_FLAG;
    user_data
}

/// Decode monitored fd from pool monitor user_data
#[inline(always)]
pub(crate) fn extract_monitored_fd(user_data: u64) -> RawFd {
    (user_data & CLIENT_FD_MASK) as RawFd
}

/// Decode backend port from pool monitor user_data
#[inline(always)]
pub(crate) fn extract_monitored_port(user_data: u64) -> u16 {
    ((user_data >> SERVER_PORT_SHIFT) & SERVER_PORT_MASK) as u16
}

/// Check if user_data represents a pool monitor operation
#[inline(always)]
pub(crate) fn is_pool_monitor(user_data: u64) -> bool {
    (user_data & POOL_MONITOR_FLAG) != 0
}

/// Set backend pooling bit to indicate connection should be returned to pool after sendzc
#[inline(always)]
pub(crate) fn set_backend_pooling_bit(user_data: u64) -> u64 {
    user_data | BACKEND_POOLING_BIT
}

/// Check if backend connection should be pooled after sendzc completion
#[inline(always)]
pub(crate) fn is_backend_pooling_enabled(user_data: u64) -> bool {
    (user_data & BACKEND_POOLING_BIT) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keepalive_preservation() {
        let base = encode_connection_data(123, 0, 0);
        let with_close = set_keepalive_bit(base, &ConnectionType::Close);
        assert!(!is_keepalive_enabled(with_close), "Connection: close should disable keepalive");
        
        let with_keepalive = set_keepalive_bit(base, &ConnectionType::KeepAlive);
        assert!(is_keepalive_enabled(with_keepalive), "Connection: keep-alive should enable keepalive");
    }

    #[test]
    fn test_bit_operations_comprehensive() {
        let base = encode_connection_data(123, 0, 456);
        let with_keepalive = set_keepalive_bit(base, &ConnectionType::KeepAlive);
        
        assert_eq!(extract_client_fd(with_keepalive), 123);
        assert_eq!(extract_backend_fd(with_keepalive), 456);
        assert!(is_keepalive_enabled(with_keepalive));
    }

    #[test]
    fn test_connection_close_flow() {
        let initial = encode_connection_data(123, 0, 0);
        let with_close = set_keepalive_bit(initial, &ConnectionType::Close);
        
        assert!(!is_keepalive_enabled(with_close), "Connection: close should disable keepalive");
        assert_eq!(extract_client_fd(with_close), 123);
        assert_eq!(extract_owner(with_close), ConnectionOwner::Worker);
    }

    #[test]
    fn test_ownership_extraction() {
        let initial = encode_connection_data(123, 2, 0);
        assert_eq!(extract_owner(initial), ConnectionOwner::Worker);
        assert_eq!(extract_connection_owner(initial), ConnectionOwner::Worker);
    }

    #[test]
    fn test_accept_operations() {
        let base = encode_connection_data(456, 0, 0);
        let accept_op = mark_as_accept_operation(base);
        
        assert!(is_accept_operation(accept_op), "Should be marked as accept operation");
        assert_eq!(extract_client_fd(accept_op), 456);
        assert_ne!(accept_op & ACCEPT_FLAG, 0, "ACCEPT_FLAG should be set");
    }

    #[test]
    fn test_backend_fd_encoding_decoding() {
        // Test basic backend FD encoding/decoding
        let client_fd = 329;
        let backend_fd = 429;
        let pool_index = 0;
        
        let user_data = encode_connection_data(client_fd, pool_index, backend_fd);
        
        assert_eq!(extract_client_fd(user_data), client_fd);
        assert_eq!(extract_backend_fd(user_data), backend_fd);
        assert_eq!(extract_pool_index(user_data), pool_index);
    }

    #[test]
    fn test_sendzc_encoding_preserves_backend_fd() {
        use crate::uring_worker::io_operations::BufferRingOperations;
        
        let client_fd = 329;
        let backend_fd = 429;
        let pool_index = 0;
        
        let original_data = encode_connection_data(client_fd, pool_index, backend_fd);
        let write_data = original_data; // No ownership transfer needed
        
        // Encode for SENDZC
        let sendzc_data = BufferRingOperations::encode_sendzc_user_data(write_data);
        
        // Decode back from SENDZC
        let decoded_data = BufferRingOperations::decode_original_from_sendzc(sendzc_data);
        
        assert_eq!(extract_backend_fd(decoded_data), backend_fd, "Backend FD should survive SENDZC encoding/decoding");
        assert_eq!(extract_client_fd(decoded_data), client_fd, "Client FD should survive SENDZC encoding/decoding");
    }

    #[test]
    fn test_real_log_value_backend_fd_extraction() {
        // Test with actual user_data value from logs: 0400006b60000149
        let user_data = 0x0400006b60000149u64;
        
        let client_fd = extract_client_fd(user_data);
        let backend_fd = extract_backend_fd(user_data);
        let pool_index = extract_pool_index(user_data);
        let owner = extract_owner(user_data);
        
        assert_eq!(client_fd, 329, "Should extract client_fd=329 from real log value");
        assert_eq!(backend_fd, 429, "Should extract backend_fd=429 from real log value");
        assert_eq!(pool_index, 0, "Should extract pool_index=0 from real log value");
        assert_eq!(owner, ConnectionOwner::IoUringRead, "Should extract IoUringRead owner from real log value");
        
        println!("Real log user_data 0x{:016x} correctly decoded: client_fd={}, backend_fd={}, pool_index={}, owner={:?}", 
                 user_data, client_fd, backend_fd, pool_index, owner);
    }
}