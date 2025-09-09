use std::sync::atomic::{AtomicUsize, Ordering};

/// Thread-local buffer pool for zero-allocation request processing
static mut BUFFER_POOL: Option<Vec<Vec<u8>>> = None;
static POOL_SIZE: AtomicUsize = AtomicUsize::new(0);
static REUSE_COUNT: AtomicUsize = AtomicUsize::new(0);
static TOTAL_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// A pooled buffer that reuses memory to avoid allocations in hot paths
pub struct PooledBuffer {
    buffer: Vec<u8>,
}

impl PooledBuffer {
    /// Create a new pooled buffer with the given capacity
    pub fn new(capacity: usize) -> Self {
        TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);
        
        // Try to reuse an existing buffer from the pool
        unsafe {
            if let Some(ref mut pool) = BUFFER_POOL {
                if let Some(mut buffer) = pool.pop() {
                    buffer.clear();
                    buffer.reserve(capacity);
                    REUSE_COUNT.fetch_add(1, Ordering::Relaxed);
                    return Self { buffer };
                }
            }
        }
        
        // Create new buffer if pool is empty
        Self {
            buffer: Vec::with_capacity(capacity),
        }
    }

    /// Copy data into the buffer
    pub fn copy_from_slice(&mut self, data: &[u8]) {
        self.buffer.clear();
        self.buffer.extend_from_slice(data);
    }

    /// Get mutable access to the underlying vector
    pub fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        &mut self.buffer
    }

    /// Get the buffer length
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        // Return buffer to pool for reuse if it's a reasonable size
        if self.buffer.capacity() <= 16384 && self.buffer.capacity() >= 64 {
            unsafe {
                if BUFFER_POOL.is_none() {
                    BUFFER_POOL = Some(Vec::new());
                }
                
                if let Some(ref mut pool) = BUFFER_POOL {
                    if pool.len() < 100 {  // Limit pool size
                        let mut buffer = std::mem::take(&mut self.buffer);
                        buffer.clear();
                        pool.push(buffer);
                        POOL_SIZE.store(pool.len(), Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// Get buffer pool statistics for benchmarking
pub fn get_pool_stats() -> (usize, usize, usize) {
    let pool_size = POOL_SIZE.load(Ordering::Relaxed);
    let reuse_count = REUSE_COUNT.load(Ordering::Relaxed);
    let total_requests = TOTAL_REQUESTS.load(Ordering::Relaxed);
    (pool_size, reuse_count, total_requests)
}

/// Reset buffer pool statistics
pub fn reset_pool_stats() {
    REUSE_COUNT.store(0, Ordering::Relaxed);
    TOTAL_REQUESTS.store(0, Ordering::Relaxed);
}

/// Calculate buffer reuse rate as percentage
pub fn get_reuse_rate() -> f64 {
    let reuse_count = REUSE_COUNT.load(Ordering::Relaxed);
    let total_requests = TOTAL_REQUESTS.load(Ordering::Relaxed);
    
    if total_requests == 0 {
        0.0
    } else {
        (reuse_count as f64 / total_requests as f64) * 100.0
    }
}