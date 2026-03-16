# Milestone 7 — Operational Hardening

## Goal

Make claude-box safe for fleet deployment (thousands of concurrent engineers) by eliminating resource leaks, adding defensive timeouts, and providing garbage collection for orphaned VMs.

## Problem

If any step fails after `vm.create()` in sandbox.rs, or if the host process is SIGKILLed/rebooted, the cloned VM (~20GB) is never deleted. At scale: 1000 engineers × 5% failure rate = ~1TB/day disk leak.

## Changes

### 7.1 — VM Cleanup Guard (`src/sandbox.rs`)

After `vm.create()`, all subsequent work runs in `run_inner()`. If it returns `Err`, `VmGuard::cleanup()` does best-effort stop+delete. On success, the guard is disarmed and explicit stop/delete runs as before.

### 7.2 — hdiutil Mount Guard (`src/vm/ssh_key.rs`)

`MountGuard` struct with synchronous `Drop` that calls `hdiutil detach -force`. Prevents leaked disk mounts if any step between attach and detach fails.

### 7.3 — Orphan VM Reaper (`src/gc.rs`, `src/main.rs`, `src/sandbox.rs`)

- `claude-box gc` subcommand: finds all `claude-box-*` VMs via `tart list`, stops and deletes them.
- Automatic pre-run reap: `Sandbox::run()` calls `gc::reap_stale_vms(1h)` at startup. Only reaps VMs older than 1 hour. Best-effort — errors logged and swallowed.

### 7.4 — Image Cache File Locking (`src/vm/image_cache.rs`)

- Advisory `flock(LOCK_EX)` via `libc` on a sibling `.lock` file. Lock held for the lifetime of `ImageCache`.
- Atomic writes: save to `.json.tmp`, then `rename()`.

### 7.5 — SSH Exponential Backoff (`src/vm/health.rs`)

Replaced fixed 2s sleep with exponential backoff: 500ms initial, 2x per attempt, 16s cap, ±25% jitter from system clock nanos.

### 7.6 — Tart Operation Timeouts (`src/vm/tart.rs`, `src/vm/health.rs`)

Tart CLI calls wrapped in `tokio::time::timeout()`:
- clone: 300s, stop/delete: 30s, ip: 60s
- Override all via `CLAUDE_BOX_TART_TIMEOUT_SECS` env var.
- **No timeout on image pull/import**: macOS VM images are large (10–20GB) and pull times vary widely. Timeouts caused false failures on slow connections.

### 7.7 — Host Fallback on Sandbox Failure (`src/main.rs`)

If `sandbox.run()` returns an error (infrastructure failure), claude-box warns on stderr and falls back to running `claude` directly on the host via `execvp`. This ensures engineers always get a working claude session even when tart is missing, the image can't be pulled, or the VM won't boot. Disabled with `--no-fallback` for CI environments where sandboxing must be enforced.

### 7.8 — Image Pull Prompt (`src/vm/image.rs`, `src/main.rs`)

When an image needs to be downloaded (not cached locally), the user is prompted interactively:
- **[W] Wait** (default): proceed with the pull, run sandboxed
- **[F] Fallback**: skip the pull, run claude directly on host with a security warning

The prompt includes a clear `⚠` warning that fallback runs WITHOUT sandbox isolation. Non-interactive sessions (CI, pipes) skip the prompt and always pull. `UserRequestedFallback` error type propagates the choice cleanly to `main.rs`, which skips the "sandbox failed" message and falls back gracefully — even when `--no-fallback` is set, since the user explicitly chose it.

## Files Modified

- `src/sandbox.rs` — VmGuard + run_inner() refactor + pre-run reap
- `src/vm/ssh_key.rs` — MountGuard around hdiutil
- `src/vm/tart.rs` — timeout wrapper for all tart commands
- `src/vm/health.rs` — exponential backoff + tart ip timeout
- `src/vm/image.rs` — image pull prompt + `UserRequestedFallback` error type (timeouts removed)
- `src/vm/image_cache.rs` — flock + atomic write
- `src/main.rs` — `gc` subcommand dispatch, `--no-fallback` flag, host fallback logic
- `src/gc.rs` — orphan reaper (new)

## Verification

```bash
cargo build && cargo clippy -- -D warnings && cargo test  # all pass
```

## Review

All 8 tasks implemented. No new crate dependencies — uses `libc` (flock), `tempfile` (atomic write), `std::time` (jitter) already present.
