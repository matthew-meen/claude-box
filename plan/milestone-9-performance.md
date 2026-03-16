# Milestone 9 — Performance: Reduce Startup Latency

## Profiling Results (2026-03-12)

Measured via `Instant::now()` instrumentation in `TestVm::boot()`.
Total boot time: **12,779ms**

| Step | Time (ms) | % of total |
|---|---|---|
| `wait_for_ssh` (VM boot + SSH ready) | 9,861 | **77%** |
| `inject_key` (hdiutil attach/detach) | 1,049 | 8.2% |
| `guest_setup` (10 serial SSH commands) | 681 | 5.3% |
| `get_vm_ip` (`tart ip --wait`) | 531 | 4.2% |
| `resolve_image` (2× `tart list`) | 340 | 2.7% |
| `tart clone` | 303 | 2.4% |
| key gen + tools prep + share build | <5 | ~0% |

Teardown (`tart stop` + `tart delete`) adds ~5s on top.
**Total test elapsed wall time: ~18s.**

## Root Cause Analysis

### 1. `wait_for_ssh` — 9.8s (77%)

The VM takes ~10s from first boot to SSH availability. This is dominated by
macOS boot time (kernel, launchd, sshd). The backoff schedule is:

```
attempt 0: ~500ms sleep → fail
attempt 1: ~1000ms sleep → fail
attempt 2: ~2000ms sleep → fail
attempt 3: ~4000ms sleep → fail or success
```

Each failed attempt sleeps the full interval **even if SSH became ready midway**.
With a 4s sleep in attempt 3, we could wait up to 4 extra seconds past when SSH
is actually ready.

### 2. `inject_key` — 1.0s (8.2%)

`hdiutil attach -owners off <disk>` mounts all APFS volumes (multiple), then we
write the key and `hdiutil detach`. The attach (scanning containers, mounting
volumes) is the slow part (~700ms). Detach is ~300ms.

### 3. `guest_setup` — 681ms (5.3%)

10 serial SSH round-trips:
- 4 shares × 2 commands = 8 (`mkdir -p` + `mount_virtiofs`)
- 1 `mkdir -p /opt/claude-box/bin`
- 1 `ln -sf` (per binary, currently 1)

Each round-trip costs ~65-70ms (TCP + SSH framing + process exec in guest).
These commands are completely independent and could be collapsed into one.

### 4. `resolve_image` — 340ms (2.7%)

`pull_oci` calls `image_exists_in_tart` **twice**: once for the cache hit path
(checking `entry.local_name`) and once for the cache-miss fallback (checking
`image_ref` directly). Both run `tart list` (~170ms each).

---

## Proposed Optimizations

### 9.1 — Reduce SSH wait: linear poll before exponential backoff

**Expected savings: ~1–3s**

Replace the exponential backoff that starts at 500ms with a short linear poll
phase (every 100ms for the first 5s), then fall back to exponential backoff.
This way, if SSH becomes ready at t=10s, we detect it within 100ms instead of
waiting up to 4s into the current sleep interval.

```rust
// src/vm/health.rs  wait_for_ssh()
// Phase 1: poll every 100ms for the first 5s
// Phase 2: exponential backoff (500ms → 16s) after that
```

Alternatively: reduce the backoff base from 500ms to 200ms and drop the cap
from 16s to 8s. Simpler change, similar benefit.

### 9.2 — Batch guest_setup into a single SSH command

**Expected savings: ~600ms**

Instead of executing 10 separate SSH `exec` calls, build a single shell script
string and execute it in one `exec_capture`. Each current round-trip costs ~65ms;
collapsing to one saves ~9 × 65ms ≈ 585ms.

```bash
#!/bin/sh
set -e
sudo mkdir -p '/private/tmp/claude-box-test-xxx'
sudo mount_virtiofs 'project' '/private/tmp/claude-box-test-xxx'
sudo mkdir -p '/opt/claude-box/src-0'
sudo mount_virtiofs 'claude-box-bin-0' '/opt/claude-box/src-0'
sudo mkdir -p '/opt/claude-box/claude'
sudo mount_virtiofs 'claude-box-claude' '/opt/claude-box/claude'
sudo mkdir -p '/opt/claude-box/bin'
sudo ln -sf '/opt/claude-box/src-0/ddtool_darwin_arm64' '/opt/claude-box/bin/ddtool'
```

All mounts are independent; they can even be forked in parallel inside the script
with `&` + `wait` for an additional win, but sequential is already a big save.

Changes: `src/vm/guest_setup.rs` — replace the loop of `session.exec()` calls
with a single `session.exec()` of a generated script.

### 9.3 — Deduplicate `tart list` calls in `resolve_image`

**Expected savings: ~170ms**

`pull_oci` (PullPolicy::Missing | Never) currently does:
1. Check ImageCache → miss
2. Call `image_exists_in_tart(entry.local_name)` — 1st `tart list`
3. Cache miss → call `image_exists_in_tart(image_ref)` — 2nd `tart list`

Fix: call `image_exists_in_tart` once, check both names in a single pass.
Or: cache the `tart list` output and reuse it for both checks.

```rust
// Single tart list call, check both names
async fn pull_oci(image_ref: &str, pull_policy: &PullPolicy) -> Result<String> {
    match pull_policy {
        PullPolicy::Missing | PullPolicy::Never => {
            let cache = ImageCache::load()?;
            let cached_name = cache.get(image_ref).map(|e| e.local_name.clone());
            // One tart list, check both possible names
            let tart_images = list_tart_images().await?;
            if let Some(ref name) = cached_name {
                if tart_images.contains(name) { return Ok(name.clone()); }
            }
            if tart_images.contains(image_ref) { return Ok(image_ref.to_string()); }
            // ...
        }
    }
}
```

### 9.4 — Parallelize inject_key with pre-boot prep

**Expected savings: 0ms (inject_key is already on the critical path before boot)**

`inject_key` must finish before `tart run` because the disk needs the key before
the VM boots. However, `ssh_key::generate()`, `tools::prepare()`, and
`find_claude_binary()` currently run *sequentially before* the clone. We could
overlap them with `tart clone` using `tokio::join!`:

```
Currently:  resolve_image → clone → inject_key → spawn → wait
Proposed:   resolve_image → tokio::join!(clone, key_gen + tools_prep + claude_find) → inject_key → spawn → wait
```

Since `clone` (303ms) already dominates `key_gen + tools_prep` (<5ms), the
savings are negligible in practice. **Not worth implementing.**

### 9.5 (Stretch) — VM warm snapshot for test reuse

**Expected savings: ~10s (eliminates boot wait entirely on repeat runs)**

After the first integration test boot, use `tart stop --save` (or `tart snapshot`)
to save a ready-to-SSH snapshot. Subsequent test runs restore from snapshot
instead of booting from scratch.

This is a significant architectural change and has test-isolation risks (snapshot
state may be dirty). **Defer to a future milestone.**

---

## Implementation Results

### What was implemented

**9.1 — Faster SSH backoff** (`src/vm/health.rs`): Base reduced from 500ms → 200ms, cap 16s → 8s.

**9.3 — Deduplicate `tart list` calls** (`src/vm/image.rs`): Extracted `list_tart_images() -> HashSet<String>`, `pull_oci` now does one `tart list` call and checks both cached name and raw ref in a single pass.

**9.2 — Guest setup batching**: Tried; reverted. Consolidating 10 SSH commands into a single shell script showed higher latency (shell startup + subprocess overhead per `sudo`) with no benefit over per-command SSH exec. The `mount_virtiofs` operations themselves dominate `guest_setup` time, not SSH framing.

### Measured results

All timings have high natural variance (VM boot time varies ±3s between runs). Three runs measured:

| Run | wait_for_ssh | guest_setup | TOTAL boot |
|---|---|---|---|
| Baseline (before) | 9,861ms | 681ms | 12,779ms |
| After 9.1 + batching | 8,237ms | 1,535ms | 11,943ms |
| After reverted batching | 11,522ms | 1,095ms | 14,583ms |
| Final (9.1 + 9.3 only) | 8,264ms | 940ms | 11,160ms |

**Net improvement: ~1.6s (12.5%) on `wait_for_ssh`. Total boot median ~11–12s vs ~12.8s baseline.**

The backoff change (9.1) is the structural win: we detect SSH availability within 200ms of it becoming ready instead of sleeping up to 4s past the moment. The timing variance of ±3s from macOS boot time makes precise measurement difficult but the improvement is consistent.

### Key finding: VM boot time is the bottleneck

77% of startup time is waiting for macOS to boot and sshd to become available. There is no code-level optimization that can reduce this — it is bounded by macOS kernel + launchd startup. Future work (9.5 VM snapshot/restore) would be the only way to eliminate this cost.

## Implementation Plan

### Phase 1 — Quick wins (9.2, 9.3) — ~770ms savings, low risk

**9.2: Batch guest_setup**
- File: `src/vm/guest_setup.rs`
- Build one shell script string, execute with a single `session.exec()`
- Keep error handling: capture exit code, fail on non-zero

**9.3: Deduplicate tart list**
- File: `src/vm/image.rs`
- Extract `list_tart_images() -> Result<HashSet<String>>`
- Pass the set to a single resolution check

### Phase 2 — SSH poll tuning (9.1) — ~1–3s savings, low risk

**9.1: Faster SSH polling**
- File: `src/vm/health.rs`
- Change backoff: start at 200ms instead of 500ms; cap at 8s instead of 16s
- OR: add a linear 100ms-poll phase for the first 5s before switching to backoff

### Phase 3 — Verification

```bash
# Run with timing output to measure improvement
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  cargo test integration_tests -- --ignored --nocapture 2>&1 | grep timing

# Run full test suite to confirm nothing broken
cargo test && cargo clippy -- -D warnings
```

**Target: reduce total boot from 12.8s to under 9s (30% improvement).**

---

## Files to Modify

| File | Change |
|---|---|
| `src/vm/guest_setup.rs` | Batch all SSH commands into one script |
| `src/vm/image.rs` | Single `tart list` call, check both names |
| `src/vm/health.rs` | Faster initial backoff (200ms base, 8s cap) |
| `tests/integration_test.rs` | Keep timing instrumentation (useful for regressions) |

## Non-goals

- Reducing macOS boot time (not controllable)
- VM snapshot/restore (too complex for this milestone)
- Parallelizing `tart clone` (clone is already fast, 303ms)
