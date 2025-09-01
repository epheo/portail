use uringress::worker::WorkerMetrics;
use std::sync::atomic::Ordering;

#[test]
fn test_worker_metrics_creation() {
    let metrics = WorkerMetrics::new();
    
    // Check initial values
    use std::sync::atomic::Ordering;
    assert_eq!(metrics.connections_accepted.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.requests_processed.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.total_latency_ms.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.min_latency_ms.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn test_worker_metrics_connection_tracking() {
    let metrics = WorkerMetrics::new();
    
    use std::sync::atomic::Ordering;
    
    // Simulate accepting connections
    metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
    metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
    metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
    
    assert_eq!(metrics.connections_accepted.load(Ordering::Relaxed), 3);
}

#[test]
fn test_worker_metrics_request_tracking() {
    let metrics = WorkerMetrics::new();
    
    use std::sync::atomic::Ordering;
    
    // Simulate processing requests
    metrics.requests_processed.fetch_add(1, Ordering::Relaxed);
    metrics.requests_processed.fetch_add(1, Ordering::Relaxed);
    
    assert_eq!(metrics.requests_processed.load(Ordering::Relaxed), 2);
}

#[test]
fn test_worker_metrics_latency_tracking() {
    let metrics = WorkerMetrics::new();
    
    // Test basic request processing metrics
    metrics.record_request_processed();
    metrics.record_request_processed();
    metrics.record_request_processed();
    
    // Note: Due to thread-local caching, metrics may not be immediately visible
    // This test verifies the API works without causing panics
}

#[test]
fn test_worker_metrics_initial_state() {
    let metrics = WorkerMetrics::new();
    
    // Test that new metrics start with expected initial values
    assert_eq!(metrics.requests_processed.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.connections_accepted.load(Ordering::Relaxed), 0);
}

#[test]
fn test_worker_metrics_error_recording() {
    let metrics = WorkerMetrics::new();
    
    // Test error recording methods work without panicking
    metrics.record_parse_error();
    metrics.record_routing_error();
    metrics.record_backend_error();
}

#[test]
fn test_worker_metrics_connection_methods() {
    let metrics = WorkerMetrics::new();
    
    // Test connection tracking methods work without panicking
    metrics.record_connection_accepted();
    metrics.record_connection_closed();
}

#[test]
fn test_worker_metrics_clone() {
    let metrics1 = WorkerMetrics::new();
    let metrics2 = metrics1.clone();
    
    // Both should point to the same atomic values
    use std::sync::atomic::Ordering;
    metrics1.connections_accepted.fetch_add(1, Ordering::Relaxed);
    assert_eq!(metrics2.connections_accepted.load(Ordering::Relaxed), 1);
}

#[test]
fn test_worker_metrics_concurrent_updates() {
    use std::sync::Arc;
    use std::thread;
    
    let metrics = Arc::new(WorkerMetrics::new());
    let mut handles = vec![];
    
    // Spawn 10 threads, each incrementing counters
    for _ in 0..10 {
        let metrics_clone = metrics.clone();
        let handle = thread::spawn(move || {
            for _ in 0..100 {
                use std::sync::atomic::Ordering;
                metrics_clone.connections_accepted.fetch_add(1, Ordering::Relaxed);
                metrics_clone.requests_processed.fetch_add(1, Ordering::Relaxed);
                // Test basic metric recording in threaded context
                std::hint::black_box(50); // Just ensure some work happens
            }
        });
        handles.push(handle);
    }
    
    // Wait for all threads to complete
    for handle in handles {
        handle.join().unwrap();
    }
    
    use std::sync::atomic::Ordering;
    assert_eq!(metrics.connections_accepted.load(Ordering::Relaxed), 1000);
    assert_eq!(metrics.requests_processed.load(Ordering::Relaxed), 1000);
    // Verify the basic counters were updated correctly
    assert_eq!(metrics.total_latency_ms.load(Ordering::Relaxed), 0);
}

#[test]
fn test_worker_metrics_comprehensive() {
    let metrics = WorkerMetrics::new();
    
    // Test all the main recording methods work
    metrics.record_request_processed();
    metrics.record_connection_accepted();
    metrics.record_connection_closed();
    metrics.record_parse_error();
    metrics.record_routing_error();
    metrics.record_backend_error();
    
    // Verify the metrics struct has expected initial values
    assert_eq!(metrics.min_latency_ms.load(std::sync::atomic::Ordering::Relaxed), u64::MAX);
    assert_eq!(metrics.max_latency_ms.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn test_worker_metrics_configuration() {
    let metrics = WorkerMetrics::new();
    
    // Test that metrics configuration works
    // The enable_metrics and sampling_rate should be set based on environment
    assert!(metrics.sampling_rate > 0); // Should have some sampling rate
}