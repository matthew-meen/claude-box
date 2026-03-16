use anyhow::Result;
use std::path::Path;
use tracing::{debug, info};

use crate::mount::DirShare;
use crate::relay::SshSession;
use crate::tools::BinaryDirShare;

/// Run all guest-side setup after SSH is ready:
/// 1. Mount each VirtioFS share at its intended guest path.
/// 2. Ensure ~/.local/bin exists (silences Claude CLI native-install warning).
/// 3. Copy MCP config to ~/.claude/settings.json if present.
/// 4. Create a symlink farm at /opt/claude-box/bin/ for allowed binaries.
pub async fn setup_guest(
    session: &SshSession,
    shares: &[DirShare],
    mcp_config_path: Option<&Path>,
    binary_shares: &[BinaryDirShare],
) -> Result<()> {
    // ── 1. Mount each VirtioFS share ─────────────────────────────────────────
    for share in shares {
        let guest_path = share.guest_mount.to_string_lossy();
        let tag = &share.tag;
        info!("mounting VirtioFS share {tag} at {guest_path}");

        let code = session
            .exec(&format!("sudo mkdir -p '{guest_path}'"))
            .await?;
        if code != 0 {
            anyhow::bail!("mkdir -p {guest_path} exited {code}");
        }

        let code = session
            .exec(&format!("sudo mount_virtiofs '{tag}' '{guest_path}'"))
            .await?;
        if code != 0 {
            anyhow::bail!("mount_virtiofs {tag} -> {guest_path} exited {code}");
        }
    }

    // ── 2. Create ~/.local/bin/claude symlink ──────────────────────────────
    // The Claude CLI checks for ~/.local/bin/claude when installMethod is
    // "native". Since we mount the binary at /opt/claude-box/claude/claude,
    // create a symlink so the CLI's self-check passes.
    session
        .exec("mkdir -p ~/.local/bin && ln -sf /opt/claude-box/claude/claude ~/.local/bin/claude")
        .await?;

    // ── 2b. Persist PATH additions in ~/.zshenv ─────────────────────────
    // Claude Code's Bash tool spawns subshells (often zsh login shells) that
    // re-read /etc/zprofile → path_helper, resetting PATH.  ~/.zshenv is
    // sourced by ALL zsh invocations (login, interactive, scripts) so PATH
    // additions here survive subshell spawns.
    {
        let mut extra_dirs = vec!["$HOME/.local/bin".to_string()];
        if !binary_shares.is_empty() {
            extra_dirs.push("/opt/claude-box/bin".to_string());
        }
        let path_line = format!(
            "export PATH={}:$PATH",
            extra_dirs.join(":")
        );
        let cmd = format!(
            "echo '{}' >> ~/.zshenv",
            path_line
        );
        session.exec(&cmd).await?;
    }

    // ── 3. Copy Claude CLI config files ────────────────────────────────────
    if let Some(config_path) = mcp_config_path {
        let config = config_path.to_string_lossy();

        // settings.json → ~/.claude/settings.json
        let settings_src = format!("{config}/settings.json");
        let cmd = format!(
            "if [ -f '{settings_src}' ]; then mkdir -p ~/.claude && cp '{settings_src}' ~/.claude/settings.json; fi"
        );
        session.exec(&cmd).await?;

        // claude.json → ~/.claude.json (onboarding/display prefs)
        let claude_src = format!("{config}/claude.json");
        let cmd = format!(
            "if [ -f '{claude_src}' ]; then cp '{claude_src}' ~/.claude.json; fi"
        );
        session.exec(&cmd).await?;
    }

    // ── 4. Build symlink farm for allowed binaries ───────────────────────────
    // /opt/claude-box/bin/ is a plain directory (not a VirtioFS share) containing
    // only symlinks to the specifically named binaries within their source mounts.
    // This enforces the allowlist: sibling files in the mounted directories are
    // accessible at their src-N paths but never appear on the guest PATH.
    if !binary_shares.is_empty() {
        let code = session
            .exec("sudo mkdir -p /opt/claude-box/bin")
            .await?;
        if code != 0 {
            anyhow::bail!("mkdir /opt/claude-box/bin exited {code}");
        }

        for (i, share) in binary_shares.iter().enumerate() {
            for (name, canonical) in share.names.iter().zip(share.canonical_filenames.iter()) {
                // src: actual filename inside the VirtioFS-mounted directory.
                // dst: name exposed on the guest PATH (original command name).
                let src = format!("/opt/claude-box/src-{i}/{canonical}");
                let dst = format!("/opt/claude-box/bin/{name}");
                let cmd = format!("sudo ln -sf '{src}' '{dst}'");
                let code = session.exec(&cmd).await?;
                if code != 0 {
                    anyhow::bail!("ln -sf {src} {dst} exited {code}");
                }
                debug!("symlinked {dst} → {src}");
            }
        }
    }

    Ok(())
}
