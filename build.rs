use std::{env, fs, path::PathBuf, process::Command};
use anyhow::{Result, anyhow, Context};

fn main() -> Result<()> {
    println!("cargo:warning=Building eBPF program from source...");
    
    // Set up paths
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| anyhow!("CARGO_MANIFEST_DIR not set"))?;
    let out_dir = env::var("OUT_DIR")
        .map_err(|_| anyhow!("OUT_DIR not set"))?;
    let ebpf_dir = PathBuf::from(&manifest_dir).join("portail-ebpf");
    
    // Add dependency tracking
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", ebpf_dir.join("Cargo.toml").display());
    
    // Determine target architecture  
    let endian = env::var("CARGO_CFG_TARGET_ENDIAN")
        .map_err(|_| anyhow!("CARGO_CFG_TARGET_ENDIAN not set"))?;
    let target = if endian == "big" {
        "bpfeb-unknown-none"
    } else if endian == "little" {
        "bpfel-unknown-none"
    } else {
        return Err(anyhow!("unsupported endian={endian:?}"));
    };
    
    let arch = env::var("CARGO_CFG_TARGET_ARCH")
        .map_err(|_| anyhow!("CARGO_CFG_TARGET_ARCH not set"))?;
    
    // Build the eBPF program directly
    let mut cmd = Command::new("cargo");
    cmd.args([
        "+nightly",
        "build",
        "--release",
        "--target", target,
        "-Z", "build-std=core",
        "--bin", "sk_reuseport"
    ])
    .current_dir(&ebpf_dir)
    .env("CARGO_CFG_BPF_TARGET_ARCH", &arch)
    .env("RUSTFLAGS", "-C debuginfo=2 -C link-arg=--btf");
    
    // Remove RUSTC env vars that might interfere with nightly toolchain selection
    cmd.env_remove("RUSTC")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    
    println!("cargo:warning=Running: {:?}", cmd);
    let output = cmd.output()
        .with_context(|| "failed to execute cargo build for eBPF".to_string())?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!("eBPF build failed:\nstdout: {}\nstderr: {}", stdout, stderr));
    }
    
    // Find and copy the built binary
    let ebpf_binary_path = ebpf_dir
        .join("target")
        .join(target)
        .join("release")
        .join("sk_reuseport");
    
    if !ebpf_binary_path.exists() {
        return Err(anyhow!("eBPF binary not found at: {}", ebpf_binary_path.display()));
    }
    
    // Copy to OUT_DIR where the main program expects it
    let dest_path = PathBuf::from(out_dir).join("sk_reuseport");
    fs::copy(&ebpf_binary_path, &dest_path)
        .with_context(|| format!("failed to copy eBPF binary to {}", dest_path.display()))?;
    
    let binary_size = fs::metadata(&dest_path)?.len();
    println!("cargo:warning=✅ eBPF program built and embedded successfully ({} bytes)", binary_size);
    
    Ok(())
}
