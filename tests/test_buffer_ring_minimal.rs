//! Minimal Buffer Ring Test
//!
//! This test isolates the buffer ring ENOBUFS issue from the complex UringRess architecture.
//! Tests the exact same buffer ring initialization and read operation sequence to reproduce
//! the ENOBUFS error in a controlled environment.

use anyhow::{anyhow, Result};
use io_uring::{opcode, types, IoUring};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use tracing::{debug, error, info};

use uringress::uring_worker::native_buffer_ring::NativeBufferRing;

#[tokio::test]
async fn test_buffer_ring_minimal_reproduction() -> Result<()> {
    // Initialize tracing for debug output
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init()
        .ok();

    info!("Starting minimal buffer ring ENOBUFS reproduction test");

    // Test the exact same parameters as our failing worker
    test_buffer_ring_with_params(1000, 256, 8192).await?;

    Ok(())
}

#[tokio::test]
async fn test_buffer_ring_different_params() -> Result<()> {
    // Initialize tracing for debug output
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init()
        .ok();

    info!("Testing buffer ring with different parameters");

    // Test with smaller, simpler parameters
    test_buffer_ring_with_params(1, 16, 4096).await?;
    test_buffer_ring_with_params(2, 32, 8192).await?;
    test_buffer_ring_with_params(0, 1, 1024).await?; // Minimal case

    Ok(())
}

async fn test_buffer_ring_with_params(
    buffer_group_id: u16, 
    buffer_count: u16, 
    buffer_size: u32
) -> Result<()> {
    info!("Testing buffer ring: group_id={}, count={}, size={}", 
          buffer_group_id, buffer_count, buffer_size);

    // Step 1: Create IoUring instance (same as worker)
    let mut ring = IoUring::new(1024)?;
    info!("✓ IoUring created successfully");

    // Step 2: Create and register native buffer ring (exact same sequence)
    let mut buffer_ring = NativeBufferRing::new(buffer_group_id, buffer_count, buffer_size);
    buffer_ring.register_with_kernel(&mut ring)?;
    info!("✓ Buffer ring registered with kernel: group_id={}", buffer_group_id);

    // Step 3: Create a simple socket pair for testing
    let (read_fd, write_fd) = create_socket_pair()?;
    info!("✓ Socket pair created: read_fd={}, write_fd={}", read_fd, write_fd);

    // Step 4: Write some test data to trigger a read operation
    let test_data = b"Hello, buffer ring test!";
    write_test_data(write_fd, test_data)?;
    info!("✓ Test data written: {} bytes", test_data.len());

    // Step 5: Submit read operation with BUFFER_SELECT (exact same as worker)
    let user_data = 42u64;
    let read_entry = build_read_with_buffer_select_op(
        read_fd,
        buffer_group_id,
        buffer_size,
        user_data,
    );
    
    debug!("Submitting read with buffer select: fd={}, group={}, size={}, user_data={}", 
           read_fd, buffer_group_id, buffer_size, user_data);

    unsafe {
        if ring.submission().push(&read_entry).is_err() {
            return Err(anyhow!("Failed to push read operation to submission queue"));
        }
    }

    // Step 6: Submit and wait for completion (exact same as worker)
    ring.submit_and_wait(1)?;
    info!("✓ Read operation submitted and waiting for completion");

    // Step 7: Process completion and check for ENOBUFS
    let mut completion = ring.completion();
    completion.sync();

    for cqe in completion {
        let completed_user_data = cqe.user_data();
        let result = cqe.result();
        let flags = cqe.flags();

        info!("Completion received: user_data={}, result={}, flags={:#x}", 
              completed_user_data, result, flags);

        if completed_user_data == user_data {
            if result < 0 {
                error!("❌ ENOBUFS ERROR REPRODUCED: result={}, flags={:#x}", result, flags);
                
                if result == -105 {
                    error!("This is the exact ENOBUFS error we're seeing in the worker!");
                    error!("Buffer group {} reports no available buffers despite successful registration", 
                           buffer_group_id);
                }
                
                return Err(anyhow!("Read operation failed with error: {}", result));
            } else {
                info!("✅ SUCCESS: Read completed with {} bytes", result);
                
                // Check if buffer ID was provided
                if flags != 0 {
                    let buffer_id = (flags >> 16) as u16;
                    info!("✓ Buffer ID extracted from completion: {}", buffer_id);
                    
                    // Test buffer retrieval
                    if let Some(borrowed_buffer) = buffer_ring.get_buffer(buffer_id, result as usize) {
                        info!("✓ Buffer retrieved successfully: id={}, length={}", 
                              buffer_id, result);
                        
                        // Verify data was read correctly
                        let buffer_data = &borrowed_buffer[..result as usize];
                        if buffer_data == test_data {
                            info!("✅ Data verification passed: buffer contains expected data");
                        } else {
                            error!("❌ Data verification failed: buffer data mismatch");
                        }
                    } else {
                        error!("❌ Failed to retrieve buffer despite successful read");
                    }
                } else {
                    error!("❌ No buffer ID in completion flags - this indicates a problem");
                }
            }
            break;
        }
    }

    // Cleanup
    close_socket_pair(read_fd, write_fd)?;
    buffer_ring.release(&mut ring)?;
    info!("✓ Test completed for group_id={}", buffer_group_id);

    Ok(())
}

fn build_read_with_buffer_select_op(
    fd: RawFd,
    buffer_group: u16,
    buffer_size: u32,
    user_data: u64,
) -> io_uring::squeue::Entry {
    opcode::Read::new(
        types::Fd(fd),
        std::ptr::null_mut(),
        buffer_size,
    )
    .buf_group(buffer_group)
    .build()
    .flags(io_uring::squeue::Flags::BUFFER_SELECT)
    .user_data(user_data)
}

fn create_socket_pair() -> Result<(RawFd, RawFd)> {
    use std::os::unix::io::AsRawFd;
    
    // Create a Unix domain socket pair for testing
    let (stream1, stream2) = UnixStream::pair()?;
    let read_fd = stream1.as_raw_fd();
    let write_fd = stream2.as_raw_fd();
    
    // Prevent the streams from being dropped (which would close the FDs)
    std::mem::forget(stream1);
    std::mem::forget(stream2);
    
    Ok((read_fd, write_fd))
}

fn write_test_data(write_fd: RawFd, data: &[u8]) -> Result<()> {
    use std::os::unix::io::FromRawFd;
    use std::io::Write;
    
    // Temporarily create a Write object to send data
    let mut writer = unsafe { std::fs::File::from_raw_fd(write_fd) };
    writer.write_all(data)?;
    writer.flush()?;
    
    // Prevent the File from being dropped (which would close the FD)
    std::mem::forget(writer);
    
    Ok(())
}

fn close_socket_pair(read_fd: RawFd, write_fd: RawFd) -> Result<()> {
    use std::os::unix::io::FromRawFd;
    
    // Create File objects and let them drop to close the FDs
    unsafe {
        let _read_file = std::fs::File::from_raw_fd(read_fd);
        let _write_file = std::fs::File::from_raw_fd(write_fd);
        // Files will be automatically closed when they drop
    }
    
    Ok(())
}

#[tokio::test]
async fn test_buffer_ring_state_validation() -> Result<()> {
    // Initialize tracing for debug output
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init()
        .ok();

    info!("Testing buffer ring state validation");

    // Create buffer ring with small parameters for easier debugging
    let buffer_group_id = 100u16;
    let buffer_count = 4u16;
    let buffer_size = 1024u32;

    let mut ring = IoUring::new(64)?;
    let mut buffer_ring = NativeBufferRing::new(buffer_group_id, buffer_count, buffer_size);
    
    info!("Buffer ring created: group_id={}, count={}, size={}", 
          buffer_group_id, buffer_count, buffer_size);

    // Register and validate internal state
    buffer_ring.register_with_kernel(&mut ring)?;
    info!("Buffer ring registered successfully");

    // Test buffer group ID consistency
    let retrieved_group_id = buffer_ring.get_buffer_group_id();
    assert_eq!(retrieved_group_id, buffer_group_id, 
               "Buffer group ID mismatch: expected {}, got {}", 
               buffer_group_id, retrieved_group_id);
    info!("✓ Buffer group ID consistency verified: {}", retrieved_group_id);

    // Test buffer size consistency
    let retrieved_buffer_size = buffer_ring.get_buffer_size();
    assert_eq!(retrieved_buffer_size, buffer_size,
               "Buffer size mismatch: expected {}, got {}", 
               buffer_size, retrieved_buffer_size);
    info!("✓ Buffer size consistency verified: {}", retrieved_buffer_size);

    info!("✅ Buffer ring state validation completed successfully");
    Ok(())
}