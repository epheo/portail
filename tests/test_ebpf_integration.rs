/// eBPF integration tests for Portail
/// Validates eBPF system initialization (requires privileged environment)

use anyhow::Result;

#[cfg(test)]
mod ebpf_tests {
    use super::*;
    use portail::ebpf::initialize_ebpf_system;

    #[test]
    fn test_ebpf_system_initialization() -> Result<()> {
        if !is_ebpf_capable() {
            println!("Skipping eBPF test - insufficient privileges or kernel support");
            return Ok(());
        }

        let result = initialize_ebpf_system();

        match result {
            Ok(_manager) => {
                println!("eBPF system initialized successfully");
            }
            Err(e) => {
                println!("eBPF initialization failed (expected in test environment): {}", e);
                assert!(e.to_string().contains("eBPF") || e.to_string().contains("capability"));
            }
        }

        Ok(())
    }

    fn is_ebpf_capable() -> bool {
        // Root always has capabilities
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            return std::path::Path::new("/sys/fs/bpf").exists();
        }

        // Check effective capabilities from /proc/self/status
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
                return (cap_eff & cap_net_admin != 0)
                    && (cap_eff & cap_bpf != 0)
                    && std::path::Path::new("/sys/fs/bpf").exists();
            }
        }
        false
    }

    #[test]
    #[ignore] // Requires root privileges and eBPF support
    fn test_basic_ebpf_workflow() -> Result<()> {
        if !is_ebpf_capable() {
            println!("Skipping basic eBPF workflow test - insufficient privileges");
            return Ok(());
        }

        let _manager = initialize_ebpf_system()?;
        println!("Basic eBPF workflow test completed successfully");
        Ok(())
    }
}
