use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::time::Duration;
use uringress::backend::PooledBuffer;

/// Benchmark allocation patterns and buffer reuse rates
fn bench_allocation_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("allocation_patterns");
    
    // Test different data sizes to verify buffer pool efficiency
    let sizes = vec![64, 512, 1024, 4096, 8192, 16384];
    
    for size in sizes {
        group.bench_with_input(
            BenchmarkId::new("pooled_buffer", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    // Create pooled buffer - this should reuse after warm-up
                    let mut buffer = PooledBuffer::new(size);
                    buffer.copy_from_slice(&black_box(vec![42u8; size]));
                    black_box(buffer.as_mut_vec().len())
                });
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("vec_allocation", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    // Direct Vec allocation for comparison
                    let data = black_box(vec![42u8; size]);
                    black_box(data.len())
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark buffer pool reuse rates
fn bench_buffer_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_reuse");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("buffer_pool_reuse_rate", |b| {
        // Warm up the pool
        for _ in 0..100 {
            let mut buffer = PooledBuffer::new(1024);
            buffer.copy_from_slice(&vec![0u8; 1024]);
        }
        
        // Initialize buffer pool stats tracking
        uringress::backend::reset_pool_stats();
        
        b.iter(|| {
            for _ in 0..100 {
                let mut buffer = PooledBuffer::new(black_box(1024));
                buffer.copy_from_slice(&black_box(vec![42u8; 1024]));
            }
        });
        
        // Validate buffer pool reuse rate
        let reuse_rate = uringress::backend::get_reuse_rate();
        println!("Buffer reuse rate: {:.2}%", reuse_rate);
        assert!(reuse_rate > 85.0, "Buffer reuse rate should be >85% but was {:.2}%", reuse_rate);
    });
    
    group.finish();
}

/// Benchmark memory allocation latency comparison
fn bench_allocation_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("allocation_latency");
    
    // HTTP request size simulation
    let http_request_data = vec![42u8; 2048];
    
    group.bench_function("to_vec_allocation", |b| {
        b.iter(|| {
            // Simulate the old .to_vec() pattern
            let _owned_data = black_box(&http_request_data).to_vec();
            black_box(_owned_data.len())
        });
    });
    
    group.bench_function("pooled_buffer", |b| {
        b.iter(|| {
            // Simulate the new pooled buffer pattern
            let mut buffer = PooledBuffer::new(http_request_data.len());
            buffer.copy_from_slice(black_box(&http_request_data));
            black_box(buffer.as_mut_vec().len())
        });
    });
    
    group.finish();
}

/// Test that validates zero allocation in steady state
fn bench_zero_allocation_validation(c: &mut Criterion) {
    let mut group = c.benchmark_group("zero_allocation_validation");
    
    group.bench_function("steady_state_allocations", |b| {
        // Warm up the buffer pool thoroughly
        for _ in 0..1000 {
            let mut buffer = PooledBuffer::new(1024);
            buffer.copy_from_slice(&vec![0u8; 1024]);
        }
        
        // Track pre-benchmark buffer pool stats
        let (initial_pool_size, initial_reuse_count, initial_total) = uringress::backend::get_pool_stats();
        println!("Initial buffer pool stats - Size: {}, Reuse: {}, Total: {}", 
                 initial_pool_size, initial_reuse_count, initial_total);
        
        b.iter(|| {
            // Simulate typical request processing
            let mut buffer = PooledBuffer::new(black_box(1024));
            buffer.copy_from_slice(&black_box(vec![42u8; 1024]));
            black_box(buffer.as_mut_vec().len())
        });
        
        // Validate steady state has high reuse rate (indicating minimal new allocations)
        let (final_pool_size, final_reuse_count, final_total) = uringress::backend::get_pool_stats();
        let steady_state_reuse_rate = if final_total > initial_total {
            ((final_reuse_count - initial_reuse_count) as f64 / (final_total - initial_total) as f64) * 100.0
        } else {
            0.0
        };
        
        println!("Steady state buffer pool stats - Size: {}, Reuse: {}, Total: {}", 
                 final_pool_size, final_reuse_count, final_total);
        println!("Steady state reuse rate: {:.2}%", steady_state_reuse_rate);
        
        // In steady state, we expect very high reuse rates (>99%)
        assert!(steady_state_reuse_rate > 99.0, 
               "Steady state should have >99% reuse rate but was {:.2}%", steady_state_reuse_rate);
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_allocation_patterns,
    bench_buffer_reuse,
    bench_allocation_latency,
    bench_zero_allocation_validation
);
criterion_main!(benches);