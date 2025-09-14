//! UringDataPlane - Multi-Worker Data Plane Manager
//!
//! This module provides the data plane wrapper that manages multiple UringWorker threads
//! with the new modular architecture. It maintains compatibility with the existing main.rs
//! while using the new zero-copy processing pipeline.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicBool, Ordering};
use std::os::unix::io::RawFd;
use tracing::{info, debug, error};

use crate::config::{WorkerConfig, PerformanceConfig};
use crate::routing::RouteTable;
use crate::runtime_config::RuntimeConfig;
use crate::uring_worker::UringWorker;
use crate::backend_pool::TcpKeepAliveConfig;

/// Multi-worker data plane using the new modular architecture
/// Manages multiple UringWorker threads with SO_REUSEPORT and eBPF integration
pub struct UringDataPlane {
    /// Worker thread handles
    workers: Vec<std::thread::JoinHandle<Result<()>>>,
    /// Configuration for each worker
    worker_configs: Vec<WorkerConfig>,
    /// Collected listener file descriptors from all workers
    collected_listener_fds: Option<Vec<(u16, RawFd)>>,
    /// Synchronization barrier for worker setup phase
    setup_barrier: Option<Arc<std::sync::Barrier>>,
    /// Synchronization barrier for eBPF attachment phase
    ebpf_barrier: Option<Arc<std::sync::Barrier>>,
    /// Zero-latency coordination for sequential worker setup (eliminates busy-wait)
    setup_sync: Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
    /// Worker shutdown flags for graceful termination
    worker_shutdown_flags: Vec<Arc<AtomicBool>>,
    // Removed: Shared backend pool - now using thread-local pools per worker
}

impl UringDataPlane {
    /// Create new data plane with worker configurations
    pub fn new(worker_configs: Vec<WorkerConfig>) -> Self {
        let worker_count = worker_configs.len();
        info!("Creating UringDataPlane with {} workers using modular architecture", worker_count);
        
        // Backend pool will be initialized in start() method when config is available
        
        Self {
            workers: Vec::with_capacity(worker_count),
            worker_configs,
            collected_listener_fds: None,
            setup_barrier: None,
            ebpf_barrier: None,
            setup_sync: Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new())),
            worker_shutdown_flags: Vec::new(),
            // Removed: backend_pool - using thread-local pools
        }
    }
    
    /// Start all workers with routing table and configuration
    /// Uses the new modular UringWorker with zero-copy processing pipeline
    pub fn start(&mut self, routes: Arc<AtomicPtr<RouteTable>>, config: RuntimeConfig, performance_config: PerformanceConfig, backend_pool_config: &crate::config::BackendPoolConfig) -> Result<()> {
        let worker_count = self.worker_configs.len();
        info!("Starting {} modular io_uring data plane workers with zero-copy pipeline", worker_count);
        
        // Initialize TCP config for thread-local pools
        let tcp_config = TcpKeepAliveConfig {
            enabled: backend_pool_config.tcp_keepalive,
            idle_time: backend_pool_config.tcp_keepalive_idle,
            probe_interval: backend_pool_config.tcp_keepalive_interval,
            probe_count: backend_pool_config.tcp_keepalive_probes as u32,
        };
        
        // Create shared storage for collecting listener FDs from all workers
        let listener_fds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let setup_barrier = Arc::new(std::sync::Barrier::new(worker_count + 1)); // +1 for main thread
        let ebpf_barrier = Arc::new(std::sync::Barrier::new(worker_count + 1)); // +1 for main thread
        
        // Store barriers for coordination
        self.setup_barrier = Some(setup_barrier.clone());
        self.ebpf_barrier = Some(ebpf_barrier.clone());
        
        // Thread-local pools handle backend monitoring per worker
        info!("Using thread-local backend pools - each worker manages its own connections");
        
        info!("MODULAR_WORKERS: Starting {} worker threads with new architecture", self.worker_configs.len());
        
        // Create shutdown flags for all workers upfront for graceful shutdown
        for _ in 0..worker_count {
            self.worker_shutdown_flags.push(Arc::new(AtomicBool::new(false)));
        }
        
        // Spawn worker threads using the new modular UringWorker
        for (worker_index, worker_config) in self.worker_configs.clone().into_iter().enumerate() {
            info!("MODULAR_WORKER: Spawning worker thread {} with zero-copy pipeline", worker_config.id);
            let routes_clone = routes.clone();
            let config_clone = config.clone();
            let performance_config_clone = performance_config.clone();
            let listener_fds_clone = listener_fds.clone();
            let setup_barrier_clone = setup_barrier.clone();
            let ebpf_barrier_clone = ebpf_barrier.clone();
            let setup_sync_clone = self.setup_sync.clone();
            let shutdown_flag = self.worker_shutdown_flags[worker_index].clone();
            let tcp_config_clone = tcp_config.clone();
            
            let handle = std::thread::spawn(move || {
                let worker_id = worker_config.id;
                info!("MODULAR_WORKER: Worker {} thread started with new architecture", worker_id);
                
                // Set panic hook for this thread
                std::panic::set_hook(Box::new(move |panic_info| {
                    error!("MODULAR_WORKER_PANIC: Worker {} thread panicked: {}", worker_id, panic_info);
                }));
                
                // Create and initialize the new modular UringWorker
                let mut worker = match UringWorker::new(worker_config, routes_clone, config_clone, performance_config_clone, tcp_config_clone) {
                    Ok(mut worker) => {
                        info!("MODULAR_WORKER: Worker {} created successfully with modular architecture", worker_id);
                        // Replace internal shutdown flag with external one for coordinated shutdown
                        worker.set_external_shutdown_flag(shutdown_flag);
                        worker
                    }
                    Err(e) => {
                        error!("MODULAR_WORKER: Failed to create worker {}: {}", worker_id, e);
                        return Err(e);
                    }
                };
                
                // Zero-latency sequential coordination for SO_REUSEPORT setup safety
                // Event-driven blocking eliminates both CPU waste and artificial latency
                {
                    let (mutex, condvar) = &*setup_sync_clone;
                    let mut current_turn = mutex.lock().unwrap();
                    while *current_turn != worker_id {
                        current_turn = condvar.wait(current_turn).unwrap();
                    }
                    info!("MODULAR_WORKER: Worker {} acquired setup turn", worker_id);
                }
                
                // Setup listeners - now executed sequentially per worker
                if let Err(e) = worker.setup_listeners() {
                    error!("MODULAR_WORKER: Worker {} failed to setup listeners: {}", worker_id, e);
                    return Err(e);
                }
                
                // Release turn to next worker with immediate notification
                {
                    let (mutex, condvar) = &*setup_sync_clone;
                    let mut current_turn = mutex.lock().unwrap();
                    *current_turn = worker_id + 1;
                    condvar.notify_all(); // Wake up next worker immediately
                }
                info!("MODULAR_WORKER: Worker {} completed setup, released turn to worker {}", worker_id, worker_id + 1);
                
                // Collect listener FDs for eBPF SO_REUSEPORT program attachment
                let worker_listener_fds = worker.get_listener_fds();
                {
                    let mut shared_fds = listener_fds_clone.lock().unwrap();
                    for (port, fd) in worker_listener_fds {
                        shared_fds.push((port, fd));
                        debug!("MODULAR_WORKER: Worker {} contributed listener fd {} for port {}", worker_id, fd, port);
                    }
                }
                
                info!("MODULAR_WORKER: Worker {} setup complete, coordinating with main thread", worker_id);
                
                // Wait for all workers to complete setup (first barrier)
                setup_barrier_clone.wait();
                info!("MODULAR_WORKER: Worker {} passed setup barrier", worker_id);
                
                // Wait for eBPF programs to be attached (second barrier)
                info!("MODULAR_WORKER: Worker {} waiting for eBPF attachment", worker_id);
                ebpf_barrier_clone.wait();
                info!("MODULAR_WORKER: Worker {} eBPF barrier released - starting event loop", worker_id);
                
                // Start the main event loop with the new zero-copy processing pipeline
                info!("MODULAR_WORKER: Worker {} starting zero-copy event loop", worker_id);
                worker.run()
            });
            
            self.workers.push(handle);
        }
        
        // Wait for all workers to complete setup
        info!("DATA_PLANE: Waiting for all {} workers to complete setup", worker_count);
        setup_barrier.wait();
        info!("DATA_PLANE: All workers completed setup phase");
        
        // Collect all listener FDs
        let collected_fds = {
            let fds_guard = listener_fds.lock().unwrap();
            fds_guard.clone()
        };
        
        info!("DATA_PLANE: Collected {} listener FDs from workers", collected_fds.len());
        for (port, fd) in &collected_fds {
            debug!("DATA_PLANE: Listener fd {} for port {}", fd, port);
        }
        
        self.collected_listener_fds = Some(collected_fds);
        Ok(())
    }
    
    /// Get listener file descriptors for SO_REUSEPORT eBPF program attachment
    pub fn get_listener_fds(&self) -> Option<Vec<(u16, RawFd)>> {
        self.collected_listener_fds.clone()
    }
    
    /// Release workers to start event loops after eBPF attachment
    /// Called after SO_REUSEPORT eBPF programs are successfully attached
    pub fn start_workers_after_ebpf_attachment(&self) -> Result<()> {
        info!("DATA_PLANE: Releasing eBPF barrier to start zero-copy event loops");
        
        if let Some(ref barrier) = self.ebpf_barrier {
            // This barrier.wait() releases all workers waiting at their second barrier.wait()
            barrier.wait();
            info!("DATA_PLANE: All workers released from eBPF barrier - zero-copy processing active");
            Ok(())
        } else {
            Err(anyhow::anyhow!("eBPF barrier not initialized - workers not started"))
        }
    }
    
    /// Wait for all worker threads to complete
    /// This is typically called during shutdown
    pub fn join_all(mut self) -> Result<()> {
        info!("DATA_PLANE: Waiting for all worker threads to complete");
        
        // Take ownership of workers to avoid Drop trait conflict
        let workers = std::mem::take(&mut self.workers);
        
        for (i, handle) in workers.into_iter().enumerate() {
            match handle.join() {
                Ok(result) => {
                    match result {
                        Ok(()) => info!("DATA_PLANE: Worker {} completed successfully", i),
                        Err(e) => error!("DATA_PLANE: Worker {} completed with error: {}", i, e),
                    }
                }
                Err(e) => {
                    error!("DATA_PLANE: Failed to join worker thread {}: {:?}", i, e);
                }
            }
        }
        
        info!("DATA_PLANE: All worker threads joined");
        Ok(())
    }
    
    /// Shutdown the data plane (alias for join_all for compatibility)
    /// This maintains compatibility with the main.rs shutdown sequence
    pub fn shutdown(&mut self) -> Result<()> {
        info!("DATA_PLANE: Initiating shutdown sequence");
        
        // Signal all workers to shutdown gracefully
        info!("DATA_PLANE: Signaling {} workers to shutdown", self.worker_shutdown_flags.len());
        for (i, flag) in self.worker_shutdown_flags.iter().enumerate() {
            flag.store(true, Ordering::Release);
            debug!("DATA_PLANE: Signaled worker {} for shutdown", i);
        }
        
        // Note: This takes self by value via std::mem::take to avoid Drop trait conflicts
        let temp_self = std::mem::replace(self, Self::new(Vec::new()));
        temp_self.join_all()
    }
}

impl Drop for UringDataPlane {
    fn drop(&mut self) {
        debug!("DATA_PLANE: UringDataPlane dropping - {} workers may still be running", self.workers.len());
    }
}