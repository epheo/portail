//! The data plane: listener management, connection workers, HTTP
//! parsing/filtering, backend connection pooling, TLS, and backend health.
//! Everything that touches a client or backend socket lives here; the
//! control planes (config file, kubernetes/) only hand it route tables.

pub mod backend_pool;
pub mod chunked;
pub mod data_plane;
pub mod health;
pub mod http_filters;
pub mod http_parser;
pub mod request_processor;
pub mod tls;
pub mod udp_worker;
pub mod worker;
