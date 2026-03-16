# claude-box — Build Cost Analysis

## Timeline

Built over 2 days: March 12–13, 2026.

- **Day 1 (Mar 12):** Milestones 1–10 — scaffold through warm pool
- **Day 2 (Mar 13):** Milestones 11–12 — authentication, guest polish

## Codebase Size

| Category | Files | Lines |
|----------|------:|------:|
| Rust source (`src/`) | 17 | 3,552 |
| Integration tests (`tests/`) | 2 | 1,310 |
| **Total Rust** | **19** | **4,862** |
| Milestone plans (`plan/`) | 13 | 2,007 |
| README | 1 | 252 |
| Install script | 1 | 51 |
| **Total project** | **34** | **7,172** |

### Source breakdown by module

| File | Lines | Responsibility |
|------|------:|----------------|
| `sandbox.rs` | 535 | Full lifecycle orchestration, warm/cold paths, exec |
| `warm_pool.rs` | 560 | Pre-suspended VM pool, snapshot/resume |
| `relay.rs` | 394 | PTY/stdin/stdout/stderr/signal relay over SSH |
| `tools.rs` | 346 | Config staging, MCP filter, binary resolution |
| `ssh_key.rs` | 316 | Ephemeral Ed25519 keypair, hdiutil disk injection |
| `main.rs` | 270 | CLI entry point, subcommand dispatch |
| `image.rs` | 235 | OCI pull, IPSW import, pull policy |
| `image_cache.rs` | 149 | Local image cache manifest with flock |
| `gc.rs` | 144 | Orphan VM reaper, `gc` subcommand |
| `images.rs` | 128 | `images` subcommand handlers |
| `guest_setup.rs` | 119 | VirtioFS mount, config copy, symlink farm |
| `mount.rs` | 106 | VirtioFS share builder |
| `tart.rs` | 87 | tart CLI wrapper |
| `health.rs` | 78 | VM IP resolution, SSH readiness |
| `config.rs` | 42 | SandboxConfig struct |
| `mod.rs` | 38 | Vm trait, VmConfig |
| `lib.rs` | 5 | Re-exports |

## Dependencies

13 crates, all well-established:

| Crate | Purpose |
|-------|---------|
| `clap` 4 | CLI framework (derive + env) |
| `tokio` 1 | Async runtime |
| `russh` + `russh-keys` 0.46 | Pure-Rust SSH client |
| `anyhow` 1 | Error handling |
| `tracing` + `tracing-subscriber` | Structured logging |
| `serde` + `serde_json` | JSON serialization |
| `uuid` 1 | VM name generation |
| `which` 6 | Host binary path resolution |
| `tempfile` 3 | Config staging dirs |
| `libc` 0.2 | Signal handling (SIGWINCH) |
| `indicatif` 0.17 | Spinner/progress UI |

No custom forks. No nightly features. No unsafe code (except libc FFI for signals).

## Milestones

| # | Milestone | Day | Key Deliverable |
|---|-----------|-----|-----------------|
| 1 | Scaffold | 1 | Project structure, CLI flags, module stubs |
| 2 | VM Lifecycle | 1 | tart clone/start/stop/delete, SSH key injection, VirtioFS |
| 3 | Tool Allowlist | 1 | MCP config injection + filtering, binary staging |
| 4 | Stdio Relay | 1 | Full PTY relay, SIGINT/SIGTERM/SIGWINCH |
| 5 | Image Management | 1 | OCI pull, IPSW import, image cache, `images` subcommand |
| 6 | Zero-copy Binaries | 1 | Read-only VirtioFS mounts, guest symlink farm |
| 7 | Operational Hardening | 1 | VmGuard, orphan reaper, MountGuard, cache locking, timeouts |
| 8 | Integration Tests | 1 | Shared VM harness, exec_capture, full pipeline tests |
| 9 | Performance | 1 | SSH backoff tuning, deduplicated tart list; ~11s cold boot |
| 10 | Warm Pool | 1 | Pre-suspended VM per image; ~5-10s warm boot |
| 11 | Authentication | 2 | apiKeyHelper, ANTHROPIC_API_KEY, Keychain OAuth forwarding |
| 12 | Guest Polish | 2 | Trust prompt, installMethod, bypass-mode, API key warnings |

## What was built

A production-grade macOS sandbox for Claude Code that:

- Boots an ephemeral tart VM per session (~5s warm, ~11s cold)
- Mounts the project directory read-write via VirtioFS (same absolute path)
- Mounts the host Claude binary read-only (no guest install needed)
- Forwards host binaries zero-copy with symlink-farm allowlist enforcement
- Relays PTY, stdin/stdout/stderr, and Unix signals transparently
- Authenticates via 3-source chain (apiKeyHelper, env var, Keychain)
- Forwards host settings, onboarding state, and project trust
- Suppresses all spurious TUI warnings for a clean guest experience
- Cleans up VMs on exit, crash, SIGKILL, or host reboot
- Manages base images (pull, cache, prune)
- Maintains a warm pool for fast startup
- Falls back to host claude if the sandbox fails

## Actual API Cost (from conversation logs)

Extracted from the JSONL conversation logs with real token counts and Anthropic pricing.

### Token totals

| Metric | Tokens |
|--------|-------:|
| Output tokens | 724,078 |
| Cache creation (input) | 9,965,126 |
| Cache read (input) | 179,633,223 |
| Uncached input | 3,787 |
| **Total input (all categories)** | **189,602,136** |

### Cost by model

| Model | Turns | Output cost | Cache create cost | Cache read cost | Total |
|-------|------:|------------:|------------------:|----------------:|------:|
| Sonnet 4.6 | 1,209 | $7.84 | $22.45 | $32.56 | **$62.85** |
| Opus 4.6 | 849 | $15.23 | $74.69 | $107.07 | **$197.02** |
| **Total** | **2,058** | **$23.07** | **$97.14** | **$139.63** | **$259.87** |

### Cost by phase

| Time block | Milestones | Turns | Cost | Key work |
|------------|------------|------:|-----:|----------|
| Mar 12 12:00–13:00 | 1 | 39 | $1.10 | Scaffold, plan review |
| Mar 12 13:00–15:00 | 1–7 | 430 | $51.46 | Plan review (Opus), implement M2–M7 |
| Mar 12 15:00–16:00 | 8 | 20 | $0.81 | Test harness planning |
| Mar 12 16:00–18:00 | 8 | 393 | $46.38 | Integration tests, fix failures |
| Mar 12 18:00–20:00 | 9–10 | 324 | $34.82 | Performance tuning, warm pool POC |
| Mar 12 20:00–21:00 | 10 | 124 | $9.33 | Warm pool completion |
| Mar 13 12:00–15:00 | 10–11 | 59 | $7.12 | Warm pool fixes, spinner UX |
| Mar 13 15:00–17:00 | 11 | 357 | $48.01 | Authentication (OAuth, Keychain) |
| Mar 13 17:00–19:00 | 12 | 312 | $60.84 | Guest polish, TUI warnings, e2e tests |

### Most expensive prompts

The 10 highest-cost individual prompts, showing what each one delivered:

| # | Cost | Turns | Prompt | Value delivered |
|---|-----:|------:|--------|-----------------|
| 1 | $36.83 | 192 | "now we're planning a new milestone - I want a full test harness..." | M8: full integration test framework with shared VM, 5 sub-tests |
| 2 | $30.74 | 89 | "There's an edge-case where the delete command might not run..." | M7: VmGuard, MountGuard, orphan reaper, graceful fallback |
| 3 | $16.79 | 68 | "review milestone 11. It doesn't work right now..." | M11: fixed full settings forwarding, apiKeyHelper extraction |
| 4 | $15.76 | 34 | "review all plans, look for ambiguities and conflicts" | Caught 8 conflicts: blocking tart run, nonexistent tart exec, SSH crate mismatch, etc. |
| 5 | $9.90 | 232 | "run the integration test, document what doesn't work" | M8–M9: ran tests, identified SSH backoff issues, fixed 4 failures |
| 6 | $9.39 | 71 | "the error is still there - find a way to capture TUI errors" | M12: `script`-based TUI capture, found trust prompt + installMethod warning |
| 7 | $9.33 | 154 | (context continuation) | M10: warm pool snapshot/resume, nslot management |
| 8 | $8.50 | 30 | "add support so that if a user is using OAuth..." | M11: Keychain OAuth token extraction, 3-source auth chain |
| 9 | $7.98 | 31 | (milestone 12 review) | M12: implement remaining fixes, run e2e verification |
| 10 | $7.64 | 135 | (warm pool implementation) | M10: pre-suspended VM pool, clone-from-snapshot, stale VM refresh |

### Cost distribution

| Bucket | Prompts | Total cost | % of total |
|--------|--------:|----------:|----------:|
| > $10 | 4 | $94.23 | 36% |
| $5 – $10 | 6 | $47.68 | 18% |
| $1 – $5 | 18 | $53.72 | 21% |
| $0.10 – $1 | 24 | $13.17 | 5% |
| < $0.10 | 74 | $0.73 | 0% |
| **Total** | **126** | **$259.87** | **100%** |

75% of cost came from 28 substantive prompts. 74 prompts (59%) were near-zero cost (model switches, compactions, file opens, task notifications).

### Cost per line of code

| Metric | Value |
|--------|------:|
| Total Rust code | 4,862 lines |
| Total API cost | $259.87 |
| **Cost per line** | **$0.053** |
| Cost per source line (excl. tests) | $0.073 |

## Human time

126 user prompts over 2 days. Most were short directives or confirmations:

- Day 1: ~30 prompts with substance, ~2h active attention
- Day 2: ~20 prompts with substance, ~1.5h active attention
- **Estimated human time: ~3.5 hours**

## Comparable manual effort

Building this from scratch manually (senior Rust engineer, familiar with tart/SSH/macOS VMs):

- Core sandbox (M1–M4): 3–4 days
- Image management + binaries (M5–M6): 2 days
- Hardening + tests (M7–M8): 2 days
- Performance + warm pool (M9–M10): 3 days
- Auth + polish (M11–M12): 2 days
- **Estimated manual build: 12–15 engineer-days**

### Cost comparison

| Approach | Calendar time | Engineer time | API cost | Total cost |
|----------|:------------:|:-------------:|:--------:|:----------:|
| Claude Code (actual) | 2 days | ~3.5h | $259.87 | ~$785 |
| Manual (estimate) | 3 weeks | 12–15 days | $0 | ~$15,600 |

At $150/hr: 3.5h human oversight = ~$525, plus $260 API = **~$785 total**.
Manual build: 13 days × 8h × $150/hr = **~$15,600**.

**~20x cost reduction**, with delivery in 2 days instead of 3 weeks.

## Quality notes

- Zero clippy warnings (`-D warnings`)
- 7 unit tests + 5 integration tests (4 VM-booting, 1 e2e with 3 sub-tests)
- 2,007 lines of milestone documentation with design rationale
- 140-line lessons file capturing mistakes and corrections
- All error paths have cleanup guards (VmGuard, MountGuard)
- Graceful fallback to host claude on any sandbox failure
