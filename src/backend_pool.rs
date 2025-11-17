//! Per-backend-address connection pool for persistent TCP connections.
//!
//! Reuses idle backend connections across requests to amortize TCP handshake cost.
//! Shared across all workers via Arc.

use std::net::SocketAddr;
use std::time::Duration;
use dashmap::DashMap;
use tokio::net::TcpStream;
use anyhow::Result;
use crate::logging::debug;

pub struct BackendPool {
    pools: DashMap<SocketAddr, Vec<TcpStream>>,
    max_idle_per_backend: usize,
    connect_timeout: Duration,
}

impl BackendPool {
    pub fn new(max_idle_per_backend: usize, connect_timeout: Duration) -> Self {
        Self {
            pools: DashMap::new(),
            max_idle_per_backend,
            connect_timeout,
        }
    }

    pub async fn acquire(&self, addr: SocketAddr) -> Result<TcpStream> {
        // Try reuse an idle connection — pop one at a time to minimize shard lock hold time.
        // The lock is released between iterations so other workers can acquire/release concurrently.
        loop {
            let conn = {
                let mut guard = match self.pools.get_mut(&addr) {
                    Some(g) => g,
                    None => break,
                };
                match guard.pop() {
                    Some(c) => c,
                    None => break,
                }
            };
            // Guard is dropped — liveness check happens outside the critical section
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

    pub fn release(&self, addr: SocketAddr, conn: TcpStream) {
        let mut conns = self.pools.entry(addr).or_default();
        if conns.len() < self.max_idle_per_backend {
            conns.push(conn);
        }
        // else: drop closes the fd
    }
}
