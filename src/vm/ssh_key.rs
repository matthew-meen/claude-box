use anyhow::{Context, Result};
use russh_keys::key::KeyPair;
use russh_keys::PublicKeyBase64;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdioSync;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// An ephemeral Ed25519 keypair. Private key stays in memory only.
pub struct EphemeralKey {
    pub keypair: KeyPair,
    /// The formatted public key line, e.g. "ssh-ed25519 <base64>"
    pub authorized_keys_line: String,
}

/// Serialize a keypair to PKCS8 PEM bytes for persistent storage.
pub fn serialize_keypair(key: &KeyPair) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    russh_keys::encode_pkcs8_pem(key, &mut buf)
        .map_err(|e| anyhow::anyhow!("encode_pkcs8_pem failed: {e}"))?;
    Ok(buf)
}

/// Load a keypair from PKCS8 PEM bytes previously written by `serialize_keypair`.
pub fn deserialize_keypair(pem: &[u8]) -> Result<KeyPair> {
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), pem)?;
    russh_keys::load_secret_key(tmp.path(), None)
        .map_err(|e| anyhow::anyhow!("load_secret_key failed: {e}"))
}

/// Generate an ephemeral Ed25519 keypair.
pub fn generate() -> Result<EphemeralKey> {
    let keypair = KeyPair::generate_ed25519();
    let public = keypair.clone_public_key()?;
    let authorized_keys_line = format!("{} {}", public.name(), public.public_key_base64());
    Ok(EphemeralKey {
        keypair,
        authorized_keys_line,
    })
}

/// Inject both the SSH public key and the Claude binary into the VM's disk in
/// a single hdiutil attach/detach cycle.
///
/// Used when creating warm pool VMs. Combining both writes into one session
/// avoids the resource-busy issue that arises when a second attach is attempted
/// after Spotlight begins indexing the newly-written claude binary.
pub async fn inject_warm_prereqs(
    vm_name: &str,
    pubkey_line: &str,
    claude_bin: &Path,
) -> Result<()> {
    let disk_path = vm_disk_path(vm_name)?;
    info!("injecting SSH key and claude binary into {} disk", vm_name);

    // -nobrowse: don't add to Finder/Spotlight, reducing resource-busy races.
    let output = Command::new("hdiutil")
        .args(["attach", "-owners", "off", "-nobrowse", disk_path.to_str().unwrap()])
        .output()
        .await
        .context("failed to run hdiutil attach")?;
    anyhow::ensure!(
        output.status.success(),
        "hdiutil attach failed for {}",
        disk_path.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parent_dev: Option<String> = None;
    let mut data_mount: Option<PathBuf> = None;

    for line in stdout.lines() {
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        let dev = cols[0].trim();

        if parent_dev.is_none() && dev.starts_with("/dev/disk") {
            let suffix = dev.trim_start_matches("/dev/disk");
            if !suffix.contains('s') {
                parent_dev = Some(dev.to_string());
            }
        }

        if cols.len() == 3 {
            let mount = cols[2].trim();
            if !mount.is_empty() {
                let candidate = PathBuf::from(mount);
                if candidate.join("Users").join("admin").exists() {
                    data_mount = Some(candidate);
                }
            }
        }
    }

    let parent_dev = parent_dev.context("hdiutil output had no parent device")?;
    let data_mount =
        data_mount.context("could not find APFS Data volume (no Users/admin) in hdiutil output")?;

    debug!("data volume: {}", data_mount.display());

    let mut guard = DeviceDetachGuard::new(&parent_dev);

    // ── 1. Write SSH public key ───────────────────────────────────────────────
    let ssh_dir = data_mount.join("Users").join("admin").join(".ssh");
    tokio::fs::create_dir_all(&ssh_dir)
        .await
        .context("creating .ssh dir in guest disk")?;

    let auth_keys = ssh_dir.join("authorized_keys");
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&auth_keys)
        .await
        .context("opening authorized_keys")?;
    f.write_all(format!("{}\n", pubkey_line).as_bytes()).await?;
    f.flush().await?;
    drop(f);

    let _ = Command::new("chmod").args(["600", auth_keys.to_str().unwrap()])
        .stdout(StdioSync::null()).stderr(StdioSync::null()).status().await;
    let _ = Command::new("chmod").args(["700", ssh_dir.to_str().unwrap()])
        .stdout(StdioSync::null()).stderr(StdioSync::null()).status().await;

    // ── 2. Write claude binary ────────────────────────────────────────────────
    let claude_dir = data_mount.join("opt").join("claude-box").join("claude");
    tokio::fs::create_dir_all(&claude_dir)
        .await
        .context("creating /opt/claude-box/claude in guest disk")?;

    let claude_dest = claude_dir.join("claude");
    tokio::fs::copy(claude_bin, &claude_dest)
        .await
        .context("copying claude binary to guest disk")?;

    let _ = Command::new("chmod").args(["755", claude_dest.to_str().unwrap()])
        .stdout(StdioSync::null()).stderr(StdioSync::null()).status().await;

    debug!("key + claude binary injected, detaching disk");

    // Use -force to ensure detach succeeds even if the OS briefly holds the
    // volume for indexing after the large binary write.
    let output = Command::new("hdiutil")
        .args(["detach", &parent_dev, "-force"])
        .stdout(StdioSync::null())
        .stderr(StdioSync::piped())
        .output()
        .await
        .context("failed to run hdiutil detach")?;
    anyhow::ensure!(output.status.success(), "hdiutil detach failed for {parent_dev}");

    guard.mark_detached();
    Ok(())
}

/// Returns the path to the cloned VM's disk image.
/// tart stores VMs at ~/.tart/vms/<name>/disk.img
pub fn vm_disk_path(vm_name: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home)
        .join(".tart")
        .join("vms")
        .join(vm_name)
        .join("disk.img");
    anyhow::ensure!(path.exists(), "VM disk not found at {}", path.display());
    Ok(path)
}

// ---------------------------------------------------------------------------
// RAII guard for hdiutil device — guarantees detach even on early return.
// ---------------------------------------------------------------------------

/// Ensures `hdiutil detach` is called on the parent device when dropped,
/// preventing mount leaks if any operation between attach and detach fails.
struct DeviceDetachGuard {
    device: String,
    detached: bool,
}

impl DeviceDetachGuard {
    fn new(device: &str) -> Self {
        Self {
            device: device.to_string(),
            detached: false,
        }
    }

    fn mark_detached(&mut self) {
        self.detached = true;
    }
}

impl Drop for DeviceDetachGuard {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        warn!("DeviceDetachGuard: force-detaching leaked hdiutil device {}", self.device);
        // Synchronous detach — hdiutil detach completes in <200ms.
        let _ = std::process::Command::new("hdiutil")
            .args(["detach", &self.device, "-force"])
            .stdout(StdioSync::null())
            .stderr(StdioSync::null())
            .status();
    }
}

/// Inject the public key into the VM's disk image using hdiutil.
///
/// Attaches the disk image (all APFS volumes auto-mount), then searches for
/// the volume that contains `Users/admin` — which is always the APFS Data
/// volume, not the sealed System volume or the ISC container. Writes the
/// public key into `authorized_keys` there, fixes permissions, and detaches
/// the whole disk image.
pub async fn inject_key(vm_name: &str, pubkey_line: &str) -> Result<()> {
    let disk_path = vm_disk_path(vm_name)?;
    info!("injecting SSH key into {} disk", vm_name);

    // Attach without a fixed mountpoint so all APFS volumes auto-mount.
    // -owners off: disable ownership enforcement so we can write files.
    let output = Command::new("hdiutil")
        .args(["attach", "-owners", "off", disk_path.to_str().unwrap()])
        .output()
        .await
        .context("failed to run hdiutil attach")?;
    anyhow::ensure!(
        output.status.success(),
        "hdiutil attach failed for {}",
        disk_path.display()
    );

    // Parse hdiutil output (tab-separated: device \t type \t mountpoint).
    // Find:
    //   parent_dev  — first /dev/diskN (no 's') for detach
    //   data_mount  — the volume whose root contains Users/admin (APFS Data)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parent_dev: Option<String> = None;
    let mut data_mount: Option<PathBuf> = None;

    for line in stdout.lines() {
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        let dev = cols[0].trim();

        // Whole disks are /dev/diskN; partitions are /dev/diskNsM.
        // Strip the /dev/disk prefix and check the remainder: "4" vs "4s1".
        if parent_dev.is_none() && dev.starts_with("/dev/disk") {
            let suffix = dev.trim_start_matches("/dev/disk");
            if !suffix.contains('s') {
                parent_dev = Some(dev.to_string());
            }
        }

        // Find the volume with the user's home directory.
        if cols.len() == 3 {
            let mount = cols[2].trim();
            if !mount.is_empty() {
                let candidate = PathBuf::from(mount);
                if candidate.join("Users").join("admin").exists() {
                    data_mount = Some(candidate);
                }
            }
        }
    }

    let parent_dev = parent_dev.context("hdiutil output had no parent device")?;
    let data_mount =
        data_mount.context("could not find APFS Data volume (no Users/admin) in hdiutil output")?;

    debug!("data volume: {}", data_mount.display());

    // Guard: detach the parent device on drop (catches panics/early returns).
    let mut guard = DeviceDetachGuard::new(&parent_dev);

    // Write key to authorized_keys.
    let ssh_dir = data_mount.join("Users").join("admin").join(".ssh");
    tokio::fs::create_dir_all(&ssh_dir)
        .await
        .context("creating .ssh dir in guest disk")?;

    let auth_keys = ssh_dir.join("authorized_keys");
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&auth_keys)
        .await
        .context("opening authorized_keys")?;
    f.write_all(format!("{}\n", pubkey_line).as_bytes()).await?;
    f.flush().await?;
    drop(f);

    // Fix permissions (600 for authorized_keys, 700 for .ssh).
    // Ownership is left as the host user's UID, which matches the guest admin
    // UID because macOS VM images use the same starting UID (501/502) as the
    // developer who built them. SSH StrictModes accepts files owned by the
    // connecting user or by root, so no chown is needed.
    let _ = Command::new("chmod").args(["600", auth_keys.to_str().unwrap()])
        .stdout(StdioSync::null()).stderr(StdioSync::null()).status().await;
    let _ = Command::new("chmod").args(["700", ssh_dir.to_str().unwrap()])
        .stdout(StdioSync::null()).stderr(StdioSync::null()).status().await;

    debug!("key injected, detaching disk");

    // Detach the parent device (unmounts all child volumes).
    let output = Command::new("hdiutil")
        .args(["detach", &parent_dev])
        .stdout(StdioSync::null())
        .stderr(StdioSync::piped())
        .output()
        .await
        .context("failed to run hdiutil detach")?;
    anyhow::ensure!(output.status.success(), "hdiutil detach failed for {parent_dev}");

    guard.mark_detached();
    Ok(())
}
