/// Address Dispatcher for Gateway API compliance
/// Manages address-to-worker mappings with load balancing intelligence
/// Single codepath: no fallback to round-robin, address mapping is required

use tracing::info;

use crate::config::WorkerConfig;

/// eBPF Configuration Provider for Worker Specialization
/// Provides configuration hints to eBPF programs - does NOT perform worker selection
/// eBPF programs handle all actual routing decisions
pub struct AddressDispatcher {
    /// Track eBPF SO_REUSEPORT attachment status
    ebpf_attached: bool,
}

impl AddressDispatcher {
    /// Create new eBPF configuration provider with worker configurations
    pub fn new(worker_configs: Vec<WorkerConfig>) -> Self {
        info!("Creating eBPF Configuration Provider for {} dynamic workers", worker_configs.len());
        
        Self {
            ebpf_attached: false,
        }
    }
    
    
    /// Set eBPF attachment status
    pub fn set_ebpf_attached(&mut self, attached: bool) {
        self.ebpf_attached = attached;
        if attached {
            info!("eBPF SO_REUSEPORT program attached successfully");
        }
    }
    
}