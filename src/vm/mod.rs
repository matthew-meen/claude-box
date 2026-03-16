pub mod guest_setup;
pub mod health;
pub mod image;
pub mod image_cache;
pub mod ssh_key;
pub mod tart;
pub mod warm_pool;

use anyhow::Result;
use std::path::PathBuf;

/// Configuration passed to the VM backend when creating a sandbox VM.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Name of the VM instance (must be unique on the host).
    pub name: String,

    /// Name of the base image to clone from (already pulled/imported).
    pub base_image: String,

    /// VirtioFS directory shares: (host_path, guest_tag).
    /// tart renders these as `--dir=<tag>:<host_path>`.
    #[allow(dead_code)]
    pub dir_shares: Vec<(PathBuf, String)>,
}

/// Minimal async interface over a VM backend.
#[allow(async_fn_in_trait)]
pub trait Vm {
    /// Clone the base image and create the VM (does not start it).
    async fn create(&self, config: &VmConfig) -> Result<()>;

    /// Stop the VM.
    async fn stop(&self, name: &str) -> Result<()>;

    /// Delete the VM and its disk image.
    async fn delete(&self, name: &str) -> Result<()>;
}
