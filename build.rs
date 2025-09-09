use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=uringress-ebpf/src/main.rs");
    println!("cargo:rerun-if-changed=uringress-ebpf/.cargo/config.toml");
    
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let dest = format!("{}/sk_reuseport", out_dir);
    
    // Always ensure we have the latest eBPF binary embedded
    println!("cargo:warning=Ensuring latest eBPF program is embedded...");
    
    let workspace_src = "uringress-ebpf/target/bpfel-unknown-none/release/sk_reuseport";
    let mut copied = false;
    
    if Path::new(workspace_src).exists() {
        match std::fs::copy(workspace_src, &dest) {
            Ok(bytes) => {
                println!("cargo:warning=✅ eBPF program embedded: {} ({} bytes)", workspace_src, bytes);
                copied = true;
            }
            Err(e) => {
                println!("cargo:warning=❌ Failed to copy eBPF binary: {}", e);
            }
        }
    } else {
        println!("cargo:warning=❌ eBPF binary not found at: {}", workspace_src);
        println!("cargo:warning=Please build it first with:");
        println!("cargo:warning=  cd uringress-ebpf && cargo +nightly build --release -Z build-std=core");
    }
    
    if !copied {
        println!("cargo:warning=eBPF binary not found at any location, creating empty stub");
        println!("cargo:warning=To build with working eBPF:");
        println!("cargo:warning=  1. cd uringress-ebpf");
        println!("cargo:warning=  2. cargo +nightly build --release -Z build-std=core");
        println!("cargo:warning=  3. cd .. && cargo build --release");
        std::fs::write(&dest, b"").unwrap();
    }
}
