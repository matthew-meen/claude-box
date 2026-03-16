use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheEntry {
    pub local_name: String,
    pub pulled_at: String, // epoch seconds as string
}

// ---------------------------------------------------------------------------
// Advisory file lock (flock-based) for concurrent process safety.
// ---------------------------------------------------------------------------

struct FileLock {
    file: File,
}

impl FileLock {
    /// Acquire an exclusive advisory lock on the given path.
    /// Creates the file if it doesn't exist.
    fn acquire(path: &PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir for lock at {}", parent.display()))?;
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open lock file {}", path.display()))?;

        // SAFETY: flock on a valid fd we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            anyhow::bail!(
                "flock failed on {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: unlocking a valid fd we own.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

// ---------------------------------------------------------------------------
// Image cache
// ---------------------------------------------------------------------------

pub struct ImageCache {
    path: PathBuf,
    entries: HashMap<String, CacheEntry>,
    _lock: FileLock,
}

impl ImageCache {
    /// Load the cache from ~/.cache/claude-box/images.json, creating if absent.
    ///
    /// Acquires an exclusive advisory lock for the lifetime of this `ImageCache`.
    /// Drop the cache to release the lock.
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME env var not set")?;
        let path = PathBuf::from(home)
            .join(".cache")
            .join("claude-box")
            .join("images.json");

        let lock_path = path.with_extension("lock");
        let lock = FileLock::acquire(&lock_path)
            .context("failed to acquire image cache lock")?;

        let entries = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read cache at {}", path.display()))?;
            serde_json::from_str(&data)
                .with_context(|| format!("failed to parse cache at {}", path.display()))?
        } else {
            HashMap::new()
        };

        Ok(Self { path, entries, _lock: lock })
    }

    /// Get a cached entry by image ref.
    pub fn get(&self, image_ref: &str) -> Option<&CacheEntry> {
        self.entries.get(image_ref)
    }

    /// Insert or update an entry.
    pub fn insert(&mut self, image_ref: &str, local_name: &str) -> Result<()> {
        let pulled_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        self.entries.insert(
            image_ref.to_string(),
            CacheEntry {
                local_name: local_name.to_string(),
                pulled_at,
            },
        );
        self.save()
    }

    /// Remove an entry.
    pub fn remove(&mut self, image_ref: &str) -> Result<()> {
        self.entries.remove(image_ref);
        self.save()
    }

    /// Save cache to disk atomically (write temp file, then rename).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create cache dir {}", parent.display()))?;
        }
        let data = serde_json::to_string_pretty(&self.entries)
            .context("failed to serialize image cache")?;

        // Atomic write: write to a sibling temp file, then rename.
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data)
            .with_context(|| format!("failed to write temp cache at {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to rename temp cache to {}", self.path.display()))?;
        Ok(())
    }

    /// Return all entries.
    pub fn entries(&self) -> &HashMap<String, CacheEntry> {
        &self.entries
    }
}
