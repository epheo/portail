//! Native io_uring Buffer Ring Implementation
//! 
//! This module provides core kernel buffer ring management using io_uring's native buffer ring functionality.
//! HTTP/TCP processing is handled by the modular uring_worker components.
//! 
//! Key features:
//! - Uses IORING_REGISTER_PBUF_RING for kernel-managed buffer rings
//! - Provides safe BorrowedBuffer lifecycle management
//! - Zero-overhead buffer selection and validation
//! - Eliminates manual buffer tracking and associated race conditions

use anyhow::{anyhow, Result};
use io_uring::IoUring;
use io_uring_buf_ring::{IoUringBufRing, BorrowedBuffer};
use tracing::info;


/// Native io_uring buffer ring implementation using IORING_REGISTER_PBUF_RING
/// Eliminates legacy ProvideBuffers overhead and EFAULT errors
/// Provides kernel-managed buffer tracking for true zero-copy operations
pub struct NativeBufferRing {
    buffer_group_id: u16,
    buffer_count: u16,
    buffer_size: u32,
    // Native io_uring buffer ring managed by kernel
    ring: Option<IoUringBufRing<Vec<u8>>>,
}

impl NativeBufferRing {
    /// Create a new native buffer ring with specified parameters
    pub fn new(buffer_group_id: u16, buffer_count: u16, buffer_size: u32) -> Self {
        Self {
            buffer_group_id,
            buffer_count,
            buffer_size,
            ring: None,
        }
    }
    
    /// Register native buffer ring with kernel using IORING_REGISTER_PBUF_RING
    /// Eliminates all ProvideBuffers fallbacks and EFAULT errors
    pub fn register_with_kernel(&mut self, ring: &mut IoUring) -> Result<()> {
        if self.ring.is_some() {
            return Ok(());
        }
        
        info!("Initializing native io_uring buffer ring: group_id={}, count={}, size={}", 
              self.buffer_group_id, self.buffer_count, self.buffer_size);
        
        // Create native buffer ring using IORING_REGISTER_PBUF_RING
        let buffer_ring = IoUringBufRing::new(
            ring,
            self.buffer_count,
            self.buffer_group_id,
            self.buffer_size as usize,
        ).map_err(|e| anyhow!("Failed to create native buffer ring: {}", e))?;
        
        self.ring = Some(buffer_ring);
        
        info!("Successfully registered native buffer ring: group_id={}, count={}, size={}", 
              self.buffer_group_id, self.buffer_count, self.buffer_size);
        Ok(())
    }
    
    /// Get buffer group ID for IOSQE_BUFFER_SELECT operations
    pub fn get_buffer_group_id(&self) -> u16 {
        self.buffer_group_id
    }
    
    /// Get buffer size for operations
    pub fn get_buffer_size(&self) -> u32 {
        self.buffer_size
    }
    
    
    /// Get raw buffer pointer by ID from native buffer ring
    /// Returns raw pointer for zero-copy operations without wrapper overhead
    /// SAFETY: Caller must ensure buffer is not released while pointer is in use
    #[inline(always)]
    pub unsafe fn get_buffer_ptr(&self, buffer_id: u16, length: usize) -> Option<(*const u8, usize)> {
        if let Some(ref ring) = self.ring {
            // Get BorrowedBuffer temporarily just to extract the pointer
            // This is suboptimal but necessary until we have direct access
            if let Some(buffer) = ring.get_buf(buffer_id, length) {
                let ptr = buffer.as_ptr();
                let len = buffer.len();
                // Buffer will be dropped here, but pointer remains valid
                // because kernel still owns the actual memory
                return Some((ptr, len));
            }
        }
        None
    }
    
    /// Get buffer by ID from native buffer ring (for debugging only)
    /// Returns buffer from kernel-managed ring for given buffer_id
    pub fn get_buffer(&self, buffer_id: u16, length: usize) -> Option<BorrowedBuffer<'_, Vec<u8>>> {
        if let Some(ref ring) = self.ring {
            unsafe { ring.get_buf(buffer_id, length) }
        } else {
            None
        }
    }
    
    /// Release buffer ring and unregister from kernel
    /// Should be called when worker shuts down to prevent resource leaks
    pub fn release(mut self, ring: &mut IoUring) -> Result<()> {
        if let Some(buffer_ring) = self.ring.take() {
            unsafe {
                buffer_ring.release(ring)
                    .map_err(|e| anyhow!("Failed to release buffer ring: {}", e))?;
            }
            info!("Successfully released buffer ring: group_id={}", self.buffer_group_id);
        }
        Ok(())
    }
}
