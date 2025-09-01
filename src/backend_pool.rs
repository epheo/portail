//! Per-worker connection pool for persistent TCP connections.
//!
//! Each worker owns its own pool — no locks, no contention.
//! Reuses idle backend connections across requests to amortize TCP handshake cost.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use anyhow::Result;
use crate::logging::debug;

pub struct BackendPool {
    pools: HashMap<SocketAddr, Vec<TcpStream>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
}

impl BackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: HashMap::new(),
            max_idle_per_backend,
            connect_timeout,
        }
    }

    pub async fn acquire(&mut self, addr: SocketAddr) -> Result<TcpStream> {
        // Try reuse an idle connection — pop and probe until we find a live one.
        if let Some(conns) = self.pools.get_mut(&addr) {
            while let Some(conn) = conns.pop() {
                let mut probe = [0u8; 0];
                match conn.try_read(&mut probe) {
                    // Would block = connection is alive and idle
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        debug!("Pool hit: reusing connection to {}", addr);
                        return Ok(conn);
                    }
                    // Zero bytes read or error = stale connection, try next
                    _ => continue,
                }
            }
        }

        // No reusable connection — open a new one
        debug!("Pool miss: connecting to {}", addr);
        let conn = tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect(addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Backend connect timeout: {}", addr))?
        .map_err(|e| anyhow::anyhow!("Backend connect failed {}: {}", addr, e))?;

        conn.set_nodelay(true)?;
        Ok(conn)
    }

    pub fn release(&mut self, addr: SocketAddr, conn: TcpStream) {
        let conns = self.pools.entry(addr).or_default();
        if conns.len() < self.max_idle_per_backend {
            conns.push(conn);
        }
        // else: drop closes the fd
    }
}
