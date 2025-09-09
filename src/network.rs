/// Network interface utilities for UringRess
/// Provides interface validation, discovery, and smart selection

use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

/// Network interface information
#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub is_up: bool,
    pub is_loopback: bool,
    pub is_wireless: bool,
}

/// Get all available network interfaces on the system
pub fn list_network_interfaces() -> Result<Vec<NetworkInterface>> {
    let net_dir = Path::new("/sys/class/net");
    
    if !net_dir.exists() {
        return Err(anyhow!("System network interface directory not found: /sys/class/net"));
    }
    
    let mut interfaces = Vec::new();
    
    for entry in fs::read_dir(net_dir)? {
        let entry = entry?;
        let interface_name = entry.file_name().to_string_lossy().to_string();
        
        if let Ok(interface_info) = get_interface_info(&interface_name) {
            interfaces.push(interface_info);
        }
    }
    
    // Sort interfaces: physical first, then virtual, then loopback
    interfaces.sort_by(|a, b| {
        match (a.is_loopback, b.is_loopback) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.cmp(&b.name),
        }
    });
    
    debug!("Discovered {} network interfaces", interfaces.len());
    Ok(interfaces)
}

/// Get detailed information about a specific network interface
pub fn get_interface_info(interface_name: &str) -> Result<NetworkInterface> {
    let interface_path = format!("/sys/class/net/{}", interface_name);
    
    if !Path::new(&interface_path).exists() {
        return Err(anyhow!("Network interface '{}' not found", interface_name));
    }
    
    // Check if interface is up
    let operstate_path = format!("{}/operstate", interface_path);
    let is_up = fs::read_to_string(&operstate_path)
        .map(|state| state.trim() == "up")
        .unwrap_or(false);
    
    // Check if interface is loopback
    let is_loopback = interface_name == "lo";
    
    // Check if interface is wireless (simple heuristic)
    let wireless_path = format!("{}/wireless", interface_path);
    let is_wireless = Path::new(&wireless_path).exists();
    
    Ok(NetworkInterface {
        name: interface_name.to_string(),
        is_up,
        is_loopback,
        is_wireless,
    })
}

/// Validate that a network interface exists and is suitable for XDP
pub fn validate_interface_for_xdp(interface_name: &str) -> Result<NetworkInterface> {
    let interface = get_interface_info(interface_name)?;
    
    debug!("Validating interface '{}' for XDP: up={}, loopback={}, wireless={}", 
           interface.name, interface.is_up, interface.is_loopback, interface.is_wireless);
    
    // Interface exists, return the info for further validation by caller
    Ok(interface)
}

/// Automatically select the best available network interface for XDP
pub fn auto_select_interface() -> Result<String> {
    let interfaces = list_network_interfaces()?;
    
    if interfaces.is_empty() {
        return Err(anyhow!("No network interfaces found on system"));
    }
    
    // Strategy: Prefer non-loopback interfaces that are up
    // Fallback to any non-loopback interface
    // Last resort: loopback
    
    // First preference: non-loopback interfaces that are up
    for interface in &interfaces {
        if !interface.is_loopback && interface.is_up {
            info!("Auto-selected network interface '{}' (up, non-loopback)", interface.name);
            return Ok(interface.name.clone());
        }
    }
    
    // Second preference: any non-loopback interface
    for interface in &interfaces {
        if !interface.is_loopback {
            warn!("Auto-selected network interface '{}' (down, but non-loopback)", interface.name);
            return Ok(interface.name.clone());
        }
    }
    
    // Last resort: loopback interface
    for interface in &interfaces {
        if interface.is_loopback {
            warn!("Auto-selected loopback interface '{}' - only interface available", interface.name);
            return Ok(interface.name.clone());
        }
    }
    
    Err(anyhow!("No suitable network interface found"))
}

/// Get a user-friendly list of available interfaces for error messages
pub fn get_available_interfaces_string() -> String {
    match list_network_interfaces() {
        Ok(interfaces) => {
            if interfaces.is_empty() {
                "none".to_string()
            } else {
                interfaces.iter()
                    .map(|iface| {
                        let status = if iface.is_up { "up" } else { "down" };
                        let type_info = if iface.is_loopback {
                            "loopback"
                        } else if iface.is_wireless {
                            "wireless"
                        } else {
                            "ethernet"
                        };
                        format!("{} ({}:{})", iface.name, type_info, status)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        Err(_) => "unable to detect".to_string(),
    }
}

/// Smart interface selection with validation and helpful error messages
pub fn select_and_validate_interface(configured_interface: Option<&str>) -> Result<String> {
    match configured_interface {
        Some(interface_name) => {
            // User specified an interface - validate it exists
            match validate_interface_for_xdp(interface_name) {
                Ok(_) => {
                    info!("Using configured network interface: {}", interface_name);
                    Ok(interface_name.to_string())
                }
                Err(_) => {
                    let available = get_available_interfaces_string();
                    Err(anyhow!(
                        "Configured network interface '{}' not found.\n\
                        Available interfaces: {}.\n\
                        Please update the 'network_interface' setting in your configuration file.",
                        interface_name, available
                    ))
                }
            }
        }
        None => {
            // No interface configured - try auto-selection
            match auto_select_interface() {
                Ok(interface_name) => {
                    info!("Auto-selected network interface: {}", interface_name);
                    Ok(interface_name)
                }
                Err(_) => {
                    let available = get_available_interfaces_string();
                    Err(anyhow!(
                        "No network interface configured and auto-selection failed.\n\
                        Available interfaces: {}.\n\
                        Please specify a 'network_interface' in your configuration file.",
                        available
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_interfaces() {
        // This test may fail on systems without /sys/class/net
        if let Ok(interfaces) = list_network_interfaces() {
            assert!(!interfaces.is_empty(), "Should find at least loopback interface");
            assert!(interfaces.iter().any(|i| i.name == "lo"), "Should find loopback interface");
        }
    }

    #[test]
    fn test_loopback_interface() {
        if let Ok(lo) = get_interface_info("lo") {
            assert!(lo.is_loopback);
            assert_eq!(lo.name, "lo");
        }
    }

    #[test]
    fn test_nonexistent_interface() {
        let result = get_interface_info("nonexistent_interface_12345");
        assert!(result.is_err());
    }
}