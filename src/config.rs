use std::path::PathBuf;

use crate::vm::image::PullPolicy;

/// Full configuration for a single sandbox run.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// OCI image ref (e.g. `ghcr.io/cirruslabs/macos-sequoia-base:latest`)
    /// or absolute path to a local `.ipsw` file.
    /// When `None` the user must have `CLAUDE_BOX_IMAGE` set.
    pub vm_image: Option<String>,

    /// Host directory to mount into the VM via VirtioFS at the same absolute path.
    pub mount: PathBuf,

    /// MCP server names to allow inside the VM.
    /// Empty vec means "pass all servers through".
    pub allow_tools: Vec<String>,

    /// Host binary names to forward into the VM.
    /// Each name is resolved via `which` and staged for VirtioFS mounting.
    pub allow_binaries: Vec<String>,

    /// When `true`, the VM is not deleted after the run.
    pub persist: bool,

    /// Explicit VM name; if `None` a `claude-box-<uuid>` name is generated.
    pub vm_name: Option<String>,

    /// Arguments forwarded verbatim to `claude` inside the VM.
    pub claude_args: Vec<String>,

    /// Controls when `tart pull` is invoked.
    pub pull_policy: PullPolicy,

    /// When `true`, smoke-boot the base image before cloning (~30s overhead).
    pub validate_image: bool,

    /// Use the warm VM pool for faster startup (default: true).
    /// Set to false with `--no-warm` to force cold boot.
    pub warm_pool: bool,
}
