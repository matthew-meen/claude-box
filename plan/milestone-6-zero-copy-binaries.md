# Milestone 6 — Zero-copy Binary Forwarding

## Goal

Replace the copy-based binary staging with read-only VirtioFS mounts of the source
directories, plus a guest-side symlink farm that enforces the allowlist. No binary
data crosses the VM boundary.

---

## Design

### Current flow (copy)
```
which(ddtool) → /usr/local/bin/ddtool
  → fs::copy to TempDir/ddtool
  → TempDir mounted as claude-box-bin VirtioFS share
  → guest PATH prepended with /opt/claude-box/bin
```

### New flow (Option A — mount + symlink)
```
which(ddtool) → /usr/local/bin/ddtool
  → /usr/local/bin/ added as read-only VirtioFS share (tag: claude-box-bin-0)
  → guest: mkdir /opt/claude-box/bin
  → guest: ln -sf /opt/claude-box/src-0/ddtool /opt/claude-box/bin/ddtool
  → guest PATH still prepended with /opt/claude-box/bin
```

Only the explicitly named binaries are symlinked into the guest PATH. Sibling files
in the mounted directory are accessible but invisible to PATH. The mounts are
read-only from the guest.

---

## Tasks

### 6.1 — Replace `stage_binaries` with `resolve_binaries` in tools.rs

Remove: `TempDir` staging, `fs::copy`, `set_permissions`, dylib staging.

Add: `resolve_binaries(allow_binaries: &[String]) -> Result<Vec<ResolvedBinary>>`

```rust
pub struct ResolvedBinary {
    /// Name as specified by --allow-binary.
    pub name: String,
    /// Resolved absolute path on the host (via `which`).
    pub host_path: PathBuf,
}
```

- Still hard-error on missing binary (no change to UX).
- Symlink resolution: call `fs::canonicalize` on the `which` result so the actual
  executable is exposed, not a dangling symlink.
- Remove `staged_dylibs` field from `ToolEnv` (dylib handling is no longer needed
  since we're mounting the source directory directly, which contains the dylibs).
- Replace `binary_staging: Option<TempDir>` with `binary_shares: Vec<BinaryDirShare>`.

```rust
pub struct BinaryDirShare {
    /// The host directory to mount (parent of one or more resolved binaries).
    pub host_dir: PathBuf,
    /// VirtioFS share tag (e.g. "claude-box-bin-0").
    pub tag: String,
    /// Binary names within this directory that are on the allowlist.
    pub names: Vec<String>,
}
```

`prepare()` calls `resolve_binaries`, groups results by `host_path.parent()`, and
builds one `BinaryDirShare` per unique parent directory.

Files touched: `src/tools.rs`

### 6.2 — Update mount.rs to emit per-directory shares

`build_shares` currently takes `binary_staging_dir: Option<&Path>` and emits a
single `claude-box-bin` share.

Change to accept `binary_dir_shares: &[BinaryDirShare]` and emit one `DirShare` per
entry:

```
tag:   claude-box-bin-0, claude-box-bin-1, …
host:  /usr/local/bin, /opt/homebrew/bin, …
guest: /opt/claude-box/src-0, /opt/claude-box/src-1, …
```

The guest mount paths use a stable index (`src-N`) that corresponds to the
`BinaryDirShare` index. `/opt/claude-box/bin` is NOT a VirtioFS share anymore — it
is created by guest_setup as a plain directory holding only symlinks.

Files touched: `src/mount.rs`

### 6.3 — Update guest_setup.rs to create symlink farm

After mounting all VirtioFS shares, add a new step:

```bash
sudo mkdir -p /opt/claude-box/bin
# For each allowed binary:
sudo ln -sf /opt/claude-box/src-N/<name> /opt/claude-box/bin/<name>
```

The symlinks are created via `SshSession::exec`. One `ln -sf` call per binary.

The VirtioFS mounts get a `ro` option when possible — check if `mount_virtiofs`
supports `-o ro` on the target macOS guest version. If not, the read-only
enforcement is best-effort at the VirtioFS layer (tart may support `--dir=<tag>:<path>:ro`).

Files touched: `src/vm/guest_setup.rs`

### 6.4 — Update sandbox.rs

- Pass `&tool_env.binary_shares` to `build_shares` instead of `binary_staging_path`.
- Remove `binary_staging_path` local variable.
- The PATH prepend in `exec_claude` stays unchanged (`/opt/claude-box/bin` is still
  the guest-side PATH entry, now backed by symlinks instead of real files).
- Remove DYLD_LIBRARY_PATH logic (was unused; dylibs are now alongside binaries in
  the mounted source directory automatically).

Files touched: `src/sandbox.rs`

### 6.5 — Update tart --dir flag for read-only mounts

Check if tart supports `--dir=tag:path:ro` (read-only). If so, emit that format for
binary source shares. If not, document the limitation.

Update `DirShare` to carry a `read_only: bool` field. Update `tart_flag()` to
render `:ro` suffix when set. Set `read_only = true` for all binary source shares.

Files touched: `src/mount.rs`

---

## Share count concern

tart supports up to ~16 VirtioFS shares before performance degrades. With the new
scheme, each unique binary parent directory consumes one share. For typical use
(tools from `/usr/local/bin`, `/opt/homebrew/bin`, `/usr/bin`) this is 2–4 shares,
well within budget.

If the user requests binaries from > 12 different directories, emit a warning but
proceed. Document the limit.

---

## What stays the same

- MCP config: still written to a staging TempDir and mounted as `claude-box-config`.
  (Claude needs to write to `~/.claude/` at runtime — sessions, state, etc. — so
  a live read-only mount of the host `~/.claude/` would break it. Keeping the copy
  for config is intentional.)
- Claude binary share: unchanged (`claude-box-claude`, always read-only).
- Project share: unchanged (read-write, same absolute path).

---

## Verification

```bash
# ddtool is accessible but not copied
claude-box --allow-binary ddtool -- sh -c 'which ddtool && ddtool version'
# Verify no TempDir staging exists for binaries in process output

# Sibling binaries are NOT on PATH
claude-box --allow-binary ddtool -- sh -c 'which otool'
# expect: "otool not found"

# Multiple binaries from different directories
claude-box --allow-binary gh --allow-binary jq -- sh -c 'gh --version && jq --version'

# Missing binary still errors before VM starts
claude-box --allow-binary nonexistent_xyz -- echo hi
# expect: hard error, no VM created
```
