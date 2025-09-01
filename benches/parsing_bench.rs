use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use uringress::parser::parse_http_headers_fast;
use std::time::Duration;

fn parsing_benchmark(c: &mut Criterion) {
    // Various HTTP request samples for comprehensive benchmarking
    let simple_request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    
    let typical_request = b"GET /api/v1/users/12345?page=1&limit=50 HTTP/1.1\r\nHost: api.example.com\r\nUser-Agent: Mozilla/5.0\r\nAccept: application/json\r\nAuthorization: Bearer token123\r\n\r\n";
    
    let complex_request = b"POST /api/v2/data/analytics/reports/generate HTTP/1.1\r\nHost: analytics.enterprise.example.com\r\nUser-Agent: Enterprise-Client/2.1.0 (Linux; x86_64)\r\nAccept: application/json, application/xml;q=0.9, */*;q=0.8\r\nAccept-Language: en-US,en;q=0.9,fr;q=0.8\r\nAccept-Encoding: gzip, deflate, br\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 1024\r\nAuthorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\r\nX-Request-ID: req-12345-67890-abcdef\r\nX-Correlation-ID: corr-98765-43210-fedcba\r\nX-API-Version: 2.1\r\nConnection: keep-alive\r\n\r\n";
    
    let malformed_request = b"INVALID REQUEST FORMAT WITHOUT PROPER HEADERS\r\n\r\n";
    
    // Fast parsing benchmarks - Target: <1μs for simple requests
    c.bench_function("parse_headers_fast_simple", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(simple_request))
        })
    });
    
    c.bench_function("parse_headers_fast_typical", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(typical_request))
        })
    });
    
    c.bench_function("parse_headers_fast_complex", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(complex_request))
        })
    });
    
    // Additional fast parsing benchmarks for different scenarios
    c.bench_function("parse_headers_fast_simple_repeated", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(simple_request))
        })
    });
    
    c.bench_function("parse_headers_fast_typical_repeated", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(typical_request))
        })
    });
    
    c.bench_function("parse_headers_fast_complex_repeated", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(complex_request))
        })
    });
    
    // Error handling benchmarks
    c.bench_function("parse_malformed_request", |b| {
        b.iter(|| {
            black_box(parse_http_headers_fast(malformed_request))
        })
    });
    
    // Scalability benchmarks - varying request sizes
    let mut group = c.benchmark_group("parse_by_request_size");
    
    for size in [100, 500, 1000, 2000, 4000].iter() {
        let large_request = generate_large_request(*size);
        group.bench_with_input(BenchmarkId::new("parse_headers_fast", size), &large_request, |b, req| {
            b.iter(|| {
                black_box(parse_http_headers_fast(req))
            })
        });
    }
    group.finish();
    
    // Different HTTP methods
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    let mut method_group = c.benchmark_group("parse_by_method");
    
    for method in methods.iter() {
        let request = format!("{} /api/test HTTP/1.1\r\nHost: example.com\r\n\r\n", method);
        method_group.bench_with_input(BenchmarkId::new("parse_headers_fast", method), &request, |b, req| {
            b.iter(|| {
                black_box(parse_http_headers_fast(req.as_bytes()))
            })
        });
    }
    method_group.finish();
    
    // Host header variations
    let hosts = [
        "api.com",
        "subdomain.api.com", 
        "very.long.subdomain.api.example.com",
        "localhost:8080",
        "192.168.1.100:3000"
    ];
    
    let mut host_group = c.benchmark_group("parse_by_host_complexity");
    for (i, host) in hosts.iter().enumerate() {
        let request = format!("GET /test HTTP/1.1\r\nHost: {}\r\n\r\n", host);
        host_group.bench_with_input(BenchmarkId::new("parse_headers_fast", i), &request, |b, req| {
            b.iter(|| {
                black_box(parse_http_headers_fast(req.as_bytes()))
            })
        });
    }
    host_group.finish();
}

fn memory_parsing_benchmark(c: &mut Criterion) {
    let request = b"GET /api/test HTTP/1.1\r\nHost: example.com\r\nUser-Agent: test\r\n\r\n";
    
    // Memory allocation benchmarks
    c.bench_function("parse_headers_fast_allocation", |b| {
        b.iter(|| {
            // This measures the allocation overhead of parsing
            black_box(parse_http_headers_fast(request))
        })
    });
    
    c.bench_function("parse_headers_fast_allocation_test", |b| {
        b.iter(|| {
            // This measures fast parsing allocation overhead in repeated calls
            black_box(parse_http_headers_fast(request))
        })
    });
}

// Generate large HTTP request for scalability testing
fn generate_large_request(size: usize) -> Vec<u8> {
    let mut request = "GET /api/large/path".to_string();
    
    // Add query parameters to reach target size
    let mut current_size = request.len();
    let mut param_count = 0;
    
    while current_size < size - 100 { // Leave room for headers
        if param_count == 0 {
            request.push('?');
        } else {
            request.push('&');
        }
        request.push_str(&format!("param{}=value{}", param_count, param_count));
        current_size = request.len();
        param_count += 1;
    }
    
    request.push_str(" HTTP/1.1\r\nHost: example.com\r\nUser-Agent: benchmark\r\n\r\n");
    request.into_bytes()
}

// Configure benchmarks for MVP performance targets
fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(10000) // Higher sample size for parsing benchmarks
        .with_plots()
}

criterion_group! {
    name = parsing_benches;
    config = config();
    targets = parsing_benchmark, memory_parsing_benchmark
}
criterion_main!(parsing_benches);