use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::process::Command;
use tracing::{debug, info, warn};

/// VM name prefix used by claude-box for auto-generated run names.
const VM_PREFIX: &str = "claude-box-";

/// Prefix for warm pool VMs — excluded from automatic GC.
const WARM_PREFIX: &str = "claude-box-warm-";

/// List all tart VMs whose name starts with `claude-box-` but NOT `claude-box-warm-`.
/// Warm VMs are managed by warm_pool.rs and must not be reaped by the GC.
async fn list_claude_box_vms() -> Result<Vec<String>> {
    let output = Command::new("tart")
        .args(["list"])
        .output()
        .await
        .context("failed to run tart list")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // tart list columns: Source  Name  Disk Size  Accessed  State
    // Skip the header row; use nth(1) to get the Name column.
    let vms: Vec<String> = stdout
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|name| name.starts_with(VM_PREFIX) && !name.starts_with(WARM_PREFIX))
        .map(|s| s.to_string())
        .collect();
    Ok(vms)
}

/// Return the modification time of the VM directory (a proxy for creation time).
fn vm_dir_mtime(vm_name: &str) -> Option<SystemTime> {
    let home = std::env::var("HOME").ok()?;
    let vm_dir = PathBuf::from(home).join(".tart").join("vms").join(vm_name);
    std::fs::metadata(&vm_dir)
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Delete stopped orphan VMs older than `max_age`.
///
/// Best-effort: individual failures are logged and swallowed so one bad VM
/// doesn't prevent reaping the rest.
pub async fn reap_stale_vms(max_age: Duration) -> Result<u32> {
    let vms = list_claude_box_vms().await?;
    if vms.is_empty() {
        return Ok(0);
    }

    let now = SystemTime::now();
    let mut reaped = 0u32;

    for name in &vms {
        // Skip VMs younger than max_age.
        if let Some(mtime) = vm_dir_mtime(name) {
            if let Ok(age) = now.duration_since(mtime) {
                if age < max_age {
                    debug!("skipping {name}: only {}s old", age.as_secs());
                    continue;
                }
            }
        }

        // Attempt stop (may already be stopped — that's fine).
        let _ = Command::new("tart")
            .args(["stop", name])
            .status()
            .await;

        // Delete.
        info!("reaping orphan VM {name}");
        match Command::new("tart")
            .args(["delete", name])
            .status()
            .await
        {
            Ok(status) if status.success() => {
                reaped += 1;
            }
            Ok(status) => {
                warn!("tart delete {name} exited with {status}");
            }
            Err(e) => {
                warn!("failed to run tart delete {name}: {e}");
            }
        }
    }

    if reaped > 0 {
        info!("reaped {reaped} orphan VM(s)");
    }
    Ok(reaped)
}

/// Interactive `claude-box gc` subcommand.
pub async fn run_gc_command() -> Result<i32> {
    let vms = list_claude_box_vms().await?;

    if vms.is_empty() {
        println!("No claude-box VMs found.");
        return Ok(0);
    }

    println!("Found {} claude-box VM(s):", vms.len());
    let now = SystemTime::now();
    for name in &vms {
        let age_str = vm_dir_mtime(name)
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|age| format!("{}m ago", age.as_secs() / 60))
            .unwrap_or_else(|| "unknown age".to_string());
        println!("  {name} ({age_str})");
    }

    let mut deleted = 0u32;
    for name in &vms {
        let _ = Command::new("tart")
            .args(["stop", name])
            .status()
            .await;

        match Command::new("tart")
            .args(["delete", name])
            .status()
            .await
        {
            Ok(status) if status.success() => {
                println!("  Deleted {name}");
                deleted += 1;
            }
            Ok(status) => {
                eprintln!("  Warning: tart delete {name} exited with {status}");
            }
            Err(e) => {
                eprintln!("  Warning: failed to delete {name}: {e}");
            }
        }
    }

    println!("Deleted {deleted}/{} VM(s).", vms.len());
    Ok(0)
}
