/// eBPF module for UringRess SO_REUSEPORT worker selection
/// Implements address-aware worker dispatch at socket level
/// 
/// Simplified Architecture:
/// - SO_REUSEPORT Program: Address-aware worker selection
/// - Maps: Worker capabilities and routing configuration
/// - Single Codepath: eBPF functionality is required, no fallbacks

pub mod unified_manager;
pub mod address_dispatcher;

pub use unified_manager::UnifiedEbpfManager;
pub use address_dispatcher::AddressDispatcher;

use anyhow::{anyhow, Result};
use tracing::info;

/// Validate eBPF system requirements - public interface for early startup validation  
/// Single codepath architecture: either all requirements pass or system fails
pub fn validate_system_requirements() -> Result<()> {
    validate_ebpf_requirements()
}

/// Initialize simplified eBPF subsystem - SO_REUSEPORT program only
/// Returns error if eBPF requirements not met - no fallback behavior
pub fn initialize_ebpf_system() -> Result<UnifiedEbpfManager> {
    info!("Initializing simplified eBPF system - SO_REUSEPORT program only");
    
    // Requirements already validated in main.rs startup
    // Create simplified eBPF manager - required component, no Option<>
    let manager = UnifiedEbpfManager::new()?;
    
    info!("eBPF system initialized successfully");
    Ok(manager)
}

/// Validate eBPF system requirements - fail fast if not met
/// Single codepath architecture: either all requirements pass or system fails
fn validate_ebpf_requirements() -> Result<()> {
    // Validate kernel version (Linux 5.10+ required for io_uring + eBPF)
    validate_kernel_version()?;
    
    // Validate capabilities (CAP_BPF, CAP_NET_ADMIN required)
    validate_capabilities()?;
    
    // Rust eBPF programs are embedded at compile time - no file validation needed
    
    info!("eBPF requirements validation passed - single codepath architecture ready");
    Ok(())
}

/// Validate kernel version meets minimum requirements
fn validate_kernel_version() -> Result<()> {
    use std::fs;
    
    let version_info = fs::read_to_string("/proc/version")
        .map_err(|e| anyhow!("Cannot read kernel version from /proc/version: {}", e))?;
    
    // Extract kernel version (format: "Linux version X.Y.Z ...")
    if let Some(version_start) = version_info.find("Linux version ") {
        let version_str = &version_info[version_start + 14..];
        if let Some(version_end) = version_str.find(' ') {
            let version = &version_str[..version_end];
            
            // Parse major.minor version
            let parts: Vec<&str> = version.split('.').collect();
            if parts.len() >= 2 {
                if let (Ok(major), Ok(minor)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                    let kernel_version = (major, minor);
                    let required_version = (5, 10);
                    
                    if kernel_version >= required_version {
                        info!("Kernel version {} meets requirements (5.10+ required)", version);
                        return Ok(());
                    } else {
                        return Err(anyhow!(
                            "Kernel version {} does not meet requirements. \
                            \nRequired: Linux 5.10+ for io_uring and eBPF integration \
                            \nCurrent: Linux {} \
                            \nSingle codepath architecture requires modern kernel features.", 
                            version, version
                        ));
                    }
                }
            }
        }
    }
    
    Err(anyhow!(
        "Cannot parse kernel version from: {} \
        \nRequired: Linux 5.10+ for io_uring and eBPF support", 
        version_info.chars().take(100).collect::<String>()
    ))
}

/// Validate process has required capabilities
fn validate_capabilities() -> Result<()> {
    use std::process::Command;
    
    // Check if running as root (uid 0) - simplest capability check
    let uid_output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| anyhow!("Cannot check user ID: {}", e))?;
    
    if !uid_output.status.success() {
        return Err(anyhow!("Cannot determine user privileges"));
    }
    
    let uid_str = String::from_utf8_lossy(&uid_output.stdout);
    let uid = uid_str.trim().parse::<u32>()
        .map_err(|e| anyhow!("Cannot parse user ID '{}': {}", uid_str.trim(), e))?;
    
    if uid == 0 {
        info!("Running as root - all eBPF capabilities available");
        return Ok(());
    }
    
    // For non-root, provide clear guidance
    Err(anyhow!(
        "UringRess requires CAP_BPF and CAP_NET_ADMIN capabilities for eBPF operations. \
        \nCurrent user ID: {} (non-root) \
        \nSolutions: \
        \n  1. Run as root: sudo uringress \
        \n  2. Set capabilities: sudo setcap cap_bpf,cap_net_admin+ep uringress \
        \n  3. Add user to appropriate groups with eBPF access \
        \nSingle codepath architecture requires full eBPF privileges.",
        uid
    ))
}

// No longer needed - Rust eBPF programs are embedded at compile time

