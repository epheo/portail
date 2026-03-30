use portail::config::PortailConfig;

#[test]
fn test_bench_config_route_lookup() {
    // Exact JSON config that the benchmark generates
    let json = r#"{
  "gateway": {"name":"bench","listeners":[{"name":"http","protocol":"HTTP","port":29100}]},
  "httpRoutes": [{"parentRefs":[{"name":"bench","sectionName":"http"}],"hostnames":["127.0.0.1"],"rules":[{"matches":[{"path":{"type":"PathPrefix","value":"/"}}],"backendRefs":[{"name":"127.0.0.1","port":29300}]}]}],
  "observability": {"logging":{"level":"error","format":"pretty","output":"stderr"}},
  "performance": {}
}"#;
    let config: PortailConfig = serde_json::from_str(json).unwrap();
    assert!(config.validate().is_ok(), "Config validation failed");
    let rt = config.to_route_table().unwrap();

    // Scenario: rewrk-core sends requests with:
    //   Host: 127.0.0.1 (from URI authority.host(), port stripped)
    //   Path: /bench.txt
    //   server_port: 29100
    let result = rt.find_http_route("127.0.0.1", "GET", "/bench.txt", &[], "", 29100);
    println!("Route lookup: {:?}", result.is_ok());
    if let Err(ref e) = result {
        println!("Error: {}", e);
        // Print route table state
        println!(
            "Listener scopes: {:?}",
            rt.listener_scopes.keys().collect::<Vec<_>>()
        );
        for (port, scopes) in &rt.listener_scopes {
            for scope in scopes {
                println!(
                    "  Port {} scope: listener_hostname={:?} http_routes_hosts={:?} catch_all={}",
                    port,
                    scope.listener_hostname,
                    scope.http_routes.keys().collect::<Vec<_>>(),
                    scope.catch_all_http_routes.is_some()
                );
            }
        }
    }
    assert!(
        result.is_ok(),
        "Route lookup failed — this is the benchmark 404 bug"
    );
}
