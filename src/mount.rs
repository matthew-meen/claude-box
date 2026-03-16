use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::tools::BinaryDirShare;

/// A single VirtioFS directory share between host and guest.
#[derive(Debug, Clone)]
pub struct DirShare {
    /// Absolute path on the host.
    pub host_path: PathBuf,
    /// Tag used by tart (`--dir=<tag>:<host_path>`).
    /// The guest mounts the share using this tag.
    pub tag: String,
    /// The path where the share will be mounted inside the guest.
    pub guest_mount: PathBuf,
    /// Mount read-only in the guest (`--dir=<tag>:<path>:ro`).
    pub read_only: bool,
}

impl DirShare {
    /// Render as the `--dir=<host_path>:<options>` flag value for `tart run`.
    ///
    /// Tart's `--dir` format: `PATH:tag=TAG` (rw) or `PATH:ro,tag=TAG` (ro).
    /// The `tag=` option sets the VirtioFS mount tag used by `mount_virtiofs`
    /// inside the guest, overriding tart's default automount tag.
    pub fn tart_flag(&self) -> String {
        if self.read_only {
            format!("{}:ro,tag={}", self.host_path.display(), self.tag)
        } else {
            format!("{}:tag={}", self.host_path.display(), self.tag)
        }
    }
}

/// Build the set of VirtioFS shares for a sandbox run.
///
/// Shares:
/// 1. The user's project directory (read-write, same absolute path).
/// 2. One read-only share per unique binary source directory (from `binary_shares`),
///    mounted at `/opt/claude-box/src-N/`. `guest_setup` creates a symlink farm at
///    `/opt/claude-box/bin/` exposing only the named binaries on PATH.
/// 3. The MCP config staging directory (mounted at `/opt/claude-box/config`), if any.
/// 4. The directory containing the host `claude` binary (read-only,
///    mounted at `/opt/claude-box/claude`).
pub fn build_shares(
    project_dir: &Path,
    binary_shares: &[BinaryDirShare],
    config_staging_dir: Option<&Path>,
    claude_bin_dir: &Path,
) -> Vec<DirShare> {
    let mut shares = vec![DirShare {
        host_path: project_dir.to_path_buf(),
        tag: "project".to_string(),
        guest_mount: project_dir.to_path_buf(),
        read_only: false,
    }];

    // One read-only share per unique binary source directory.
    // Guest path: /opt/claude-box/src-0, /opt/claude-box/src-1, …
    for (i, bin_share) in binary_shares.iter().enumerate() {
        shares.push(DirShare {
            host_path: bin_share.host_dir.clone(),
            tag: bin_share.tag.clone(),
            guest_mount: PathBuf::from(format!("/opt/claude-box/src-{i}")),
            read_only: true,
        });
    }

    if let Some(config_dir) = config_staging_dir {
        shares.push(DirShare {
            host_path: config_dir.to_path_buf(),
            tag: "claude-box-config".to_string(),
            guest_mount: PathBuf::from("/opt/claude-box/config"),
            read_only: false,
        });
    }

    shares.push(DirShare {
        host_path: claude_bin_dir.to_path_buf(),
        tag: "claude-box-claude".to_string(),
        guest_mount: PathBuf::from("/opt/claude-box/claude"),
        read_only: true,
    });

    shares
}

/// Find the real `claude` binary on PATH (not claude-box itself).
///
/// Returns the canonicalized path so that when its parent directory is
/// mounted as a VirtioFS share, the file is accessible by name inside the
/// VM without dangling symlinks.
pub fn find_claude_binary() -> Result<PathBuf> {
    let own_exe = std::env::current_exe().unwrap_or_default();
    for entry in std::env::var("PATH").unwrap_or_default().split(':') {
        let candidate = PathBuf::from(entry).join("claude");
        if candidate.exists() && candidate != own_exe {
            // Canonicalize to resolve symlinks; the canonical path's parent
            // directory is the directory we'll share via VirtioFS.
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    anyhow::bail!(
        "could not find 'claude' binary on PATH — install Claude Code alongside claude-box"
    )
}
