use uringress::routing::{RouteTable, Backend, FnvRouterHasher};

#[test]
fn test_fnv_hash_consistency() {
    let host = "example.com";
    let hash1 = FnvRouterHasher::hash(host);
    let hash2 = FnvRouterHasher::hash(host);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_fnv_hash_different_values() {
    let hash1 = FnvRouterHasher::hash("example.com");
    let hash2 = FnvRouterHasher::hash("different.com");
    assert_ne!(hash1, hash2);
}

#[test]
fn test_route_table_creation() {
    let table = RouteTable::new();
    assert!(table.http_routes.is_empty());
    assert!(table.tcp_routes.is_empty());
}

#[test]
fn test_route_table_http_routing() {
    let mut table = RouteTable::new();

    let backend = Backend::new("192.168.1.100".to_string(), 8080).unwrap();
    table.add_http_route("api.example.com", "/v1/users", vec![backend]);

    let result = table.find_http_backends("api.example.com", "/v1/users/123");
    assert!(result.is_ok());

    let backends = result.unwrap();
    let backend_ref = &backends[0];
    assert_eq!(backend_ref.socket_addr.ip(), std::net::IpAddr::V4("192.168.1.100".parse().unwrap()));
    assert_eq!(backend_ref.socket_addr.port(), 8080);
}

#[test]
fn test_route_table_http_routing_no_match() {
    let table = RouteTable::new();

    let result = table.find_http_backends("nonexistent.com", "/api/test");
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

    let result = table.find_tcp_backends(5432);
    assert!(result.is_ok());

    let backends = result.unwrap();
    let backend_ref = &backends[0];
    assert!(format!("{}", backend_ref.socket_addr.ip()).starts_with("192.168.1.1"));
    assert_eq!(backend_ref.socket_addr.port(), 5432);
}

#[test]
fn test_route_table_tcp_routing_no_match() {
    let table = RouteTable::new();

    let result = table.find_tcp_backends(9999);
    assert!(result.is_err());
}

#[test]
fn test_backend_creation() {
    let backend = Backend::new("127.0.0.1".to_string(), 9000).unwrap();
    assert_eq!(format!("{}", backend.socket_addr.ip()), "127.0.0.1");
    assert_eq!(backend.socket_addr.port(), 9000);
}

#[test]
fn test_path_matching_longest_prefix_first() {
    let mut table = RouteTable::new();

    // Add routes in reverse order to test sorting
    let backend1 = Backend::new("127.0.0.1".to_string(), 8080).unwrap();
    let backend2 = Backend::new("127.0.0.2".to_string(), 8080).unwrap();

    table.add_http_route("api.com", "/api", vec![backend1]);
    table.add_http_route("api.com", "/api/v1", vec![backend2]);

    // Should match longer prefix first
    let result = table.find_http_backends("api.com", "/api/v1/users");
    assert!(result.is_ok());
    let backends = result.unwrap();
    let backend_ref = &backends[0];
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.2");

    // Should match shorter prefix for non-v1 paths
    let result = table.find_http_backends("api.com", "/api/v2/users");
    assert!(result.is_ok());
    let backends = result.unwrap();
    let backend_ref = &backends[0];
    assert_eq!(format!("{}", backend_ref.socket_addr.ip()), "127.0.0.1");
}
