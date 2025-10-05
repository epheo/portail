//! eBPF module for UringRess SO_REUSEPORT worker selection
//! Implements address-aware worker dispatch at socket level
//!
//! Simplified Architecture:
//! - SO_REUSEPORT Program: Address-aware worker selection
//! - Maps: Worker capabilities and routing configuration
//! - Single Codepath: eBPF functionality is required, no fallbacks

pub mod unified_manager;

pub use unified_manager::UnifiedEbpfManager;

use anyhow::{anyhow, Result};
use crate::logging::info;

/// Validate eBPF system requirements - fail fast if not met
/// Single codepath: either all requirements pass or system fails
pub fn validate_system_requirements() -> Result<()> {
    validate_kernel_version()?;
    validate_capabilities()?;
    info!("eBPF requirements validation passed");
    Ok(())
}

/// Initialize eBPF subsystem - SO_REUSEPORT program only
pub fn initialize_ebpf_system() -> Result<UnifiedEbpfManager> {
    info!("Initializing eBPF system - SO_REUSEPORT program only");
    let manager = UnifiedEbpfManager::new()?;
    info!("eBPF system initialized successfully");
    Ok(manager)
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

fn validate_capabilities() -> Result<()> {
    // SAFETY: getuid() is always safe and cannot fail
    let uid = unsafe { libc::getuid() };

    if uid == 0 {
        info!("Running as root - all eBPF capabilities available");
        return Ok(());
    }

    if has_required_capabilities() {
        info!("Required capabilities (CAP_BPF, CAP_NET_ADMIN) detected via setcap");
        return Ok(());
    }

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

/// Check effective capabilities from /proc/self/status (no external deps)
fn has_required_capabilities() -> bool {
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return false,
    };

    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:\t") {
            let cap_eff = match u64::from_str_radix(hex.trim(), 16) {
                Ok(v) => v,
                Err(_) => return false,
            };
            let cap_net_admin = 1u64 << 12;
            let cap_bpf = 1u64 << 39;
            return (cap_eff & cap_net_admin != 0) && (cap_eff & cap_bpf != 0);
        }
    }

    false
}

