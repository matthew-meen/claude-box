use anyhow::{Context, Result};
use russh_keys::key::KeyPair;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tracing::{debug, info};

use crate::relay::SshSession;

/// Get the IP address of a running tart VM.
pub async fn get_vm_ip(vm_name: &str) -> Result<String> {
    let timeout = Duration::from_secs(60);
    let output = tokio::time::timeout(
        timeout,
        Command::new("tart")
            .args(["ip", vm_name, "--wait", "30"])
            .output(),
    )
    .await
    .context("tart ip timed out")?
    .context("failed to run tart ip")?;
    anyhow::ensure!(output.status.success(), "tart ip failed for {vm_name}");
    let ip = String::from_utf8(output.stdout)
        .context("tart ip output not utf8")?
        .trim()
        .to_string();
    anyhow::ensure!(!ip.is_empty(), "tart ip returned empty string for {vm_name}");
    Ok(ip)
}

/// Compute a sleep duration with exponential backoff and jitter.
///
/// Base interval starts at 200ms, doubles each attempt, caps at 8s.
/// Jitter of ±25% is applied using system clock nanos (no `rand` dep).
fn backoff_duration(attempt: u32) -> Duration {
    let exp = attempt.min(5); // 2^5 × 200ms = 6.4s, effectively capped at 8s
    let base_ms: u64 = 200 * (1u64 << exp);
    let jitter_range = base_ms / 4; // ±25%
    // Cheap pseudo-jitter from system clock nanoseconds.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let jitter_offset = if jitter_range > 0 {
        nanos % (jitter_range * 2)
    } else {
        0
    };
    let ms = base_ms - jitter_range + jitter_offset;
    Duration::from_millis(ms)
}

/// Poll until an SSH connection succeeds or timeout expires.
/// Returns the established SshSession.
///
/// Uses exponential backoff (200ms → 8s) with ±25% jitter to avoid
/// thundering-herd effects when many VMs boot concurrently.
pub async fn wait_for_ssh(ip: &str, key: &KeyPair, timeout_secs: u64) -> Result<SshSession> {
    info!("waiting for SSH on {ip}");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match SshSession::connect(ip, key).await {
            Ok(session) => {
                info!("SSH ready on {ip} after {attempt} attempts");
                return Ok(session);
            }
            Err(e) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("SSH on {ip} not ready after {timeout_secs}s: {e}");
                }
                let delay = backoff_duration(attempt - 1);
                debug!("SSH attempt {attempt} failed: {e} (retry in {}ms)", delay.as_millis());
                tokio::time::sleep(delay).await;
            }
        }
    }
}
