use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::info;

/// Returned when the user chooses to skip image pull and fallback to host claude.
#[derive(Debug)]
pub struct UserRequestedFallback;

impl std::fmt::Display for UserRequestedFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user requested fallback to host claude (unsandboxed)")
    }
}

impl std::error::Error for UserRequestedFallback {}

use crate::vm::image_cache::ImageCache;

/// Pull policy controlling when `tart pull` is invoked.
#[derive(Clone, Debug, clap::ValueEnum, Default)]
pub enum PullPolicy {
    Always,
    #[default]
    Missing,
    Never,
}

/// Resolve an image reference, ensuring it is available locally as a tart image.
///
/// Resolution order:
/// 1. If `image_ref` is a path to an existing `.ipsw` file → import via
///    `tart create --from-ipsw`.
/// 2. Otherwise treat it as an OCI ref and run `tart pull` (subject to pull_policy).
///
/// Returns the local tart image name to use for cloning.
pub async fn resolve_image(image_ref: &str, pull_policy: &PullPolicy) -> Result<String> {
    let path = Path::new(image_ref);
    if path.exists() && path.extension().is_some_and(|e| e == "ipsw") {
        import_ipsw(path).await
    } else {
        pull_oci(image_ref, pull_policy).await
    }
}

/// Validate an image by doing a smoke-boot.
pub async fn validate_image(name: &str) -> Result<()> {
    info!("validating image {name} (smoke boot)");
    let status = Command::new("tart")
        .args(["run", "--no-graphics", name, "--", "true"])
        .status()
        .await
        .context("failed to spawn tart run for validation")?;
    if !status.success() {
        anyhow::bail!("image validation failed for {name}");
    }
    Ok(())
}

/// Return the set of image names currently in tart's local store.
///
/// `tart list` output: "Source  Name  Disk  Size  Accessed  State"
/// The name is the second whitespace-separated field on each data line.
async fn list_tart_images() -> Result<std::collections::HashSet<String>> {
    let output = Command::new("tart")
        .args(["list"])
        .output()
        .await
        .context("failed to spawn tart list")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect())
}

/// Check if a named image already exists in tart's local store.
pub async fn image_exists_in_tart(name: &str) -> Result<bool> {
    Ok(list_tart_images().await?.contains(name))
}

fn make_spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg} [{elapsed_precise}]")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

async fn pull_oci(image_ref: &str, pull_policy: &PullPolicy) -> Result<String> {
    match pull_policy {
        PullPolicy::Missing | PullPolicy::Never => {
            // Single tart list call — check both cached name and raw ref.
            let tart_images = list_tart_images().await?;
            let cache = ImageCache::load()?;
            if let Some(entry) = cache.get(image_ref) {
                if tart_images.contains(&entry.local_name) {
                    info!("using cached image {image_ref} → {}", entry.local_name);
                    return Ok(entry.local_name.clone());
                }
            }
            // Cache miss: check tart directly (image may have been pulled via
            // `tart pull` outside of claude-box, or the cache file was deleted).
            if tart_images.contains(image_ref) {
                info!("image {image_ref} found in tart (no cache entry)");
                return Ok(image_ref.to_string());
            }
            if matches!(pull_policy, PullPolicy::Never) {
                anyhow::bail!(
                    "image {image_ref} not found locally and --pull=never; \
                     run `claude-box images pull {image_ref}` first"
                );
            }
            do_pull_oci(image_ref).await
        }
        PullPolicy::Always => do_pull_oci(image_ref).await,
    }
}

async fn do_pull_oci(image_ref: &str) -> Result<String> {
    // The image needs to be downloaded. Prompt the user if interactive.
    if !prompt_pull_or_fallback(image_ref) {
        return Err(UserRequestedFallback.into());
    }

    info!("pulling OCI image {image_ref}");
    let pb = make_spinner(format!("Pulling {image_ref}"));

    // No timeout — macOS VM images are large and pull times vary widely.
    let status = Command::new("tart")
        .args(["pull", image_ref])
        .status()
        .await
        .context("failed to spawn tart pull")?;

    if !status.success() {
        pb.finish_with_message(format!("✗ Failed to pull {image_ref}"));
        anyhow::bail!("tart pull {image_ref} failed with {status}");
    }

    pb.finish_with_message(format!("✓ Pulled {image_ref}"));

    // Update cache.
    let mut cache = ImageCache::load()?;
    cache.insert(image_ref, image_ref)?;

    Ok(image_ref.to_string())
}

/// Ask the user whether to wait for the image pull or fallback to unsandboxed claude.
/// Returns `true` to proceed with the pull, `false` to fallback.
/// Non-interactive sessions always proceed with the pull.
fn prompt_pull_or_fallback(image_ref: &str) -> bool {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        // Non-interactive: proceed with pull (CI, scripts, etc.)
        return true;
    }

    eprintln!("claude-box: image {image_ref} is not cached and needs to be downloaded.");
    eprintln!("claude-box: this may take a while depending on your connection.");
    eprintln!();
    eprintln!("  [W] Wait for download (sandboxed)");
    eprintln!("  [F] Fallback to host claude now");
    eprintln!();
    eprintln!("  ⚠  Fallback runs claude directly on your host WITHOUT sandbox isolation.");
    eprintln!();
    eprint!("claude-box: [W/f] ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return true; // default to pull on read error
    }
    let answer = input.trim().to_lowercase();
    // Default (empty) = wait
    if answer.is_empty() || answer == "w" || answer == "wait" {
        return true;
    }
    if answer == "f" || answer == "fallback" {
        return false;
    }
    // Unrecognised input → default to wait
    true
}

async fn import_ipsw(ipsw_path: &Path) -> Result<String> {
    // Validate path exists and is readable.
    if !ipsw_path.exists() {
        anyhow::bail!("IPSW path does not exist: {}", ipsw_path.display());
    }
    std::fs::metadata(ipsw_path)
        .with_context(|| format!("cannot read IPSW at {}", ipsw_path.display()))?;

    let stem = ipsw_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("imported")
        .to_string();

    // Idempotency: skip if already imported.
    if image_exists_in_tart(&stem).await? {
        info!("IPSW already imported as {stem}, skipping");
        return Ok(stem);
    }

    info!("importing IPSW {} as {stem}", ipsw_path.display());
    let pb = make_spinner(format!("Importing IPSW {} as {stem}", ipsw_path.display()));

    // No timeout — IPSW imports can take a long time for large images.
    let status = Command::new("tart")
        .args(["create", "--from-ipsw", ipsw_path.to_str().unwrap(), &stem])
        .status()
        .await
        .context("failed to spawn tart create --from-ipsw")?;

    if !status.success() {
        pb.finish_with_message(format!("✗ Import failed for {stem}"));
        anyhow::bail!(
            "tart create --from-ipsw {} failed with {status}",
            ipsw_path.display()
        );
    }

    pb.finish_with_message(format!("✓ Imported {stem}"));
    Ok(stem)
}
