//! Warm VM pool — pre-suspended VMs for fast sandbox startup.
//!
//! Design:
//! - One warm VM per base image, kept in a `--suspendable` suspended state.
//! - Single VirtioFS slot at `~/.cache/claude-box/warm-slots/project` (symlink).
//!   VZF snapshot/restore is only reliable with exactly 1 VirtioFS share.
//! - The claude binary is injected directly into the warm VM disk via hdiutil
//!   (no VirtioFS share needed for it).
//! - Binary allowlists (--allow-binary) are incompatible with the warm pool;
//!   callers must disable warm pool when binary_shares is non-empty.
//! - Per-run: update project-slot symlink → clone warm VM → `tart run` with the
//!   slot path → VM resumes from snapshot → SSH with persisted warm key.
//! - Key is injected ONCE into the warm VM disk at creation; clones inherit
//!   authorized_keys from the snapshot (no per-run hdiutil = no disk mutation).
//! - Warm VMs are rebuilt weekly or on `claude-box warm refresh`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::relay::SshSession;
use crate::vm::{health, ssh_key, tart::TartVm, Vm, VmConfig};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Drain up to 4 KiB from a piped child stderr handle (200ms timeout).
async fn drain_tart_stderr(handle: &mut Option<tokio::process::ChildStderr>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(stderr) = handle else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        stderr.read_to_end(&mut buf),
    )
    .await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Re-create the warm VM when it is older than this.
const REFRESH_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// SSH timeout when resuming from snapshot (faster than cold boot).
const RESUME_SSH_TIMEOUT_SECS: u64 = 30;

// ── File lock (flock-based advisory lock) ────────────────────────────────────

struct FileLock {
    _file: File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir for lock at {}", parent.display()))?;
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        // SAFETY: flock on a valid fd we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            anyhow::bail!("flock failed on {}: {}", path.display(), std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: unlocking a valid fd we own.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

// ── State file ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct WarmPoolEntry {
    tart_name: String,
    created_at: u64, // Unix epoch seconds
}

struct WarmPoolState {
    path: PathBuf,
    entries: HashMap<String, WarmPoolEntry>,
}

impl WarmPoolState {
    fn load(cache_dir: &Path) -> Result<Self> {
        let path = cache_dir.join("warm-pool.json");
        let entries = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("read warm-pool state {}", path.display()))?;
            serde_json::from_str(&data)
                .with_context(|| format!("parse warm-pool state {}", path.display()))?
        } else {
            HashMap::new()
        };
        Ok(Self { path, entries })
    }

    fn get(&self, image_ref: &str) -> Option<&WarmPoolEntry> {
        self.entries.get(image_ref)
    }

    fn set(&mut self, image_ref: &str, tart_name: &str) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.entries.insert(
            image_ref.to_string(),
            WarmPoolEntry { tart_name: tart_name.to_string(), created_at: now },
        );
        self.save()
    }

    fn remove(&mut self, image_ref: &str) -> Result<()> {
        self.entries.remove(image_ref);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.entries)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

// ── Slot management ──────────────────────────────────────────────────────────

/// Single VirtioFS slot for the project directory.
///
/// The slot path is fixed and baked into the warm VM snapshot; only the
/// symlink target changes per run. VZF snapshot/restore requires exactly
/// the same number and paths of VirtioFS shares — 1 slot is reliably stable.
struct WarmSlots {
    root: PathBuf,        // ~/.cache/claude-box/warm-slots/
    placeholder: PathBuf, // empty dir for unused slot
}

impl WarmSlots {
    fn new(cache_dir: &Path) -> Result<Self> {
        let root = cache_dir.join("warm-slots");
        let placeholder = cache_dir.join("warm-slot-placeholder");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&placeholder)?;
        Ok(Self { root, placeholder })
    }

    fn project_slot(&self) -> PathBuf {
        self.root.join("project")
    }

    /// The `--dir=` flag value for `tart run` (1 slot).
    fn tart_dir_arg(&self) -> String {
        format!("{}:tag=project", self.project_slot().display())
    }

    /// Point the project slot symlink at `project_dir`.
    fn activate(&self, project_dir: &Path) -> Result<()> {
        atomic_symlink(&self.project_slot(), project_dir)
    }

    /// Point the project slot symlink at the placeholder (for warm VM creation).
    fn init_placeholder(&self) -> Result<()> {
        atomic_symlink(&self.project_slot(), &self.placeholder)
    }
}

/// Atomically replace a symlink at `link` to point to `target`.
fn atomic_symlink(link: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    if link.exists() || link.is_symlink() {
        std::fs::remove_file(link)
            .with_context(|| format!("remove symlink {}", link.display()))?;
    }
    symlink(target, link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
    Ok(())
}

// ── Short hash ───────────────────────────────────────────────────────────────

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ── Cache dir helper ─────────────────────────────────────────────────────────

fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".cache").join("claude-box"))
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Try to boot a sandbox run via the warm pool.
///
/// Returns `(run_vm_name, ssh_session, tart_child)` on success. The caller
/// must stop/delete the run VM after use.
///
/// Falls through (returns `Err`) if the warm pool is unavailable or fails so
/// the caller can fall back to cold boot.
///
/// NOTE: The warm key is baked into the snapshot (injected once at warm VM
/// creation). No per-run hdiutil injection — that would modify disk.img and
/// cause VZF to reject the snapshot at resume time.
///
/// NOTE: Binary allowlists are incompatible with the warm pool (more than 1
/// VirtioFS share causes unreliable VZF snapshot/restore). The caller must
/// ensure no binary shares are needed before calling this function.
pub async fn try_warm_boot(
    image_ref: &str,
    base_image: &str,
    project_dir: &Path,
    claude_bin: &Path,
) -> Result<(String, SshSession, Child)> {
    let dir = cache_dir()?;
    let lock_path = dir.join("warm-pool.lock");
    let slots = WarmSlots::new(&dir)?;

    // Ensure we have a valid warm VM (create/refresh as needed).
    let warm_name =
        ensure_warm_vm(image_ref, base_image, claude_bin, &dir, &lock_path, &slots).await?;

    // Load the persisted warm SSH key (written once at warm VM creation).
    let warm_keypair = load_warm_key(&dir, image_ref)?;

    // Acquire lock: symlink update + clone + spawn must be atomic.
    let lock = FileLock::acquire(&lock_path)?;

    // Update project slot symlink for this run's project directory.
    slots.activate(project_dir)?;

    // Clone warm VM to a fresh run instance.
    let run_name = format!("claude-box-{}", uuid::Uuid::new_v4());
    let tart = TartVm::new();
    let clone_config = VmConfig {
        name: run_name.clone(),
        base_image: warm_name,
        dir_shares: vec![],
    };
    tart.create(&clone_config).await.context("clone warm VM")?;

    // Spawn tart run — VM resumes from snapshot with the single slot path.
    // --suspendable is required to resume from the parent's saved state.
    let run_args = vec![
        "run".to_string(),
        "--suspendable".to_string(),
        "--no-graphics".to_string(),
        format!("--dir={}", slots.tart_dir_arg()),
        run_name.clone(),
    ];

    let mut child = Command::new("tart")
        .args(&run_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn tart run for warm clone")?;
    let mut tart_stderr = child.stderr.take();

    // Release the lock — tart has exec'd and VirtioFS dir is opened.
    drop(lock);

    // Wait for SSH using the warm key (shorter timeout than cold boot).
    let ip = match health::get_vm_ip(&run_name).await {
        Ok(ip) => ip,
        Err(e) => {
            let extra = drain_tart_stderr(&mut tart_stderr).await;
            let e = e.context("get IP of warm clone");
            if extra.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("tart run stderr:\n{extra}")));
        }
    };
    let session = match health::wait_for_ssh(&ip, &warm_keypair, RESUME_SSH_TIMEOUT_SECS).await {
        Ok(s) => s,
        Err(e) => {
            let extra = drain_tart_stderr(&mut tart_stderr).await;
            let e = e.context("SSH timeout on warm resume");
            if extra.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("tart run stderr:\n{extra}")));
        }
    };

    Ok((run_name, session, child))
}

/// List all warm VMs with their age.
pub async fn list_warm_vms() -> Result<Vec<(String, String, u64)>> {
    let dir = cache_dir()?;
    let state = WarmPoolState::load(&dir)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    let mut result = Vec::new();
    for (image_ref, entry) in &state.entries {
        let age_secs = now.saturating_sub(entry.created_at);
        result.push((image_ref.clone(), entry.tart_name.clone(), age_secs));
    }
    Ok(result)
}

/// Force-refresh the warm VM for the given image ref.
pub async fn refresh_warm_vm(image_ref: &str, base_image: &str, claude_bin: &Path) -> Result<()> {
    let dir = cache_dir()?;
    let lock_path = dir.join("warm-pool.lock");
    let slots = WarmSlots::new(&dir)?;
    {
        let mut state = WarmPoolState::load(&dir)?;
        if let Some(entry) = state.get(image_ref) {
            let old_name = entry.tart_name.clone();
            state.remove(image_ref)?;
            delete_warm_vm_if_exists(&old_name).await;
        }
    }
    ensure_warm_vm(image_ref, base_image, claude_bin, &dir, &lock_path, &slots).await?;
    Ok(())
}

/// Delete all warm VMs and clear the state file.
pub async fn delete_all_warm_vms() -> Result<u32> {
    let dir = cache_dir()?;
    let mut state = WarmPoolState::load(&dir)?;
    let mut deleted = 0u32;
    let entries: Vec<(String, String)> = state
        .entries
        .iter()
        .map(|(k, v)| (k.clone(), v.tart_name.clone()))
        .collect();
    for (image_ref, tart_name) in entries {
        delete_warm_vm_if_exists(&tart_name).await;
        let _ = std::fs::remove_file(warm_key_path(&dir, &image_ref));
        state.remove(&image_ref)?;
        deleted += 1;
    }
    Ok(deleted)
}

// ── Warm key persistence ─────────────────────────────────────────────────────

fn warm_key_path(cache_dir: &Path, image_ref: &str) -> PathBuf {
    let hash = &short_hash(image_ref)[..8];
    cache_dir.join("warm-keys").join(format!("{hash}.pem"))
}

fn save_warm_key(cache_dir: &Path, image_ref: &str, keypair: &russh_keys::key::KeyPair) -> Result<()> {
    let path = warm_key_path(cache_dir, image_ref);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let pem = ssh_key::serialize_keypair(keypair)?;
    std::fs::write(&path, pem)?;
    // Restrict to owner read only (600).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_warm_key(cache_dir: &Path, image_ref: &str) -> Result<russh_keys::key::KeyPair> {
    let path = warm_key_path(cache_dir, image_ref);
    let pem = std::fs::read(&path)
        .with_context(|| format!("read warm key at {}", path.display()))?;
    ssh_key::deserialize_keypair(&pem)
}

// ── Internal: warm VM lifecycle ───────────────────────────────────────────────

/// Return a valid warm VM tart name, creating or refreshing as needed.
async fn ensure_warm_vm(
    image_ref: &str,
    base_image: &str,
    claude_bin: &Path,
    dir: &Path,
    lock_path: &Path,
    slots: &WarmSlots,
) -> Result<String> {
    let state = WarmPoolState::load(dir)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    if let Some(entry) = state.get(image_ref) {
        let age = now.saturating_sub(entry.created_at);
        if age < REFRESH_AGE.as_secs() && tart_vm_exists(&entry.tart_name).await {
            return Ok(entry.tart_name.clone());
        }
        // Stale or missing — delete and recreate.
        let old = entry.tart_name.clone();
        drop(state);
        let mut state = WarmPoolState::load(dir)?;
        state.remove(image_ref)?;
        drop(state);
        delete_warm_vm_if_exists(&old).await;
    }

    create_warm_vm(image_ref, base_image, claude_bin, dir, lock_path, slots).await
}

/// Clone + inject + boot + suspend a fresh warm VM. Returns its tart name.
async fn create_warm_vm(
    image_ref: &str,
    base_image: &str,
    claude_bin: &Path,
    dir: &Path,
    _lock_path: &Path,
    slots: &WarmSlots,
) -> Result<String> {
    let warm_name = format!("claude-box-warm-{}", &short_hash(image_ref)[..8]);
    info!("creating warm VM {warm_name} from {base_image}");

    // Clean up any stale VM with this name.
    delete_warm_vm_if_exists(&warm_name).await;

    // Point the project slot at the placeholder for initial boot.
    slots.init_placeholder()?;

    // Clone base image.
    let tart = TartVm::new();
    let vm_config =
        VmConfig { name: warm_name.clone(), base_image: base_image.to_string(), dir_shares: vec![] };
    tart.create(&vm_config).await.context("clone base image for warm VM")?;

    // Generate the warm SSH key. Injected ONCE into the warm VM disk;
    // all clones inherit authorized_keys from the snapshot.
    // Also bakes the claude binary into the disk in the same hdiutil session
    // to avoid resource-busy issues from a second attach after Spotlight
    // starts indexing the first write.
    let warm_key = ssh_key::generate()?;
    ssh_key::inject_warm_prereqs(&warm_name, &warm_key.authorized_keys_line, claude_bin)
        .await
        .context("inject SSH key + claude binary for warm VM")?;
    save_warm_key(dir, image_ref, &warm_key.keypair)?;

    // Boot warm VM with --suspendable and the single project slot.
    let run_args = vec![
        "run".to_string(),
        "--suspendable".to_string(),
        "--no-graphics".to_string(),
        format!("--dir={}", slots.tart_dir_arg()),
        warm_name.clone(),
    ];

    let mut warm_child = Command::new("tart")
        .args(&run_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn tart run for warm VM")?;
    let mut tart_stderr = warm_child.stderr.take();

    // Wait for SSH — confirms the VM is fully booted before suspending.
    let ip_result = health::get_vm_ip(&warm_name).await;
    match ip_result {
        Ok(ip) => {
            if let Err(e) = health::wait_for_ssh(&ip, &warm_key.keypair, 120).await {
                let extra = drain_tart_stderr(&mut tart_stderr).await;
                let _ = TartVm::new().stop(&warm_name).await;
                let _ = warm_child.wait().await;
                let _ = TartVm::new().delete(&warm_name).await;
                let e = e.context("warm VM SSH timeout during creation");
                if extra.is_empty() {
                    return Err(e);
                }
                return Err(e.context(format!("tart run stderr:\n{extra}")));
            }
        }
        Err(e) => {
            let extra = drain_tart_stderr(&mut tart_stderr).await;
            let _ = TartVm::new().stop(&warm_name).await;
            let _ = warm_child.wait().await;
            let _ = TartVm::new().delete(&warm_name).await;
            let e = e.context("warm VM did not get an IP during creation");
            if extra.is_empty() {
                return Err(e);
            }
            return Err(e.context(format!("tart run stderr:\n{extra}")));
        }
    }

    // Suspend the warm VM.
    let suspend_out = Command::new("tart")
        .args(["suspend", &warm_name])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("tart suspend warm VM")?;
    if !suspend_out.status.success() {
        let _ = TartVm::new().stop(&warm_name).await;
        let _ = warm_child.wait().await;
        let _ = TartVm::new().delete(&warm_name).await;
        let detail = String::from_utf8_lossy(&suspend_out.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            anyhow::bail!("tart suspend failed for warm VM {warm_name}");
        }
        anyhow::bail!("tart suspend failed for warm VM {warm_name}:\n{detail}");
    }
    let _ = warm_child.wait().await;

    // Persist state.
    let mut state = WarmPoolState::load(dir)?;
    state.set(image_ref, &warm_name)?;

    info!("warm VM {warm_name} ready");
    Ok(warm_name)
}

async fn tart_vm_exists(name: &str) -> bool {
    let output = Command::new("tart")
        .args(["list"])
        .output()
        .await
        .unwrap_or_else(|_| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: vec![],
            stderr: vec![],
        });
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| line.split_whitespace().nth(1) == Some(name))
}

async fn delete_warm_vm_if_exists(name: &str) {
    let tart = TartVm::new();
    if let Err(e) = tart.stop(name).await {
        warn!("stop {name}: {e}");
    }
    if let Err(e) = tart.delete(name).await {
        warn!("delete {name}: {e}");
    }
}
