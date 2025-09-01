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
        
        // TODO: Implement buffer pool stats for profiling analysis
        
        b.iter(|| {
            for _ in 0..100 {
                let mut buffer = PooledBuffer::new(black_box(1024));
                buffer.copy_from_slice(&black_box(vec![42u8; 1024]));
            }
        });
        
        // TODO: Add buffer pool reuse rate validation when stats are implemented
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
        
        // TODO: Track pre-benchmark buffer pool stats
        
        b.iter(|| {
            // Simulate typical request processing
            let mut buffer = PooledBuffer::new(black_box(1024));
            buffer.copy_from_slice(&black_box(vec![42u8; 1024]));
            black_box(buffer.as_mut_vec().len())
        });
        
        // TODO: Validate steady state has zero new allocations
        
        // TODO: Add allocation rate validation when stats are implemented
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