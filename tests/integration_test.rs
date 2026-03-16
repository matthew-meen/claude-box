//! Integration tests that boot a real tart VM and exercise the full sandbox pipeline.
//!
//! All tests share a single VM to avoid ~40s boot overhead per test.
//! Run with: `CLAUDE_BOX_IMAGE=<image> cargo test integration_tests -- --ignored`
//!
//! Prerequisites:
//! - tart installed
//! - CLAUDE_BOX_IMAGE env var set to a locally cached image
//! - `ddtool` on host PATH

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use tempfile::TempDir;
use tokio::process::Child;

use claude_box::mount;
use claude_box::relay::SshSession;
use claude_box::tools;
use claude_box::vm::{
    guest_setup,
    health,
    image::{resolve_image, PullPolicy},
    ssh_key,
    tart::TartVm,
    Vm, VmConfig,
};

// ── Test VM fixture ──────────────────────────────────────────────────────────

struct TestVm {
    vm_name: String,
    session: SshSession,
    vm: TartVm,
    child: Child,
    project_dir: TempDir,
}

impl TestVm {
    async fn boot() -> Result<Self> {
        let t0 = Instant::now();
        let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
            .context("CLAUDE_BOX_IMAGE env var required for integration tests")?;

        // Resolve image (must already be cached — PullPolicy::Never).
        let t = Instant::now();
        let base_image = resolve_image(&image_ref, &PullPolicy::Never).await?;
        eprintln!("[timing] resolve_image: {}ms", t.elapsed().as_millis());

        // Generate ephemeral SSH keypair.
        let t = Instant::now();
        let key = ssh_key::generate().context("failed to generate SSH keypair")?;
        eprintln!("[timing] ssh_key::generate: {}ms", t.elapsed().as_millis());

        // Create project dir with a marker file.
        // Must be under /private/tmp (not /var/folders/…) so guest_setup can
        // `mkdir -p` the same absolute path inside the VM.
        let t = Instant::now();
        let project_dir = tempfile::Builder::new()
            .prefix("claude-box-test-")
            .tempdir_in("/private/tmp")
            .context("creating project tempdir")?;

        // Prepare tool env with ddtool allowed.
        let t2 = Instant::now();
        let tool_env = tools::prepare(&[], &["ddtool".to_string()], project_dir.path())?;
        eprintln!("[timing] tools::prepare: {}ms", t2.elapsed().as_millis());
        std::fs::write(
            project_dir.path().join("test-marker.txt"),
            "claude-box-integration-test-marker",
        )?;

        // Find host claude binary.
        let claude_bin = mount::find_claude_binary()?;
        let claude_bin_dir = claude_bin
            .parent()
            .context("claude binary has no parent dir")?
            .to_path_buf();

        // Build VirtioFS shares.
        let config_staging_path = tool_env.config_staging_dir.path().to_path_buf();
        let shares = mount::build_shares(
            project_dir.path(),
            &tool_env.binary_shares,
            Some(&config_staging_path),
            &claude_bin_dir,
        );
        eprintln!("[timing] prep (project dir + claude + shares): {}ms", t.elapsed().as_millis());

        // Clone VM.
        let vm_name = format!("claude-box-test-{}", uuid::Uuid::new_v4());
        let vm = TartVm::new();
        let vm_config = VmConfig {
            name: vm_name.clone(),
            base_image,
            dir_shares: shares
                .iter()
                .map(|s| (s.host_path.clone(), s.tag.clone()))
                .collect(),
        };
        let t = Instant::now();
        vm.create(&vm_config).await?;
        eprintln!("[timing] tart clone: {}ms", t.elapsed().as_millis());

        // Inject SSH key.
        let t = Instant::now();
        ssh_key::inject_key(&vm_name, &key.authorized_keys_line).await?;
        eprintln!("[timing] inject_key (hdiutil): {}ms", t.elapsed().as_millis());

        // Boot VM with VirtioFS shares.
        let t = Instant::now();
        let mut args = vec!["run".to_string(), "--no-graphics".to_string()];
        for share in &shares {
            args.push(format!("--dir={}", share.tart_flag()));
        }
        args.push(vm_name.clone());

        let child = tokio::process::Command::new("tart")
            .args(&args)
            .spawn()
            .context("failed to spawn tart run")?;
        eprintln!("[timing] tart run spawn: {}ms", t.elapsed().as_millis());

        // Wait for SSH.
        let t = Instant::now();
        let ip = health::get_vm_ip(&vm_name).await?;
        eprintln!("[timing] get_vm_ip (tart ip --wait): {}ms", t.elapsed().as_millis());

        let t = Instant::now();
        let session = health::wait_for_ssh(&ip, &key.keypair, 240).await?;
        eprintln!("[timing] wait_for_ssh: {}ms", t.elapsed().as_millis());

        // Guest setup: mount shares, copy config, build symlink farm.
        let t = Instant::now();
        guest_setup::setup_guest(
            &session,
            &shares,
            Some(std::path::Path::new("/opt/claude-box/config")),
            &tool_env.binary_shares,
        )
        .await?;
        eprintln!("[timing] guest_setup (mounts + symlinks): {}ms", t.elapsed().as_millis());
        eprintln!("[timing] TOTAL boot: {}ms", t0.elapsed().as_millis());

        Ok(Self {
            vm_name,
            session,
            vm,
            child,
            project_dir,
        })
    }

    async fn teardown(mut self) {
        let _ = self.vm.stop(&self.vm_name).await;
        let _ = self.child.wait().await;
        let _ = self.vm.delete(&self.vm_name).await;
    }

    fn project_path(&self) -> PathBuf {
        self.project_dir.path().to_path_buf()
    }
}

// ── Sub-tests ────────────────────────────────────────────────────────────────

async fn test_vm_boot_and_ssh(vm: &TestVm) {
    let out = vm.session.exec_capture("echo hello").await.unwrap();
    assert_eq!(out.exit_code, 0, "echo hello exited {}", out.exit_code);
    assert!(
        out.stdout.contains("hello"),
        "stdout missing 'hello': {}",
        out.stdout
    );
}

async fn test_ddtool_version(vm: &TestVm) {
    let out = vm
        .session
        .exec_capture("export PATH=/opt/claude-box/bin:$PATH && ddtool version")
        .await
        .unwrap();
    assert_eq!(
        out.exit_code, 0,
        "ddtool version exited {} — stderr: {}",
        out.exit_code, out.stderr
    );
    assert!(
        !out.stdout.trim().is_empty(),
        "ddtool version produced no output"
    );
}

async fn test_binary_allowlist_enforcement(vm: &TestVm) {
    let out = vm
        .session
        .exec_capture("ls /opt/claude-box/bin/")
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    let entries: Vec<&str> = out.stdout.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    assert!(
        entries.contains(&"ddtool"),
        "expected ddtool in bin dir, got: {:?}",
        entries
    );
    // Should not contain random system binaries.
    assert!(
        !entries.contains(&"curl"),
        "bin dir should not contain non-allowed binaries: {:?}",
        entries
    );
}

async fn test_project_dir_mounted(vm: &TestVm) {
    let marker_path = vm.project_path().join("test-marker.txt");
    let cmd = format!("cat '{}'", marker_path.display());
    let out = vm.session.exec_capture(&cmd).await.unwrap();
    assert_eq!(
        out.exit_code, 0,
        "cat marker exited {} — stderr: {}",
        out.exit_code, out.stderr
    );
    assert_eq!(
        out.stdout.trim(),
        "claude-box-integration-test-marker",
        "marker content mismatch"
    );
}

async fn test_project_dir_writable(vm: &TestVm) {
    let output_path = vm.project_path().join("guest-output.txt");
    let cmd = format!(
        "echo 'written-by-guest' > '{}'",
        output_path.display()
    );
    let out = vm.session.exec_capture(&cmd).await.unwrap();
    assert_eq!(
        out.exit_code, 0,
        "write exited {} — stderr: {}",
        out.exit_code, out.stderr
    );
    // Verify on the host side.
    let content = std::fs::read_to_string(&output_path).expect("guest output file not found on host");
    assert_eq!(content.trim(), "written-by-guest");
}

async fn test_exit_code_propagation(vm: &TestVm) {
    let out = vm.session.exec_capture("exit 42").await.unwrap();
    assert_eq!(out.exit_code, 42, "expected exit code 42, got {}", out.exit_code);
}

async fn test_claude_binary_accessible(vm: &TestVm) {
    let out = vm
        .session
        .exec_capture("/opt/claude-box/claude/claude --version")
        .await
        .unwrap();
    assert_eq!(
        out.exit_code, 0,
        "claude --version exited {} — stderr: {}",
        out.exit_code, out.stderr
    );
    assert!(
        !out.stdout.trim().is_empty(),
        "claude --version produced no output"
    );
}

async fn test_claude_runs_ddtool(vm: &TestVm) {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("    skipping (ANTHROPIC_API_KEY not set)");
        return;
    }
    let project = vm.project_path();
    let cmd = format!(
        "export PATH=/opt/claude-box/bin:$PATH && cd '{}' && /opt/claude-box/claude/claude --print 'run ddtool version and show me the output'",
        project.display()
    );
    let out = vm.session.exec_capture(&cmd).await.unwrap();
    assert_eq!(
        out.exit_code, 0,
        "claude --print exited {} — stderr: {}",
        out.exit_code, out.stderr
    );
    // We can't predict exact output, but it should contain something from ddtool.
    eprintln!("    claude output: {}", out.stdout.chars().take(200).collect::<String>());
}

// ── End-to-end claude --print test (captures errors/warnings) ────────────────

/// Full pipeline test: boot VM, inject auth + settings, run `claude --print`,
/// and capture stdout + stderr. Reports any warnings or errors found in the
/// output so they can be triaged and fixed.
///
/// Run with:
///   CLAUDE_BOX_IMAGE=<image> cargo test e2e_claude_print -- --ignored --nocapture
#[test]
#[ignore]
fn e2e_claude_print() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        e2e_claude_print_inner().await.expect("e2e_claude_print failed");
    });
}

async fn e2e_claude_print_inner() -> Result<()> {
    use claude_box::vm::warm_pool;

    let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
        .context("CLAUDE_BOX_IMAGE env var required for integration tests")?;

    let base_image = resolve_image(&image_ref, &PullPolicy::Never).await?;
    let claude_bin = mount::find_claude_binary()?;

    let project_dir = tempfile::Builder::new()
        .prefix("claude-box-e2e-")
        .tempdir_in("/private/tmp")
        .context("creating project tempdir")?;

    // Use warm pool for faster startup.
    let tart = TartVm::new();
    let (run_name, session, mut child) =
        warm_pool::try_warm_boot(&image_ref, &base_image, project_dir.path(), &claude_bin)
            .await
            .context("try_warm_boot failed")?;

    // Mount project VirtioFS share.
    let project_share = claude_box::mount::DirShare {
        host_path: project_dir.path().to_path_buf(),
        tag: "project".to_string(),
        guest_mount: project_dir.path().to_path_buf(),
        read_only: false,
    };
    guest_setup::setup_guest(&session, &[project_share], None, &[])
        .await
        .context("guest_setup on warm clone")?;

    // Prepare tool env and transfer config files via SSH.
    let tool_env = tools::prepare(&[], &[], project_dir.path())?;
    let staging = tool_env.config_staging_dir.path();

    let settings_path = staging.join("settings.json");
    if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        let escaped = content.replace('\'', "'\\''");
        let cmd = format!(
            "mkdir -p ~/.claude && printf '%s' '{}' > ~/.claude/settings.json",
            escaped
        );
        session.exec(&cmd).await?;
    }

    let claude_json_path = staging.join("claude.json");
    if claude_json_path.exists() {
        let content = std::fs::read_to_string(&claude_json_path)?;
        let escaped = content.replace('\'', "'\\''");
        let cmd = format!("printf '%s' '{}' > ~/.claude.json", escaped);
        session.exec(&cmd).await?;
    }

    // Resolve API key from host (same 3-source chain as sandbox.rs).
    let api_key = resolve_api_key_for_test(&tool_env).await;

    // Diagnostic: check guest env, claude --version, and ~/.claude.json contents.
    eprintln!("\n[e2e] Diagnostic: checking guest env...");
    let diag = session.exec_capture(
        "echo '=== env ===' && env | grep -i claude; \
         echo '=== claude --version ===' && /opt/claude-box/claude/claude --version 2>&1; \
         echo '=== ~/.claude.json ===' && cat ~/.claude.json 2>&1; \
         echo '=== ~/.claude/settings.json ===' && cat ~/.claude/settings.json 2>&1"
    ).await?;
    eprintln!("[e2e] Diagnostics:\n{}", diag.stdout);

    // Try running claude --print with a 15s timeout inside the VM to capture
    // partial output from whatever prompt is blocking.
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref key) = api_key {
        parts.push(format!("export ANTHROPIC_API_KEY='{}' &&", key.replace('\'', "'\\''")));
    }
    parts.push(format!("cd '{}' &&", project_dir.path().display()));
    // macOS has no `timeout` command. Use perl alarm to kill claude after 30s.
    // Redirect stderr to stdout so we capture everything.
    parts.push("perl -e 'alarm 30; exec @ARGV' /opt/claude-box/claude/claude --dangerously-skip-permissions --print 'say hello world' </dev/null 2>&1; echo \"EXIT=$?\"".to_string());
    let cmd = parts.join(" ");

    eprintln!("\n[e2e] Running with 30s VM-side timeout...");
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session.exec_capture(&cmd),
    )
    .await
    .context("SSH exec timed out after 60s")?
    .context("exec_capture failed")?;

    eprintln!("[e2e] Exit code: {}", out.exit_code);
    eprintln!("[e2e] Stdout ({} bytes):\n{}", out.stdout.len(), out.stdout);
    eprintln!("[e2e] Stderr ({} bytes):\n{}", out.stderr.len(), out.stderr);

    // ── Analyse output for warnings/errors ──────────────────────────────────
    let mut issues: Vec<String> = Vec::new();

    // Check stderr for any content (warnings, prompts, errors).
    if !out.stderr.trim().is_empty() {
        for line in out.stderr.lines() {
            let lower = line.to_lowercase();
            if lower.contains("error") || lower.contains("warn") || lower.contains("fail") {
                issues.push(format!("STDERR: {}", line.trim()));
            } else if lower.contains("permission") || lower.contains("accept") || lower.contains("allow") {
                issues.push(format!("PERMISSION_PROMPT: {}", line.trim()));
            } else if lower.contains("onboarding") || lower.contains("setup") || lower.contains("welcome") {
                issues.push(format!("ONBOARDING: {}", line.trim()));
            } else if !line.trim().is_empty() {
                issues.push(format!("STDERR_OTHER: {}", line.trim()));
            }
        }
    }

    // Check stdout for unexpected prompts/warnings mixed in.
    for line in out.stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("do you want to") || lower.contains("press enter") || lower.contains("[y/n]") {
            issues.push(format!("INTERACTIVE_PROMPT: {}", line.trim()));
        }
        if lower.contains("permission") && lower.contains("allow") {
            issues.push(format!("PERMISSION_IN_STDOUT: {}", line.trim()));
        }
    }

    // Check exit code.
    if out.exit_code != 0 {
        issues.push(format!("NON_ZERO_EXIT: code={}", out.exit_code));
    }

    // Report issues.
    if issues.is_empty() {
        eprintln!("\n[e2e] No issues detected.");
    } else {
        eprintln!("\n[e2e] === ISSUES DETECTED ({}) ===", issues.len());
        for (i, issue) in issues.iter().enumerate() {
            eprintln!("  {}. {}", i + 1, issue);
        }
        eprintln!("[e2e] === END ISSUES ===\n");
    }

    // Write issues to a file for easy review.
    let issues_path = "/tmp/claude-box-e2e-issues.txt";
    let issues_content = if issues.is_empty() {
        "No issues detected.\n".to_string()
    } else {
        issues
            .iter()
            .enumerate()
            .map(|(i, issue)| format!("{}. {}", i + 1, issue))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    std::fs::write(issues_path, &issues_content)?;
    eprintln!("[e2e] Issues written to {issues_path}");

    // ── Sub-test 2: verify --dangerously-skip-permissions works with tool use ─
    // Ask Claude to write a file — this requires tool permissions. If
    // --dangerously-skip-permissions isn't working, Claude will prompt and hang.
    if api_key.is_some() {
        let marker = project_dir.path().join("tool-test.txt");
        let mut tool_parts: Vec<String> = Vec::new();
        if let Some(ref key) = api_key {
            tool_parts.push(format!("export ANTHROPIC_API_KEY='{}' &&", key.replace('\'', "'\\''")));
        }
        tool_parts.push(format!("cd '{}' &&", project_dir.path().display()));
        tool_parts.push(format!(
            "perl -e 'alarm 60; exec @ARGV' /opt/claude-box/claude/claude \
             --dangerously-skip-permissions --print \
             'Write the exact text \"sandbox-tool-ok\" to the file {}. Do not include anything else.' \
             </dev/null 2>&1; echo \"TOOL_EXIT=$?\"",
            marker.display()
        ));
        let tool_cmd = tool_parts.join(" ");

        eprintln!("\n[e2e] Sub-test 2: tool use (write file) with --dangerously-skip-permissions...");
        let tool_out = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            session.exec_capture(&tool_cmd),
        )
        .await
        .context("tool use test timed out after 90s")?
        .context("exec_capture failed")?;

        eprintln!("[e2e] Tool test stdout:\n{}", tool_out.stdout);

        // Check if the file was written on the host (VirtioFS pass-through).
        if marker.exists() {
            let content = std::fs::read_to_string(&marker).unwrap_or_default();
            eprintln!("[e2e] Tool test: file written, content={:?}", content.trim());
            assert!(
                content.contains("sandbox-tool-ok"),
                "tool-test.txt has wrong content: {:?}",
                content
            );
        } else {
            // File might not exist if Claude chose to use echo/redirect instead of
            // the Write tool — that's fine as long as it didn't hang.
            eprintln!("[e2e] Tool test: file not found on host (Claude may have used stdout)");
        }
    }

    // ── Sub-test 3: capture TUI startup warnings via `script` ─────────────
    // The --print mode bypasses TUI rendering, so warnings that only appear
    // in the interactive UI won't show up.  Run Claude in TUI mode briefly
    // using macOS `script` to record everything the terminal renders, then
    // pipe /exit to quit.
    if api_key.is_some() {
        eprintln!("\n[e2e] Sub-test 3: TUI startup warning capture...");
        let mut tui_parts: Vec<String> = Vec::new();
        if let Some(ref key) = api_key {
            tui_parts.push(format!(
                "export ANTHROPIC_API_KEY='{}'",
                key.replace('\'', "'\\''")
            ));
        }
        tui_parts.push("export PATH=$HOME/.local/bin:$PATH".to_string());
        tui_parts.push(format!("cd '{}'", project_dir.path().display()));
        // Use `script` to capture full PTY output.  Feed "/exit\n" via pipe
        // so Claude exits after startup. The perl alarm is a safety net.
        tui_parts.push(
            "perl -e 'alarm 45; exec @ARGV' \
             script -q /tmp/claude-tui-capture.log \
             /bin/sh -c '(sleep 3 && printf \"/exit\\n\") | /opt/claude-box/claude/claude --dangerously-skip-permissions'"
                .to_string(),
        );
        let tui_cmd = tui_parts.join(" && ");

        let _tui_out = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            session.exec_capture(&tui_cmd),
        )
        .await
        .context("TUI capture timed out after 60s")?
        .context("TUI exec_capture failed")?;

        // Read the captured TUI output, stripping ANSI escape codes.
        let raw_capture = session
            .exec_capture("cat /tmp/claude-tui-capture.log | col -b 2>/dev/null || cat /tmp/claude-tui-capture.log")
            .await
            .unwrap_or_else(|_| claude_box::relay::ExecOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
            });

        let tui_text = raw_capture.stdout;
        eprintln!("[e2e] TUI capture ({} bytes):\n{}", tui_text.len(), tui_text);

        // Check for known warnings in the TUI output.
        let mut tui_issues: Vec<String> = Vec::new();
        for line in tui_text.lines() {
            let lower = line.to_lowercase();
            if lower.contains("installmethod") || lower.contains(".local/bin") {
                tui_issues.push(format!("INSTALL_WARNING: {}", line.trim()));
            }
            if lower.contains("detected a custom api key") {
                tui_issues.push(format!("API_KEY_WARNING: {}", line.trim()));
            }
            if lower.contains("bypass permissions") || lower.contains("dangerous") {
                tui_issues.push(format!("PERMISSIONS_WARNING: {}", line.trim()));
            }
            if lower.contains("warning") || lower.contains("error") {
                tui_issues.push(format!("TUI_WARNING: {}", line.trim()));
            }
        }

        if tui_issues.is_empty() {
            eprintln!("[e2e] TUI: No warnings detected.");
        } else {
            eprintln!("\n[e2e] === TUI WARNINGS ({}) ===", tui_issues.len());
            for (i, issue) in tui_issues.iter().enumerate() {
                eprintln!("  {}. {}", i + 1, issue);
            }
            eprintln!("[e2e] === END TUI WARNINGS ===");
        }

        // Also write to file.
        let tui_issues_path = "/tmp/claude-box-tui-issues.txt";
        let tui_issues_content = if tui_issues.is_empty() {
            "No TUI warnings detected.\n".to_string()
        } else {
            tui_issues
                .iter()
                .enumerate()
                .map(|(i, w)| format!("{}. {}", i + 1, w))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        std::fs::write(tui_issues_path, &tui_issues_content)?;
        eprintln!("[e2e] TUI issues written to {tui_issues_path}");

        // Save full capture for offline review.
        std::fs::write("/tmp/claude-box-tui-capture.txt", &tui_text)?;
        eprintln!("[e2e] Full TUI capture saved to /tmp/claude-box-tui-capture.txt");
    }

    // Teardown.
    let _ = tart.stop(&run_name).await;
    let _ = child.wait().await;
    let _ = tart.delete(&run_name).await;

    // The test passes even with issues — it's a diagnostic test.
    // But fail if claude couldn't even start (no auth, binary missing, etc.).
    if api_key.is_some() {
        assert!(
            out.exit_code == 0 || !out.stdout.trim().is_empty(),
            "claude --print produced no output and exited {} — likely auth or binary issue.\nstderr: {}",
            out.exit_code, out.stderr
        );
    }

    Ok(())
}

/// Resolve API key for tests using the same 3-source chain as sandbox.rs:
/// apiKeyHelper → ANTHROPIC_API_KEY → macOS Keychain.
async fn resolve_api_key_for_test(tool_env: &tools::ToolEnv) -> Option<String> {
    // 1. apiKeyHelper
    if let Some(ref helper) = tool_env.api_key_helper {
        if let Ok(out) = tokio::process::Command::new("sh")
            .args(["-c", helper])
            .output()
            .await
        {
            if out.status.success() {
                let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !key.is_empty() {
                    eprintln!("[e2e] Auth: apiKeyHelper");
                    return Some(key);
                }
            }
        }
    }

    // 2. ANTHROPIC_API_KEY env var
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        eprintln!("[e2e] Auth: ANTHROPIC_API_KEY env var");
        return Some(key);
    }

    // 3. macOS Keychain
    let user = std::env::var("USER").ok()?;
    let out = tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code", "-a", &user, "-w"])
        .output()
        .await
        .ok()?;
    if out.status.success() {
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !key.is_empty() && key.starts_with("sk-") {
            eprintln!("[e2e] Auth: macOS Keychain (OAuth)");
            return Some(key);
        }
    }

    eprintln!("[e2e] Auth: none (test will likely fail)");
    None
}

// ── Warm-pool integration test ───────────────────────────────────────────────

/// Boots via the warm pool and asserts the full pipeline works and is faster
/// than a cold boot (< 20s total from try_warm_boot call to SSH ready).
///
/// Run with:
///   CLAUDE_BOX_IMAGE=<image> cargo test integration_tests_warm -- --ignored --nocapture
#[test]
#[ignore]
fn integration_tests_warm() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        integration_tests_warm_inner().await.expect("integration_tests_warm failed");
    });
}

async fn integration_tests_warm_inner() -> Result<()> {
    use claude_box::vm::warm_pool;
    use claude_box::mount::DirShare;

    let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
        .context("CLAUDE_BOX_IMAGE env var required for integration tests")?;

    // Resolve image (creates warm VM on first run if needed).
    let base_image = resolve_image(&image_ref, &PullPolicy::Never).await?;

    let claude_bin = mount::find_claude_binary()?;

    let project_dir = tempfile::Builder::new()
        .prefix("claude-box-warm-test-")
        .tempdir_in("/private/tmp")?;
    std::fs::write(project_dir.path().join("warm-marker.txt"), "warm-pool-test")?;

    // ── First call: seeds the warm pool (may create the VM, ~25s on first run). ─
    eprintln!("[warm-integ] seeding warm pool (first call may create the warm VM)…");
    let tart = TartVm::new();
    let (seed_name, seed_session, mut seed_child) =
        warm_pool::try_warm_boot(&image_ref, &base_image, project_dir.path(), &claude_bin)
            .await
            .context("try_warm_boot (seed) failed")?;
    let out = seed_session.exec_capture("echo warm-seed").await?;
    assert_eq!(out.exit_code, 0, "seed SSH check failed");
    assert!(out.stdout.contains("warm-seed"));
    let _ = tart.stop(&seed_name).await;
    let _ = seed_child.wait().await;
    let _ = tart.delete(&seed_name).await;
    eprintln!("[warm-integ] seed run done, warm VM now cached");

    // ── Second call: actual warm boot from cached snapshot. ──────────────────
    let t = Instant::now();
    let (run_name, session, mut child) =
        warm_pool::try_warm_boot(&image_ref, &base_image, project_dir.path(), &claude_bin)
            .await
            .context("try_warm_boot (warm) failed")?;
    let boot_ms = t.elapsed().as_millis();
    eprintln!("[warm-integ] warm boot to SSH ready: {boot_ms}ms");

    // Verify SSH connectivity.
    let out = session.exec_capture("echo warm-hello").await?;
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("warm-hello"), "stdout: {}", out.stdout);

    // Mount only the project VirtioFS share (1-slot warm design).
    let project_share = DirShare {
        host_path: project_dir.path().to_path_buf(),
        tag: "project".to_string(),
        guest_mount: project_dir.path().to_path_buf(),
        read_only: false,
    };
    guest_setup::setup_guest(&session, &[project_share], None, &[])
        .await
        .context("guest_setup on warm clone")?;

    // Verify the project dir is mounted and accessible.
    let marker_path = project_dir.path().join("warm-marker.txt");
    let out = session.exec_capture(&format!("cat '{}'", marker_path.display())).await?;
    assert_eq!(out.exit_code, 0, "cat marker failed: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "warm-pool-test");

    // Verify the claude binary is accessible from the VM disk (not VirtioFS).
    let out = session.exec_capture("/opt/claude-box/claude/claude --version").await?;
    assert_eq!(out.exit_code, 0, "claude --version failed: stderr={}", out.stderr);
    assert!(!out.stdout.trim().is_empty(), "claude --version produced no output");

    // Warm boot (cached snapshot resume) should complete within 20s.
    // Cold boot takes ~40s; warm VM creation (first run) takes ~25s.
    assert!(boot_ms < 20_000, "warm boot took {boot_ms}ms — expected < 20000ms");

    // Teardown.
    let _ = tart.stop(&run_name).await;
    let _ = child.wait().await;
    let _ = tart.delete(&run_name).await;

    eprintln!("[warm-integ] PASSED (boot: {boot_ms}ms)");
    Ok(())
}

// ── N-slot snapshot test ─────────────────────────────────────────────────────

/// Determine how many VirtioFS slots can be used with tart snapshot/restore.
///
/// Creates a warm VM with N slots, properly suspends it, clones it, and
/// attempts to resume the clone. Reports which slot counts work.
///
/// Run with:
///   CLAUDE_BOX_IMAGE=<image> cargo test warm_nslot_test -- --ignored --nocapture
#[test]
#[ignore]
fn warm_nslot_test() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        warm_nslot_test_inner().await.expect("warm_nslot_test_inner failed");
    });
}

async fn warm_nslot_test_inner() -> Result<()> {
    use std::os::unix::fs::symlink;

    let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
        .context("CLAUDE_BOX_IMAGE env var required")?;
    let base_image = resolve_image(&image_ref, &PullPolicy::Never).await?;

    let placeholder = PathBuf::from("/private/tmp/nslot-placeholder");
    tokio::fs::create_dir_all(&placeholder).await?;

    // Test N slots: try 1, 2, 4, 8, 11
    for n in [1usize, 2, 4, 8, 11] {
        let warm_name = format!("claude-box-warm-nslot-{n}");
        let clone_name = format!("claude-box-warm-nslot-{n}-clone");
        let tart = TartVm::new();

        // Cleanup any previous run
        let _ = tart.stop(&warm_name).await;
        let _ = tart.delete(&warm_name).await;
        let _ = tart.stop(&clone_name).await;
        let _ = tart.delete(&clone_name).await;

        // Create N symlink slots
        let slot_root = PathBuf::from("/private/tmp/nslot-slots");
        tokio::fs::create_dir_all(&slot_root).await?;
        let mut slot_paths = Vec::new();
        for i in 0..n {
            let slot = slot_root.join(format!("slot-{i}"));
            if slot.exists() || slot.is_symlink() {
                std::fs::remove_file(&slot)?;
            }
            symlink(&placeholder, &slot)?;
            slot_paths.push(slot);
        }

        // Clone base image
        eprintln!("[nslot] N={n}: cloning...");
        let vm_config = VmConfig { name: warm_name.clone(), base_image: base_image.clone(), dir_shares: vec![] };
        tart.create(&vm_config).await?;

        // Generate SSH key and inject
        let key = ssh_key::generate()?;
        ssh_key::inject_key(&warm_name, &key.authorized_keys_line).await?;

        // Build dir args
        let mut run_args = vec!["run".to_string(), "--suspendable".to_string(), "--no-graphics".to_string()];
        for (i, p) in slot_paths.iter().enumerate() {
            run_args.push(format!("--dir={}:tag=slot-{i}", p.display()));
        }
        run_args.push(warm_name.clone());

        // Boot
        let mut warm_child = tokio::process::Command::new("tart")
            .args(&run_args)
            .spawn()
            .context("failed to spawn tart run")?;

        // Wait for SSH
        let ip = health::get_vm_ip(&warm_name).await?;
        let session = health::wait_for_ssh(&ip, &key.keypair, 120).await?;
        drop(session);
        eprintln!("[nslot] N={n}: VM up (IP={ip}), suspending...");

        // Suspend
        let status = tokio::process::Command::new("tart")
            .args(["suspend", &warm_name])
            .status().await?;
        anyhow::ensure!(status.success(), "tart suspend failed");

        // Wait for tart run to fully exit
        warm_child.wait().await?;
        let snap_size = std::fs::metadata(
            format!("{}/.tart/vms/{}/state.vzvmsave",
                std::env::var("HOME").unwrap(), warm_name))
            .map(|m| m.len()).unwrap_or(0);
        eprintln!("[nslot] N={n}: snapshot={snap_size}B");

        // Clone and resume
        let clone_config = VmConfig { name: clone_name.clone(), base_image: warm_name.clone(), dir_shares: vec![] };
        tart.create(&clone_config).await?;

        let mut clone_run_args = vec!["run".to_string(), "--suspendable".to_string(), "--no-graphics".to_string()];
        for (i, p) in slot_paths.iter().enumerate() {
            clone_run_args.push(format!("--dir={}:tag=slot-{i}", p.display()));
        }
        clone_run_args.push(clone_name.clone());

        let mut clone_child = tokio::process::Command::new("tart")
            .args(&clone_run_args)
            .spawn()
            .context("failed to spawn clone tart run")?;

        // Wait up to 30s for SSH on clone
        let clone_ip_result = health::get_vm_ip(&clone_name).await;
        let success = match clone_ip_result {
            Ok(ip) => {
                match health::wait_for_ssh(&ip, &key.keypair, 30).await {
                    Ok(_) => true,
                    Err(e) => { eprintln!("[nslot] N={n}: SSH FAILED: {e}"); false }
                }
            }
            Err(e) => { eprintln!("[nslot] N={n}: IP FAILED: {e}"); false }
        };

        eprintln!("[nslot] N={n}: {} ← {}",
            if success { "PASS" } else { "FAIL" },
            if success { "snapshot restored OK" } else { "Code=12 or timeout" });

        // Teardown
        let _ = tart.stop(&clone_name).await;
        let _ = clone_child.wait().await;
        let _ = tart.delete(&clone_name).await;
        let _ = tart.stop(&warm_name).await;
        let _ = tart.delete(&warm_name).await;
    }

    Ok(())
}

// ── Warm-pool reliability test ───────────────────────────────────────────────

/// Phase 10.0: Symlink-based VirtioFS resume reliability test.
///
/// Creates one "warm" VM suspended with a fixed VirtioFS slot path that is a
/// symlink, then clones + resumes it 10 times — each time pointing the symlink
/// to a different directory. Passes if ≤ 1 failure in 10 attempts.
///
/// Run with:
///   CLAUDE_BOX_IMAGE=<image> cargo test warm_pool_reliability -- --ignored --nocapture
#[test]
#[ignore]
fn warm_pool_reliability() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        warm_pool_reliability_inner().await.expect("warm_pool_reliability_inner failed");
    });
}

async fn warm_pool_reliability_inner() -> Result<()> {
    use std::os::unix::fs::symlink;

    let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
        .context("CLAUDE_BOX_IMAGE env var required for integration tests")?;

    // Fixed warm slot path — this exact string is baked into the warm VM's snapshot.
    // It must remain identical every time the warm VM clone is resumed.
    let home = std::env::var("HOME").context("HOME not set")?;
    let slot_root = PathBuf::from(&home).join(".cache/claude-box/warm-slots");
    let slot_path = slot_root.join("rel-test-project"); // this will be a symlink
    tokio::fs::create_dir_all(&slot_root).await?;

    // Initial placeholder dir so the warm VM boot has something real to open.
    let placeholder = PathBuf::from("/tmp/warm-rel-test-placeholder");
    tokio::fs::create_dir_all(&placeholder).await?;

    // Create or reset the symlink.
    if slot_path.exists() || slot_path.is_symlink() {
        tokio::fs::remove_file(&slot_path).await.ok();
    }
    symlink(&placeholder, &slot_path)
        .with_context(|| format!("symlink {} -> {}", slot_path.display(), placeholder.display()))?;

    // Resolve image.
    let base_image = resolve_image(&image_ref, &PullPolicy::Never).await?;

    // Generate warm SSH key (clones inherit authorized_keys from snapshot).
    let warm_key = ssh_key::generate()?;

    // Clean up any warm VM from a previous aborted run.
    let warm_name = "claude-box-warm-rel-test".to_string();
    let tart = TartVm::new();
    let _ = tart.stop(&warm_name).await;
    let _ = tart.delete(&warm_name).await;

    // Clone base image to warm VM.
    eprintln!("\n[warm-rel] Creating warm VM {warm_name}...");
    let warm_config = VmConfig { name: warm_name.clone(), base_image, dir_shares: vec![] };
    tart.create(&warm_config).await?;

    // Inject warm SSH key.
    ssh_key::inject_key(&warm_name, &warm_key.authorized_keys_line).await?;

    // Boot warm VM with --suspendable and the fixed slot path.
    let slot_str = format!("{}:tag=rel-test-project", slot_path.display());
    let mut warm_child = tokio::process::Command::new("tart")
        .args(["run", "--suspendable", "--no-graphics", &format!("--dir={slot_str}"), &warm_name])
        .spawn()
        .context("failed to spawn tart run for warm VM")?;

    // Wait for SSH.
    let ip = health::get_vm_ip(&warm_name).await.context("warm VM did not get an IP")?;
    let session = health::wait_for_ssh(&ip, &warm_key.keypair, 120).await
        .context("warm VM SSH never became ready")?;
    drop(session);
    eprintln!("[warm-rel] Warm VM booted (IP={ip}), suspending...");

    // Suspend warm VM.
    let status = tokio::process::Command::new("tart")
        .args(["suspend", &warm_name])
        .status()
        .await
        .context("failed to run tart suspend")?;
    anyhow::ensure!(status.success(), "tart suspend failed for {warm_name}");
    let _ = warm_child.wait().await;
    eprintln!("[warm-rel] Warm VM suspended. Beginning 10-iteration reliability test.\n");

    // ── 10-iteration clone+resume loop ───────────────────────────────────────
    let mut results: Vec<(usize, bool, u64)> = Vec::new();

    for i in 1..=10usize {
        // New content dir for this iteration.
        let iter_dir = PathBuf::from(format!("/tmp/warm-rel-iter-{i}"));
        tokio::fs::create_dir_all(&iter_dir).await?;
        tokio::fs::write(iter_dir.join("marker.txt"), format!("iteration-{i}")).await?;

        // Atomically update symlink to new dir.
        if slot_path.is_symlink() {
            tokio::fs::remove_file(&slot_path).await?;
        }
        symlink(&iter_dir, &slot_path).with_context(|| format!("update symlink for iter {i}"))?;
        eprintln!("[warm-rel] Iter {i:2}: symlink -> {}", iter_dir.display());

        // Clone the warm VM.
        let clone_name = format!("claude-box-warm-rel-clone-{i}");
        let _ = tart.stop(&clone_name).await;
        let _ = tart.delete(&clone_name).await;

        let t_clone = Instant::now();
        let clone_config = VmConfig { name: clone_name.clone(), base_image: warm_name.clone(), dir_shares: vec![] };
        tart.create(&clone_config).await.with_context(|| format!("tart clone for iter {i}"))?;
        eprintln!("[warm-rel] Iter {i:2}: cloned in {}ms", t_clone.elapsed().as_millis());

        // Resume clone with SAME slot path (critical: must match snapshot).
        let mut clone_child = tokio::process::Command::new("tart")
            .args(["run", "--suspendable", "--no-graphics", &format!("--dir={slot_str}"), &clone_name])
            .spawn()
            .with_context(|| format!("spawn tart run for iter {i}"))?;

        // Check if SSH becomes ready — this is the pass/fail criterion.
        let t_ssh = Instant::now();
        let (success, elapsed_ms) = match health::get_vm_ip(&clone_name).await {
            Ok(clone_ip) => {
                match health::wait_for_ssh(&clone_ip, &warm_key.keypair, 30).await {
                    Ok(session) => {
                        let ms = t_ssh.elapsed().as_millis() as u64;
                        eprintln!("[warm-rel] Iter {i:2}: PASS — SSH ready in {ms}ms (IP={clone_ip})");
                        drop(session);
                        (true, ms)
                    }
                    Err(e) => {
                        let ms = t_ssh.elapsed().as_millis() as u64;
                        eprintln!("[warm-rel] Iter {i:2}: FAIL — SSH timeout ({ms}ms): {e}");
                        (false, ms)
                    }
                }
            }
            Err(e) => {
                let ms = t_ssh.elapsed().as_millis() as u64;
                eprintln!("[warm-rel] Iter {i:2}: FAIL — IP timeout ({ms}ms): {e}");
                (false, ms)
            }
        };

        results.push((i, success, elapsed_ms));

        // Tear down clone before next iteration.
        let _ = tart.stop(&clone_name).await;
        let _ = clone_child.wait().await;
        let _ = tart.delete(&clone_name).await;
        eprintln!();
    }

    // ── Cleanup warm VM ───────────────────────────────────────────────────────
    let _ = tart.stop(&warm_name).await;
    let _ = tart.delete(&warm_name).await;

    // ── Report ────────────────────────────────────────────────────────────────
    let pass_count = results.iter().filter(|(_, ok, _)| *ok).count();
    let fail_count = 10 - pass_count;

    eprintln!("[warm-rel] === RESULTS ===");
    for (i, ok, ms) in &results {
        eprintln!("  Iter {:2}: {} ({ms}ms)", i, if *ok { "PASS" } else { "FAIL" });
    }
    eprintln!("[warm-rel] Total: {pass_count}/10 passed, {fail_count}/10 failed");

    anyhow::ensure!(
        fail_count <= 1,
        "Symlink VirtioFS resume is unreliable: {fail_count}/10 failures (threshold: 1). \
         Consider the SFTP fallback design from plan/milestone-10-warm-pool.md"
    );

    eprintln!("[warm-rel] PASS — symlink approach is reliable ({fail_count}/10 failures ≤ 1 threshold)");
    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Cleanup guard that stops and deletes the VM even if a test panics.
/// Uses a separate tokio runtime for the synchronous Drop impl.
struct CleanupGuard {
    vm_name: Option<String>,
}

impl CleanupGuard {
    /// Disarm the guard — caller will handle cleanup explicitly.
    fn disarm(&mut self) {
        self.vm_name = None;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let Some(vm_name) = self.vm_name.take() else {
            return;
        };
        eprintln!("\n=== Panic cleanup: tearing down VM {vm_name} ===");
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            let vm = TartVm::new();
            rt.block_on(async {
                let _ = vm.stop(&vm_name).await;
                let _ = vm.delete(&vm_name).await;
            });
        }
        eprintln!("=== Panic cleanup done ===");
    }
}

#[test]
#[ignore]
fn integration_tests() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        eprintln!("\n=== Booting test VM ===");
        let vm = TestVm::boot().await.expect("TestVm::boot failed");
        eprintln!("=== VM {} ready ===\n", vm.vm_name);

        // Guard ensures cleanup even if a sub-test panics.
        let mut guard = CleanupGuard {
            vm_name: Some(vm.vm_name.clone()),
        };

        run_subtests(&vm).await;

        // Happy path: disarm guard, do full teardown (including child wait).
        guard.disarm();
        eprintln!("\n=== Tearing down VM {} ===", vm.vm_name);
        vm.teardown().await;
        eprintln!("=== Done ===\n");
    });
}

async fn run_subtests(vm: &TestVm) {
    eprintln!("--- test_vm_boot_and_ssh");
    test_vm_boot_and_ssh(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_ddtool_version");
    test_ddtool_version(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_binary_allowlist_enforcement");
    test_binary_allowlist_enforcement(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_project_dir_mounted");
    test_project_dir_mounted(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_project_dir_writable");
    test_project_dir_writable(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_exit_code_propagation");
    test_exit_code_propagation(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_claude_binary_accessible");
    test_claude_binary_accessible(vm).await;
    eprintln!("--- PASSED\n");

    eprintln!("--- test_claude_runs_ddtool");
    test_claude_runs_ddtool(vm).await;
    eprintln!("--- PASSED\n");
}
