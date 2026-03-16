use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

mod config;
mod gc;
mod images;
mod mount;
mod relay;
mod sandbox;
mod tools;
mod vm;

use config::SandboxConfig;
use sandbox::Sandbox;
use vm::image::PullPolicy;
use vm::warm_pool;

/// Runs Claude Code inside an ephemeral macOS VM sandbox.
///
/// All unrecognised flags and positional arguments are forwarded verbatim to
/// `claude` running inside the VM.
#[derive(Parser, Debug)]
#[command(name = "claude-box", disable_help_flag = true)]
struct Cli {
    /// OCI image ref or path to a local .ipsw file.
    /// Env: CLAUDE_BOX_IMAGE
    #[arg(long, env = "CLAUDE_BOX_IMAGE")]
    vm_image: Option<String>,

    /// Host directory to mount into the VM at the same absolute path.
    /// Defaults to the current working directory.
    #[arg(long)]
    mount: Option<PathBuf>,

    /// MCP server name to enable inside the VM (repeatable).
    /// When not specified all servers from ~/.claude/settings.json are passed through.
    #[arg(long = "allow-tool", value_name = "NAME")]
    allow_tools: Vec<String>,

    /// Host binary name to forward into the VM (repeatable).
    /// The binary is located via PATH, copied to a staging dir and mounted at
    /// /opt/claude-box/bin which is prepended to the guest PATH.
    #[arg(long = "allow-binary", value_name = "NAME")]
    allow_binaries: Vec<String>,

    /// Keep the VM after the run instead of deleting it.
    #[arg(long)]
    persist: bool,

    /// Explicit VM name; defaults to claude-box-<uuid>.
    #[arg(long)]
    vm_name: Option<String>,

    /// Pass --help through to the inner `claude` command.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    help: bool,

    /// When to pull the base image from the registry.
    #[arg(long, value_enum, default_value_t = PullPolicy::Missing)]
    pull: PullPolicy,

    /// Smoke-boot the base image before cloning (adds ~30s).
    #[arg(long)]
    validate_image: bool,

    /// Disable fallback to host claude on sandbox failure.
    /// By default, if the sandbox fails claude-box will warn and run claude
    /// directly on the host. Use this flag in CI to enforce sandboxing.
    #[arg(long)]
    no_fallback: bool,

    /// Disable the warm VM pool and force a cold boot for this run.
    #[arg(long)]
    no_warm: bool,

    /// All remaining arguments are forwarded verbatim to `claude` inside the VM.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    claude_args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Dispatch subcommands before full CLI parse.
    let argv: Vec<String> = std::env::args().collect();
    match argv.get(1).map(|s| s.as_str()) {
        Some("images") => {
            let image_args: Vec<String> = argv.into_iter().skip(2).collect();
            let exit_code = images::run_images_command(&image_args).await?;
            std::process::exit(exit_code);
        }
        Some("gc") => {
            let exit_code = gc::run_gc_command().await?;
            std::process::exit(exit_code);
        }
        Some("warm") => {
            let exit_code = run_warm_command(&argv[2..]).await?;
            std::process::exit(exit_code);
        }
        _ => {}
    }

    let cli = Cli::parse();

    // Rebuild the full args list to pass to claude inside the VM.
    let mut inner_args = cli.claude_args;
    if cli.help {
        inner_args.insert(0, "--help".to_string());
    }

    let mount = match cli.mount {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    let no_fallback = cli.no_fallback;
    // Clone args for potential fallback before moving into config.
    let fallback_args = inner_args.clone();

    // Check for tart before attempting the sandbox.
    if which::which("tart").is_err() && !ensure_tart_installed().await {
        if no_fallback {
            anyhow::bail!("tart is not installed and --no-fallback is set");
        }
        eprintln!("claude-box: tart not available, falling back to host claude");
        return fallback_to_host_claude(&fallback_args);
    }

    let config = SandboxConfig {
        vm_image: cli.vm_image,
        mount,
        allow_tools: cli.allow_tools,
        allow_binaries: cli.allow_binaries,
        persist: cli.persist,
        vm_name: cli.vm_name,
        claude_args: inner_args,
        pull_policy: cli.pull,
        validate_image: cli.validate_image,
        warm_pool: !cli.no_warm,
    };

    let sandbox = Sandbox::new(config);
    match sandbox.run().await {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            // Check if the user explicitly chose to skip the image pull.
            let user_requested =
                e.downcast_ref::<vm::image::UserRequestedFallback>().is_some();

            if no_fallback && !user_requested {
                return Err(e);
            }
            if !user_requested {
                eprintln!("claude-box: sandbox failed: {e:#}");
            }
            eprintln!("claude-box: falling back to host claude");
            fallback_to_host_claude(&fallback_args)
        }
    }
}

/// Find the host `claude` binary and exec into it, replacing this process.
fn fallback_to_host_claude(args: &[String]) -> Result<()> {
    let claude_bin = mount::find_claude_binary()
        .context("fallback failed: could not find claude on PATH")?;

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(claude_bin)
        .args(args)
        .exec();
    // exec() only returns on error.
    Err(err.into())
}

/// Handle `claude-box warm <subcommand>` commands.
async fn run_warm_command(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("list") => {
            let vms = warm_pool::list_warm_vms().await?;
            if vms.is_empty() {
                println!("No warm VMs found.");
            } else {
                println!("{:<50} {:<30} AGE", "IMAGE", "VM NAME");
                for (image_ref, tart_name, age_secs) in &vms {
                    let age = if *age_secs < 3600 {
                        format!("{}m", age_secs / 60)
                    } else if *age_secs < 86400 {
                        format!("{}h", age_secs / 3600)
                    } else {
                        format!("{}d", age_secs / 86400)
                    };
                    println!("{:<50} {:<30} {}", image_ref, tart_name, age);
                }
            }
            Ok(0)
        }
        Some("refresh") => {
            let image_ref = std::env::var("CLAUDE_BOX_IMAGE")
                .context("CLAUDE_BOX_IMAGE required for `claude-box warm refresh`")?;
            use vm::image::{resolve_image, PullPolicy};
            let base_image = resolve_image(&image_ref, &PullPolicy::Missing).await?;
            let claude_bin = crate::mount::find_claude_binary()?;
            eprintln!("Refreshing warm VM for {image_ref}…");
            warm_pool::refresh_warm_vm(&image_ref, &base_image, &claude_bin).await?;
            println!("Warm VM refreshed.");
            Ok(0)
        }
        Some("delete") => {
            let deleted = warm_pool::delete_all_warm_vms().await?;
            println!("Deleted {deleted} warm VM(s).");
            Ok(0)
        }
        _ => {
            eprintln!("Usage: claude-box warm <list|refresh|delete>");
            Ok(1)
        }
    }
}

/// Prompt the user to install tart via Homebrew. Returns true if tart is
/// available after this function returns (either already installed or
/// successfully installed).
async fn ensure_tart_installed() -> bool {
    use std::io::{IsTerminal, Write};

    // Only prompt if stdin is interactive.
    if !std::io::stdin().is_terminal() {
        eprintln!("claude-box: tart is not installed (non-interactive, skipping install prompt)");
        return false;
    }

    eprint!("claude-box: tart is not installed. Install via Homebrew? [Y/n] ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    let answer = input.trim().to_lowercase();
    if !answer.is_empty() && answer != "y" && answer != "yes" {
        return false;
    }

    eprintln!("claude-box: installing tart via brew...");
    let status = tokio::process::Command::new("brew")
        .args(["install", "cirruslabs/cli/tart"])
        .status()
        .await;

    match status {
        Ok(s) if s.success() => {
            eprintln!("claude-box: tart installed successfully");
            true
        }
        Ok(s) => {
            eprintln!("claude-box: brew install failed with {s}");
            false
        }
        Err(e) => {
            eprintln!("claude-box: failed to run brew: {e}");
            false
        }
    }
}
