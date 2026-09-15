//! The request sniff runs once per request ahead of the parser and decides
//! HTTP versus opaque TCP on a shared listener. It lives in its own bench
//! binary so parsing_bench stays byte-identical: a 26 ns loop moves by more
//! than the gate's threshold when code alignment in its binary shifts.

use criterion::{criterion_group, criterion_main, Criterion};
use portail::proxy::request_processor::is_http_request;
use std::hint::black_box;
use std::time::Duration;

fn sniff_benchmark(c: &mut Criterion) {
    let common = b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\n\r\n";
    let extension = b"PROPFIND /calendars/ HTTP/1.1\r\nHost: example.com\r\nDepth: 0\r\n\r\n";
    let tls_client_hello: &[u8] = &[
        0x16, 0x03, 0x01, 0x00, 0xf4, 0x01, 0x00, 0x00, 0xf0, 0x03, 0x03,
    ];

    c.bench_function("sniff_common_method", |b| {
        b.iter(|| black_box(is_http_request(black_box(common))))
    });

    c.bench_function("sniff_extension_method", |b| {
        b.iter(|| black_box(is_http_request(black_box(extension))))
    });

    c.bench_function("sniff_non_http", |b| {
        b.iter(|| black_box(is_http_request(black_box(tls_client_hello))))
    });
}

fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(10000)
}

criterion_group! {
    name = sniff_benches;
    config = config();
    targets = sniff_benchmark
}
criterion_main!(sniff_benches);
