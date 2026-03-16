use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A group of host binaries from the same parent directory, to be mounted
/// as a single read-only VirtioFS share.
#[derive(Debug)]
pub struct BinaryDirShare {
    /// The host directory to mount (parent of one or more resolved binaries).
    pub host_dir: PathBuf,
    /// VirtioFS share tag (e.g. `"claude-box-bin-0"`).
    pub tag: String,
    /// Names to expose on the guest PATH (original command names, e.g. "ddtool").
    pub names: Vec<String>,
    /// Canonical filenames inside `host_dir` corresponding to each entry in
    /// `names` (same length, same order). Often identical to `names`, but
    /// differs when the binary is a symlink whose target has a different name
    /// (e.g. "ddtool" → "ddtool_darwin_arm64").  The symlink farm in the
    /// guest uses these as the src filename when creating bin/ddtool →
    /// src-N/ddtool_darwin_arm64.
    pub canonical_filenames: Vec<String>,
}

/// Result of preparing the tool environment for a sandbox run.
pub struct ToolEnv {
    /// Staging directory containing `settings.json` and `claude.json` for
    /// injection into the VM guest. Mounted as the `claude-box-config`
    /// VirtioFS share (cold path) or transferred via SSH (warm path).
    pub config_staging_dir: TempDir,
    /// Per-directory binary shares — one per unique parent directory of the
    /// allowed binaries. Mounted read-only; a guest-side symlink farm exposes
    /// only the named binaries on PATH.
    pub binary_shares: Vec<BinaryDirShare>,
    /// The `apiKeyHelper` command extracted from the host's settings.json.
    /// Run on the **host** at exec time to produce an API key for the guest.
    pub api_key_helper: Option<String>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the Claude settings file under `home`.
/// Tries `~/.claude/settings.json` first, then `~/.claude.json`.
fn find_settings_file(home: &Path) -> Option<PathBuf> {
    let p1 = home.join(".claude").join("settings.json");
    if p1.exists() {
        return Some(p1);
    }
    let p2 = home.join(".claude.json");
    if p2.exists() {
        return Some(p2);
    }
    None
}

/// Exact keys from `~/.claude.json` that are safe to forward into the guest VM.
const CLAUDE_JSON_FORWARD_KEYS: &[&str] = &[
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
    "showSpinnerTree",
    // Contains fingerprints (not actual keys) of API keys the user has approved.
    // Forwarding this suppresses the "Detected a custom API key" warning.
    "customApiKeyResponses",
];

/// Key prefixes from `~/.claude.json` that are safe to forward.
/// Matches any key starting with these prefixes, so new `hasShown*` keys
/// added by future Claude CLI versions are automatically forwarded.
const CLAUDE_JSON_FORWARD_PREFIXES: &[&str] = &["hasShown", "hasCompleted"];

// ---------------------------------------------------------------------------
// Claude config preparation
// ---------------------------------------------------------------------------

/// Prepare the Claude CLI configuration to inject into the guest VM.
///
/// Reads the host's `~/.claude/settings.json`:
/// - Extracts and strips `apiKeyHelper` (run on host, not guest)
/// - Applies MCP server filtering if `allow_tools` is non-empty
/// - Writes the result to `staging_dir/settings.json`
///
/// Reads the host's `~/.claude.json`:
/// - Allowlists safe fields (onboarding/display prefs)
/// - Writes the result to `staging_dir/claude.json`
///
/// Returns `(api_key_helper, staging_dir)`.
fn prepare_claude_config(
    allow_tools: &[String],
    home: &Path,
    project_dir: &Path,
) -> Result<(Option<String>, TempDir)> {
    let staging = TempDir::new().context("creating config staging dir")?;
    let mut api_key_helper: Option<String> = None;

    // ── settings.json ────────────────────────────────────────────────────
    if let Some(settings_path) = find_settings_file(home) {
        let raw = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        let mut settings: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", settings_path.display()))?;

        // Extract apiKeyHelper before writing to staging (it runs on host only).
        if let Some(obj) = settings.as_object_mut() {
            if let Some(helper) = obj.remove("apiKeyHelper") {
                api_key_helper = helper.as_str().map(|s| s.to_string());
            }
        }

        // Suppress the "Claude Code running in Bypass Permissions mode" warning.
        // The VM is the sandbox — the bypass warning is noise.
        if let Some(obj) = settings.as_object_mut() {
            obj.insert(
                "skipDangerousModePermissionPrompt".to_string(),
                Value::Bool(true),
            );
        }

        // Apply MCP filtering.
        if !allow_tools.is_empty() {
            filter_mcp_servers(&mut settings, allow_tools);
        }

        std::fs::write(
            staging.path().join("settings.json"),
            serde_json::to_string_pretty(&settings)?,
        )
        .context("writing settings.json to staging dir")?;
    }

    // ── claude.json (onboarding/display prefs + trust + installMethod) ──
    let mut forwarded = serde_json::Map::new();

    // Forward allowlisted fields from the host's ~/.claude.json.
    let claude_json_path = home.join(".claude.json");
    if claude_json_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&claude_json_path) {
            if let Ok(full) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = full.as_object() {
                    for (key, val) in obj {
                        let dominated = CLAUDE_JSON_FORWARD_KEYS.contains(&key.as_str())
                            || CLAUDE_JSON_FORWARD_PREFIXES
                                .iter()
                                .any(|p| key.starts_with(p));
                        if dominated {
                            forwarded.insert(key.clone(), val.clone());
                        }
                    }
                }
            }
        }
    }

    // Pre-approve the project directory so Claude doesn't show the
    // "Is this a project you trust?" prompt (--dangerously-skip-permissions
    // doesn't bypass this — see claude-code#28506).
    let project_key = project_dir.to_string_lossy().to_string();
    let mut project_entry = serde_json::Map::new();
    project_entry.insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
    project_entry.insert("allowedTools".to_string(), Value::Array(vec![]));
    let mut projects = serde_json::Map::new();
    projects.insert(project_key, Value::Object(project_entry));
    forwarded.insert("projects".to_string(), Value::Object(projects));

    // Set installMethod so the CLI doesn't warn about a missing
    // ~/.local/bin directory when the binary is mounted from the host.
    forwarded.insert(
        "installMethod".to_string(),
        Value::String("system".to_string()),
    );

    std::fs::write(
        staging.path().join("claude.json"),
        serde_json::to_string_pretty(&Value::Object(forwarded))?,
    )
    .context("writing claude.json to staging dir")?;

    Ok((api_key_helper, staging))
}

fn filter_mcp_servers(settings: &mut Value, allow: &[String]) {
    let allow_set: std::collections::HashSet<&str> = allow.iter().map(|s| s.as_str()).collect();

    if let Some(servers) = settings
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
    {
        servers.retain(|name, _| {
            let keep = allow_set.contains(name.as_str());
            if !keep {
                debug!("MCP server '{name}' excluded by allowlist");
            }
            keep
        });
    }
}

// ---------------------------------------------------------------------------
// Binary resolution (zero-copy, VirtioFS mount + guest symlink farm)
// ---------------------------------------------------------------------------

/// Resolve each requested binary to its canonical host path, group by parent
/// directory, and return one [`BinaryDirShare`] per unique directory.
///
/// Each directory will be mounted as a read-only VirtioFS share; `guest_setup`
/// then creates a symlink farm so only the named binaries appear on the guest PATH.
///
/// Hard error if any requested binary cannot be found on `PATH`.
pub fn resolve_binaries(allow_binaries: &[String]) -> Result<Vec<BinaryDirShare>> {
    if allow_binaries.is_empty() {
        return Ok(vec![]);
    }

    // Maintain insertion order for deterministic share indices.
    let mut dirs: Vec<PathBuf> = Vec::new();
    // Map dir → list of (exposed_name, canonical_filename) pairs.
    let mut entries_by_dir: HashMap<PathBuf, Vec<(String, String)>> = HashMap::new();

    for name in allow_binaries {
        let src = which::which(name)
            .map_err(|_| anyhow::anyhow!("binary '{name}' not found on host PATH"))?;
        // Canonicalize to resolve host-side symlinks so the shared directory
        // actually contains the binary (not just a dangling symlink pointing
        // out of the share).
        let canonical = src
            .canonicalize()
            .with_context(|| format!("canonicalizing path for '{name}'"))?;
        let dir = canonical
            .parent()
            .with_context(|| format!("binary '{name}' has no parent directory"))?
            .to_path_buf();
        // The canonical filename inside `dir` (may differ from `name` when the
        // binary is a renamed symlink, e.g. "ddtool" → "ddtool_darwin_arm64").
        let canonical_filename = canonical
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(name)
            .to_string();

        if !entries_by_dir.contains_key(&dir) {
            dirs.push(dir.clone());
        }
        let entries = entries_by_dir.entry(dir).or_default();
        if !entries.iter().any(|(n, _)| n == name) {
            entries.push((name.clone(), canonical_filename.clone()));
        }
        debug!("resolved binary '{name}' → {} (file: {canonical_filename})", canonical.display());
    }

    if dirs.len() > 12 {
        warn!(
            "{} unique binary directories requested; VirtioFS share count is high \
             (tart practical limit ~16)",
            dirs.len()
        );
    }

    let shares = dirs
        .into_iter()
        .enumerate()
        .map(|(i, dir)| {
            let entries = entries_by_dir.remove(&dir).unwrap_or_default();
            let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
            let canonical_filenames: Vec<String> = entries.into_iter().map(|(_, c)| c).collect();
            BinaryDirShare {
                host_dir: dir,
                tag: format!("claude-box-bin-{i}"),
                names,
                canonical_filenames,
            }
        })
        .collect();

    Ok(shares)
}

// ---------------------------------------------------------------------------
// Top-level prepare
// ---------------------------------------------------------------------------

/// Prepare Claude CLI config, MCP filtering, and binary resolution.
pub fn prepare(
    allow_tools: &[String],
    allow_binaries: &[String],
    project_dir: &Path,
) -> Result<ToolEnv> {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let (api_key_helper, config_staging_dir) =
        prepare_claude_config(allow_tools, &home, project_dir)?;
    let binary_shares = resolve_binaries(allow_binaries)?;

    Ok(ToolEnv {
        config_staging_dir,
        binary_shares,
        api_key_helper,
    })
}

/// Return the guest path of the binary symlink farm.
#[allow(dead_code)]
pub fn guest_bin_path() -> &'static Path {
    Path::new("/opt/claude-box/bin")
}

// ---------------------------------------------------------------------------
// Testable variant that accepts an explicit home directory.
// ---------------------------------------------------------------------------

/// Like `prepare_claude_config` but returns just the settings Value for testing.
/// Useful for integration tests that need to avoid touching the real `$HOME`.
#[allow(dead_code)]
pub fn prepare_mcp_config_with_home(
    allow_tools: &[String],
    home: &Path,
) -> Result<Option<(PathBuf, Value)>> {
    let settings_path = find_settings_file(home);

    let Some(settings_path) = settings_path else {
        debug!(
            "no settings file found under {}; skipping MCP config injection",
            home.display()
        );
        return Ok(None);
    };

    let raw = std::fs::read_to_string(&settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut settings: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", settings_path.display()))?;

    if !allow_tools.is_empty() {
        filter_mcp_servers(&mut settings, allow_tools);
    }

    Ok(Some((settings_path, settings)))
}
