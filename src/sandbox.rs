use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::time::Duration;
use tokio::process::Child;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::SandboxConfig;
use crate::mount::{self, DirShare};
use crate::relay::SshSession;
use crate::tools;
use crate::vm::{
    image::{resolve_image, validate_image},
    ssh_key,
    tart::TartVm,
    warm_pool,
    Vm, VmConfig,
};
use crate::vm::{guest_setup, health};

// ---------------------------------------------------------------------------
// Spinner — ticking progress indicator shown during sandbox setup.
// Cleared before claude's output starts.
// ---------------------------------------------------------------------------

struct Spinner(ProgressBar);

impl Spinner {
    fn start(msg: &str) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.enable_steady_tick(Duration::from_millis(80));
        pb.set_message(msg.to_string());
        Self(pb)
    }

    fn set(&self, msg: &'static str) {
        self.0.set_message(msg);
    }

    fn finish(self) {
        self.0.finish_and_clear();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if !self.0.is_finished() {
            self.0.finish_and_clear();
        }
    }
}

pub struct Sandbox {
    config: SandboxConfig,
}

// ---------------------------------------------------------------------------
// VM cleanup guard — guarantees stop+delete on any failure after vm.create().
// ---------------------------------------------------------------------------

struct VmGuard<'a> {
    vm: &'a TartVm,
    name: String,
    persist: bool,
    child: Option<Child>,
    disarmed: bool,
}

impl<'a> VmGuard<'a> {
    fn new(vm: &'a TartVm, name: String, persist: bool) -> Self {
        Self {
            vm,
            name,
            persist,
            child: None,
            disarmed: false,
        }
    }

    fn set_child(&mut self, child: Child) {
        self.child = Some(child);
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// Best-effort cleanup: stop the VM, wait for the child, delete the VM.
    async fn cleanup(&mut self) {
        if self.disarmed || self.persist {
            return;
        }
        warn!("cleaning up VM {} after failure", self.name);
        if let Err(e) = self.vm.stop(&self.name).await {
            warn!("cleanup: tart stop failed (may not be running): {e}");
        }
        if let Some(ref mut child) = self.child {
            let _ = child.wait().await;
        }
        if let Err(e) = self.vm.delete(&self.name).await {
            warn!("cleanup: tart delete failed: {e}");
        }
    }
}

impl Sandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self { config }
    }

    /// Run the full sandbox lifecycle and return the exit code from `claude`.
    pub async fn run(self) -> Result<i32> {
        let config = &self.config;

        // 0. Reap stale orphan VMs from previous crashed runs.
        if let Err(e) = crate::gc::reap_stale_vms(std::time::Duration::from_secs(3600)).await {
            warn!("orphan reap failed (non-fatal): {e}");
        }

        // 1. Resolve image (may show its own pull progress bar).
        let image_ref = config
            .vm_image
            .as_deref()
            .context("--vm-image / CLAUDE_BOX_IMAGE required")?;
        let base_image = resolve_image(image_ref, &config.pull_policy).await?;

        // Start the setup spinner now that image is resolved.
        let spinner = Spinner::start(if config.validate_image {
            "Validating image…"
        } else {
            "Cloning VM…"
        });

        // 1b. Optionally validate image via smoke boot.
        if config.validate_image {
            validate_image(&base_image).await?;
            spinner.set("Cloning VM…");
        }

        // 2. Generate ephemeral SSH keypair.
        let key = ssh_key::generate().context("failed to generate SSH keypair")?;

        // 3. Prepare tool environment (MCP filter + binary resolution).
        let tool_env =
            tools::prepare(&config.allow_tools, &config.allow_binaries, &config.mount)?;

        // 4. Find host claude binary.
        let claude_bin = mount::find_claude_binary()?;
        let claude_bin_dir = claude_bin
            .parent()
            .context("claude binary has no parent dir")?
            .to_path_buf();

        // 5. Build VirtioFS shares.
        let config_staging_path = tool_env.config_staging_dir.path().to_path_buf();
        let shares = mount::build_shares(
            &config.mount,
            &tool_env.binary_shares,
            Some(&config_staging_path),
            &claude_bin_dir,
        );

        // 6. Try warm pool if enabled (no explicit vm_name, no binary allowlist —
        //    binary allowlists require more than 1 VirtioFS share which is
        //    incompatible with VZF snapshot/restore reliability).
        if config.warm_pool
            && config.vm_name.is_none()
            && !config.persist
            && tool_env.binary_shares.is_empty()
        {
            spinner.set("Starting VM…");
            match warm_pool::try_warm_boot(
                image_ref,
                &base_image,
                &config.mount,
                &claude_bin,
            )
            .await
            {
                Ok((run_name, session, child)) => {
                    return run_warm(
                        run_name, session, child, &tool_env, &config.mount, config, spinner,
                    )
                    .await;
                }
                Err(e) => {
                    warn!("warm boot failed ({e:#}), falling back to cold boot");
                    spinner.set("Cloning VM…");
                }
            }
        }

        // 7. Cold boot: clone VM.
        let vm_name = config
            .vm_name
            .clone()
            .unwrap_or_else(|| format!("claude-box-{}", Uuid::new_v4()));

        let vm = TartVm::new();
        let vm_config = VmConfig {
            name: vm_name.clone(),
            base_image,
            dir_shares: shares
                .iter()
                .map(|s| (s.host_path.clone(), s.tag.clone()))
                .collect(),
        };
        vm.create(&vm_config).await?;

        // -- From here on, the VM exists on disk.  Guard ensures cleanup. --
        let mut guard = VmGuard::new(&vm, vm_name.clone(), config.persist);

        let result =
            run_inner(&mut guard, &vm_name, &key, &tool_env, &shares, config, spinner).await;

        match result {
            Ok(exit_code) => {
                // Happy path: explicit stop + delete.
                guard.disarm();
                vm.stop(&vm_name).await?;
                if let Some(ref mut child) = guard.child {
                    let _ = child.wait().await;
                }
                if !config.persist {
                    vm.delete(&vm_name).await?;
                }
                Ok(exit_code)
            }
            Err(e) => {
                guard.cleanup().await;
                Err(e)
            }
        }
    }
}

/// Run after a successful warm boot: mount project VirtioFS → send config → claude → stop/delete.
///
/// In the warm path only 1 VirtioFS share is used (project directory). The
/// claude binary is baked into the warm VM disk (no VirtioFS share needed).
/// Binary allowlists are not supported with the warm pool; the caller must
/// ensure binary_shares is empty before calling this.
async fn run_warm(
    run_name: String,
    session: SshSession,
    mut child: Child,
    tool_env: &tools::ToolEnv,
    project_dir: &Path,
    config: &SandboxConfig,
    spinner: Spinner,
) -> Result<i32> {
    let vm = TartVm::new();

    // Build a project-only DirShare for VirtioFS mounting.
    let project_share = DirShare {
        host_path: project_dir.to_path_buf(),
        tag: "project".to_string(),
        guest_mount: project_dir.to_path_buf(),
        read_only: false,
    };

    // Mount the project VirtioFS share. No binary shares — not supported in warm path.
    spinner.set("Configuring guest…");
    let setup_result =
        guest_setup::setup_guest(&session, &[project_share], None, &[]).await;

    if let Err(e) = setup_result {
        warn!("warm boot guest_setup failed ({e:#}), cleaning up");
        let _ = vm.stop(&run_name).await;
        let _ = child.wait().await;
        let _ = vm.delete(&run_name).await;
        return Err(e).context("guest_setup on warm clone");
    }

    // Transfer config files via SSH (no VirtioFS config mount in warm path).
    {
        let staging = tool_env.config_staging_dir.path();

        // settings.json → ~/.claude/settings.json
        let settings_path = staging.join("settings.json");
        if settings_path.exists() {
            let content = std::fs::read_to_string(&settings_path)
                .context("read settings.json for warm SSH transfer")?;
            let escaped = content.replace('\'', "'\\''");
            let cmd = format!(
                "mkdir -p ~/.claude && printf '%s' '{}' > ~/.claude/settings.json",
                escaped
            );
            let code = session.exec(&cmd).await?;
            if code != 0 {
                warn!("warm settings.json SSH transfer exited {code}");
            }
        }

        // claude.json → ~/.claude.json
        let claude_json_path = staging.join("claude.json");
        if claude_json_path.exists() {
            let content = std::fs::read_to_string(&claude_json_path)
                .context("read claude.json for warm SSH transfer")?;
            let escaped = content.replace('\'', "'\\''");
            let cmd = format!("printf '%s' '{}' > ~/.claude.json", escaped);
            let code = session.exec(&cmd).await?;
            if code != 0 {
                warn!("warm claude.json SSH transfer exited {code}");
            }
        }
    }

    // Execute claude (binary is at /opt/claude-box/claude/claude, baked into disk).
    let result = exec_claude(&session, tool_env, config, spinner).await;

    // Teardown regardless of claude exit.
    let _ = vm.stop(&run_name).await;
    let _ = child.wait().await;
    let _ = vm.delete(&run_name).await;

    result
}

/// Inner lifecycle after vm.create() — separated so errors trigger guard cleanup.
async fn run_inner(
    guard: &mut VmGuard<'_>,
    vm_name: &str,
    key: &ssh_key::EphemeralKey,
    tool_env: &tools::ToolEnv,
    shares: &[DirShare],
    config: &SandboxConfig,
    spinner: Spinner,
) -> Result<i32> {
    // 8. Inject SSH key into cloned disk (before boot).
    spinner.set("Preparing disk…");
    ssh_key::inject_key(vm_name, &key.authorized_keys_line).await?;

    // 9. Boot VM in background with VirtioFS shares.
    spinner.set("Starting VM…");
    info!("starting VM {vm_name}");
    let mut vm_child = start_vm_with_shares(vm_name, shares).await?;
    // Take the piped stderr handle before moving the child into the guard.
    // If tart exits before SSH is ready we can surface the error output.
    let mut tart_stderr = vm_child.stderr.take();
    guard.set_child(vm_child);

    // 10. Get VM IP and wait for SSH.
    let ip = match health::get_vm_ip(vm_name).await {
        Ok(ip) => ip,
        Err(e) => {
            let extra = drain_tart_stderr(&mut tart_stderr).await;
            if extra.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("tart run stderr:\n{extra}")));
        }
    };
    spinner.set("Waiting for SSH…");
    let session = match health::wait_for_ssh(&ip, &key.keypair, 120).await {
        Ok(s) => s,
        Err(e) => {
            let extra = drain_tart_stderr(&mut tart_stderr).await;
            if extra.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("tart run stderr:\n{extra}")));
        }
    };

    // 11. Guest setup: mount VirtioFS shares, copy config, build symlink farm.
    spinner.set("Configuring guest…");
    guest_setup::setup_guest(
        &session,
        shares,
        Some(std::path::Path::new("/opt/claude-box/config")),
        &tool_env.binary_shares,
    )
    .await?;

    // 12. Execute claude inside the VM.
    exec_claude(&session, tool_env, config, spinner).await
}

/// Resolve an API key for the guest VM. Checked in order:
/// 1. `apiKeyHelper` from host settings — run on host, capture stdout
/// 2. `ANTHROPIC_API_KEY` env var — forward directly
/// 3. macOS Keychain — read the API key stored by Claude CLI's OAuth flow
///
/// Returns `None` if no credential source is available.
async fn resolve_api_key(tool_env: &tools::ToolEnv) -> Result<Option<String>> {
    // 1. apiKeyHelper (explicit credential helper command).
    if let Some(ref helper) = tool_env.api_key_helper {
        let out = tokio::process::Command::new("sh")
            .args(["-c", helper])
            .output()
            .await
            .context("failed to run apiKeyHelper")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("apiKeyHelper exited non-zero: {}", stderr.trim());
        }
        let key = String::from_utf8(out.stdout)
            .context("apiKeyHelper output not UTF-8")?
            .trim()
            .to_string();
        anyhow::ensure!(!key.is_empty(), "apiKeyHelper returned empty output");
        return Ok(Some(key));
    }

    // 2. ANTHROPIC_API_KEY env var.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        return Ok(Some(key));
    }

    // 3. macOS Keychain — Claude CLI stores the OAuth-minted API key under
    //    service "Claude Code", account = macOS username.
    if let Some(key) = read_keychain_api_key().await {
        return Ok(Some(key));
    }

    Ok(None)
}

/// Try to read the Claude API key from macOS Keychain.
/// Returns `None` (no error) if the entry doesn't exist or can't be read.
async fn read_keychain_api_key() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code", "-a", &user, "-w"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let key = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if key.is_empty() || !key.starts_with("sk-") {
        return None;
    }
    info!("using API key from macOS Keychain (Claude Code OAuth)");
    Some(key)
}

async fn exec_claude(
    session: &SshSession,
    tool_env: &tools::ToolEnv,
    config: &SandboxConfig,
    spinner: Spinner,
) -> Result<i32> {
    // Resolve auth before clearing the spinner (helper may take a moment).
    let api_key = resolve_api_key(tool_env).await?;

    // Clear the spinner before claude starts writing to the terminal.
    spinner.finish();

    let mut parts: Vec<String> = Vec::new();

    // Inject ANTHROPIC_API_KEY if we have one.
    if let Some(key) = api_key {
        parts.push(format!("export ANTHROPIC_API_KEY={} &&", shell_escape(&key)));
    }

    // Prepend ~/.local/bin (where we symlink the Claude binary for the
    // native-install check) and the optional binary-shares symlink farm.
    {
        let mut extra_paths = vec!["$HOME/.local/bin".to_string()];
        if !tool_env.binary_shares.is_empty() {
            extra_paths.push("/opt/claude-box/bin".to_string());
        }
        parts.push(format!(
            "export PATH={}:$PATH &&",
            extra_paths.join(":")
        ));
    }

    // cd to the mounted project dir.
    parts.push(format!(
        "cd {} &&",
        shell_escape(&config.mount.to_string_lossy())
    ));

    // Invoke claude (mounted from host at /opt/claude-box/claude/claude).
    // Always skip permissions — the VM IS the sandbox.
    parts.push("/opt/claude-box/claude/claude --dangerously-skip-permissions".to_string());
    parts.extend(config.claude_args.iter().map(|a| shell_escape(a)));

    let cmd = parts.join(" ");
    crate::relay::Relay::run(session, &cmd).await
}

/// Spawn `tart run` in the background with `--dir` flags for each VirtioFS share.
///
/// Returns the child process handle — caller must hold it until VM shutdown
/// and call `.wait()` after `tart stop`. Stderr is piped so callers can
/// capture and surface it if the VM exits before SSH is established.
async fn start_vm_with_shares(vm_name: &str, shares: &[DirShare]) -> Result<Child> {
    let mut args = vec!["run".to_string(), "--no-graphics".to_string()];
    for share in shares {
        args.push(format!("--dir={}", share.tart_flag()));
    }
    args.push(vm_name.to_string());

    let child = tokio::process::Command::new("tart")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn tart run")?;

    Ok(child)
}

/// Drain up to 4 KiB from a piped child stderr handle (non-blocking, 200ms timeout).
/// Returns trimmed output or an empty string if nothing is available.
async fn drain_tart_stderr(
    handle: &mut Option<tokio::process::ChildStderr>,
) -> String {
    use tokio::io::AsyncReadExt;
    let Some(stderr) = handle else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        stderr.read_to_end(&mut buf),
    )
    .await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
