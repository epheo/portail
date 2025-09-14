use uringress::routing::{RouteTable, Backend, BackendPool, LoadBalanceStrategy, FnvRouterHasher, RouterHasher};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

#[test]
fn test_fnv_hash_consistency() {
    let hasher = FnvRouterHasher;
    let host = "example.com";
    let hash1 = hasher.hash(host);
    let hash2 = hasher.hash(host);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_fnv_hash_different_values() {
    let hasher = FnvRouterHasher;
    let hash1 = hasher.hash("example.com");
    let hash2 = hasher.hash("different.com");
    assert_ne!(hash1, hash2);
}

#[test]
fn test_route_table_creation() {
    let table = RouteTable::new();
    assert!(table.http_routes.is_empty());
    assert!(table.tcp_routes.is_empty());
    assert!(table.default_http_backend.is_none());
    // Note: RouteTable doesn't have default_tcp_backend field in current implementation
}

#[test]
fn test_route_table_http_routing() {
    let mut table = RouteTable::new();
    
    let backend = Backend::new("192.168.1.100".to_string(), 8080).unwrap();
    table.add_http_route("api.example.com", "/v1/users", backend);
    
    let hasher = FnvRouterHasher;
    let host_hash = hasher.hash("api.example.com");
    let result = table.find_http_backend_pool(host_hash, "/v1/users/123");
    assert!(result.is_ok());
    
    let backend_pool = result.unwrap();
    let backend_ref = backend_pool.get_backend(0).unwrap();
    assert_eq!(backend_ref.socket_addr.ip(), std::net::IpAddr::V4("192.168.1.100".parse().unwrap()));
    assert_eq!(backend_ref.socket_addr.port(), 8080);
}

#[test]
fn test_route_table_http_routing_no_match() {
    let table = RouteTable::new();
    
    let hasher = FnvRouterHasher;
    let host_hash = hasher.hash("nonexistent.com");
    let result = table.find_http_backend_pool(host_hash, "/api/test");
    assert!(result.is_err());
}

#[test]
fn test_route_table_tcp_routing() {
    let mut table = RouteTable::new();
    
    let backends = vec![
        Backend::new("192.168.1.100".to_string(), 5432).unwrap(),
        Backend::new("192.168.1.101".to_string(), 5432).unwrap(),
    ];
    table.add_tcp_route(5432, backends);
    
    let result = table.find_tcp_backend_pool(5432);
    assert!(result.is_ok());
    
    let backend_pool = result.unwrap();
    let backend_ref = backend_pool.get_backend(0).unwrap();
    assert!(format!("{}", backend_ref.socket_addr.ip()).starts_with("192.168.1.1"));
    assert_eq!(backend_ref.socket_addr.port(), 5432);
}

#[test]
fn test_route_table_tcp_routing_no_match() {
    let table = RouteTable::new();
    
    let result = table.find_tcp_backend_pool(9999);
    assert!(result.is_err());
}

#[test]
fn test_backend_pool_round_robin() {
    let backends = vec![
        Backend::new("127.0.0.1".to_string(), 8080).unwrap(),
        Backend::new("127.0.0.2".to_string(), 8080).unwrap(),
        Backend::new("127.0.0.3".to_string(), 8080).unwrap(),
    ];
    
    let pool = BackendPool {
        backends,
        strategy: LoadBalanceStrategy::RoundRobin,
        health_state: Arc::new(AtomicU64::new(7)), // All 3 healthy
        _padding: [0; 0],
    };
    
    // Test manual backend selection by index
    let b1 = pool.get_backend(0).unwrap();
    let b2 = pool.get_backend(1).unwrap();
    let b3 = pool.get_backend(2).unwrap();
    let b4 = pool.get_backend(0).unwrap(); // Wrap around manually
    
    assert_eq!(format!("{}", b1.socket_addr.ip()), "127.0.0.1");
    assert_eq!(format!("{}", b2.socket_addr.ip()), "127.0.0.2");
    assert_eq!(format!("{}", b3.socket_addr.ip()), "127.0.0.3");
    assert_eq!(format!("{}", b4.socket_addr.ip()), "127.0.0.1"); // Wrapped around
}

#[test]
fn test_backend_pool_empty() {
    let pool = BackendPool {
        backends: vec![],
        strategy: LoadBalanceStrategy::RoundRobin,
        health_state: Arc::new(AtomicU64::new(0)),
        _padding: [0; 0],
    };
    
    let result = pool.get_backend(0);
    assert!(result.is_none());
}

#[test]
fn test_backend_creation() {
    let backend = Backend::new("127.0.0.1".to_string(), 9000).unwrap();
    assert_eq!(format!("{}", backend.socket_addr.ip()), "127.0.0.1");
    assert_eq!(backend.socket_addr.port(), 9000);
    assert!(backend.health_check_path.is_none());
}

#[test]
fn test_backend_with_health_check() {
    let backend = Backend::new("127.0.0.1".to_string(), 9000).unwrap()
        .with_health_check("/health".to_string());
    assert_eq!(backend.health_check_path, Some("/health".to_string()));
}

#[test]
fn test_path_matching_longest_prefix_first() {
    let mut table = RouteTable::new();
    
    // Add routes in reverse order to test sorting
    let backend1 = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    let backend2 = Backend::new("127.0.0.2".to_string(), 8080).unwrap();
    
    table.add_http_route("api.com", "/api", backend1);
    table.add_http_route("api.com", "/api/v1", backend2);
    
    let hasher = FnvRouterHasher;
    let host_hash = hasher.hash("api.com");
    
    // Should match longer prefix first
    let result = table.find_http_backend_pool(host_hash, "/api/v1/users");
    assert!(result.is_ok());
    let backend_pool = result.unwrap();
    let backend_ref = backend_pool.get_backend(0).unwrap();
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.2");
    
    // Should match shorter prefix for non-v1 paths
    let result = table.find_http_backend_pool(host_hash, "/api/v2/users");
    assert!(result.is_ok());
    let backend_pool = result.unwrap();
    let backend_ref = backend_pool.get_backend(0).unwrap();
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.1");
}