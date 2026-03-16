use anyhow::{Context, Result};
use tokio::process::Command;

use crate::vm::image::{image_exists_in_tart, resolve_image, PullPolicy};
use crate::vm::image_cache::ImageCache;

pub async fn run_images_command(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("list") | None => cmd_list().await,
        Some("pull") => {
            let image_ref = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: claude-box images pull <ref>"))?;
            cmd_pull(image_ref).await
        }
        Some("rm") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: claude-box images rm <ref|name>"))?;
            cmd_rm(name).await
        }
        Some("prune") => cmd_prune().await,
        Some(other) => {
            eprintln!("unknown images subcommand: {other}");
            Ok(1)
        }
    }
}

async fn cmd_list() -> Result<i32> {
    let cache = ImageCache::load()?;
    let entries = cache.entries();

    if entries.is_empty() {
        println!("No cached images.");
    } else {
        println!("{:<60} {:<40} PULLED AT", "IMAGE REF", "LOCAL NAME");
        println!("{}", "-".repeat(110));
        for (image_ref, entry) in entries {
            println!(
                "{:<60} {:<40} {}",
                image_ref, entry.local_name, entry.pulled_at
            );
        }
    }

    println!();
    println!("--- tart list ---");
    let output = Command::new("tart")
        .args(["list"])
        .output()
        .await
        .context("failed to spawn tart list")?;
    print!("{}", String::from_utf8_lossy(&output.stdout));

    Ok(0)
}

async fn cmd_pull(image_ref: &str) -> Result<i32> {
    let local_name = resolve_image(image_ref, &PullPolicy::Always).await?;
    println!("Pulled {image_ref} → {local_name}");
    Ok(0)
}

async fn cmd_rm(image_ref: &str) -> Result<i32> {
    let mut cache = ImageCache::load()?;

    // Determine the local tart name: check cache first, fall back to image_ref itself.
    let local_name = if let Some(entry) = cache.get(image_ref) {
        entry.local_name.clone()
    } else {
        image_ref.to_string()
    };

    // Remove from cache.
    cache.remove(image_ref)?;

    // Delete from tart if it exists.
    if image_exists_in_tart(&local_name).await? {
        let status = Command::new("tart")
            .args(["delete", &local_name])
            .status()
            .await
            .context("failed to spawn tart delete")?;
        if !status.success() {
            anyhow::bail!("tart delete {local_name} failed with {status}");
        }
        println!("Deleted {local_name}");
    } else {
        println!("Image {local_name} not found in tart (removed from cache only)");
    }

    Ok(0)
}

async fn cmd_prune() -> Result<i32> {
    let mut cache = ImageCache::load()?;
    let entries: Vec<(String, String)> = cache
        .entries()
        .iter()
        .map(|(k, v)| (k.clone(), v.local_name.clone()))
        .collect();

    if entries.is_empty() {
        println!("Nothing to prune.");
        return Ok(0);
    }

    for (image_ref, local_name) in &entries {
        // Remove from tart if present.
        if image_exists_in_tart(local_name).await? {
            let status = Command::new("tart")
                .args(["delete", local_name])
                .status()
                .await
                .context("failed to spawn tart delete")?;
            if !status.success() {
                eprintln!("warning: tart delete {local_name} failed with {status}");
            } else {
                println!("Deleted {local_name}");
            }
        }
        cache.remove(image_ref)?;
    }

    println!("Pruned {} image(s).", entries.len());
    Ok(0)
}
