use anyhow::{bail, Context, Result};
use std::time::Duration;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info};

use super::{Vm, VmConfig};

/// Default timeout for tart operations, by subcommand.
fn default_timeout(subcommand: &str) -> Duration {
    match subcommand {
        "clone" => Duration::from_secs(300),
        "stop" | "delete" => Duration::from_secs(30),
        _ => Duration::from_secs(60),
    }
}

/// Read an override timeout from `CLAUDE_BOX_TART_TIMEOUT_SECS`.
/// Returns `None` if the env var is absent or unparseable.
fn env_timeout_override() -> Option<Duration> {
    std::env::var("CLAUDE_BOX_TART_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// VM backend that delegates all lifecycle operations to the `tart` CLI.
pub struct TartVm;

impl TartVm {
    pub fn new() -> Self {
        Self
    }

    async fn run_tart(&self, args: &[&str]) -> Result<()> {
        let subcommand = args.first().copied().unwrap_or("unknown");
        let timeout = env_timeout_override().unwrap_or_else(|| default_timeout(subcommand));

        debug!("tart {} (timeout {}s)", args.join(" "), timeout.as_secs());

        let future = Command::new("tart")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();

        let output = tokio::time::timeout(timeout, future)
            .await
            .with_context(|| format!("tart {subcommand} timed out after {}s", timeout.as_secs()))?
            .context("failed to spawn tart")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                bail!("tart {} exited with {}", subcommand, output.status);
            } else {
                bail!("tart {} failed:\n{}", subcommand, detail);
            }
        }
        Ok(())
    }
}

impl Default for TartVm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm for TartVm {
    async fn create(&self, config: &VmConfig) -> Result<()> {
        info!("cloning {} -> {}", config.base_image, config.name);
        self.run_tart(&["clone", &config.base_image, &config.name])
            .await
    }

    async fn stop(&self, name: &str) -> Result<()> {
        info!("stopping VM {name}");
        self.run_tart(&["stop", name]).await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        info!("deleting VM {name}");
        self.run_tart(&["delete", name]).await
    }
}
