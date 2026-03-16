# Lessons Learned

Patterns and corrections captured during development. Review at the start of each session.

---

## 1. `tart run` blocks — use `.spawn()` not `.status().await`

`tart run` does not return until the VM shuts down. Using `.status().await` on it will block the entire async runtime until the VM exits, making it impossible to interact with the VM over SSH. Always `.spawn()` the `tart run` process and hold the `Child` handle; call `.wait()` only after `tart stop` has been issued.

**Wrong:** `Command::new("tart").args(["run", ...]).status().await`
**Right:** `Command::new("tart").args(["run", ...]).spawn()` → hold `Child` → `.wait()` after stop

---

## 2. `tart exec` does not exist

The `tart` CLI has no `exec` subcommand. All guest interaction must go through SSH. Do not add a `Vm::exec` method to the `Vm` trait — it has no tart backend implementation.

---

## 3. `claude --config` is not a valid flag

Claude Code does not accept a `--config` flag. MCP configuration must reach the guest by another mechanism. The correct approach: write the filtered `settings.json` to a VirtioFS staging directory, mount it into the guest, and copy it to `~/.claude/settings.json` over SSH before invoking claude.

---

## 4. Binary named `claude-box`, not `claude`

The binary must be named `claude-box` (not `claude`) so that subcommands like `claude-box images list` are unambiguous and don't collide with claude's own argument namespace. A drop-in replacement approach (shadowing `claude` on PATH) was considered and rejected for this reason.

---

## 5. SSH crate consolidation — use `russh-keys` from M2 onward

The original plan used `ed25519-dalek` + `ssh-key` for key generation in M2 and `russh` only in M4. This creates two incompatible key type ecosystems. Use `russh-keys::key::KeyPair` for everything from M2 onward — it integrates directly with `russh`'s `authenticate_publickey` call.

---

## 6. MCP temp config must not leak via `.keep()`

Using `tmp.keep()` to persist a temp config file produces a `(File, PathBuf)` that drops the file handle but keeps the path — the file survives until the process exits but is not cleaned up on early errors. Correct approach: store the `TempDir` itself in `ToolEnv` (as `config_staging_dir: Option<TempDir>`). Its `Drop` implementation cleans up automatically even on error paths.

---

## 7. `dirs_next` crate is not available — use `std::env::var("HOME")`

The plan referenced `dirs_next::home_dir()` but the crate was not in scope. Use `std::env::var("HOME").ok().map(PathBuf::from)` to locate the home directory. Prefer this over adding new dependencies for simple path lookups.

---

## 8. IPSW import is idempotent — always check first

`tart create --from-ipsw` can take 10–30 minutes. Before running it, check whether the derived image name already exists in `tart list`. If it does, skip the import silently. This prevents accidental re-imports and makes repeated runs with the same IPSW fast.

---

## 9. Subagents keep context clean for parallel milestones

When multiple milestones touch disjoint files, launching them as parallel subagents is strictly better than sequential in-context implementation. The main context window stays small and the wall-clock time is dominated by the slowest milestone, not the sum. For this project M3 (tools.rs), M4 (relay.rs), and M5 (image.rs + cache + images subcommand) ran in parallel with no conflicts.

---

## 11. Zero-copy binary forwarding: mount source dir + guest symlink farm

Copying host binaries into a staging `TempDir` is unnecessary. Instead, resolve each binary to its canonical path (`which` + `canonicalize`), group binaries by parent directory, and mount each unique parent as a separate read-only VirtioFS share (`--dir=tag:path:ro`). In `guest_setup`, create a `ln -sf` for each named binary pointing from the mounted source directory into `/opt/claude-box/bin/`. This gives the guest PATH access to exactly the allowed set with no data movement.

**Why:** Avoids disk I/O and staging TempDir lifetime management; binaries in the VM always reflect the current host version; dylib staging becomes unnecessary (dylibs live alongside the binary in the same mounted directory).

**How to apply:** Use `resolve_binaries` (not `stage_binaries`) whenever binaries need forwarding. Remember that tart has a practical VirtioFS share limit (~16); warn if the number of unique binary directories exceeds 12.

---

## 10. `channel.make_writer()` for stdin forwarding (russh 0.46)

To forward host stdin to an SSH exec channel in russh 0.46, use `channel.make_writer()` which returns an `AsyncWrite`. Then `tokio::io::copy(&mut tokio::io::stdin(), &mut channel_writer)` in a spawned task handles the forwarding cleanly. Do not try to drive stdin and channel messages in the same `select!` loop — the borrow checker will reject it because both require `&mut channel`.

---

## 12. Async cleanup in Rust requires explicit guard pattern, not Drop

Rust's `Drop` trait cannot call async code. For resources that need async cleanup (like `tart stop` + `tart delete`), use an explicit `VmGuard` struct with an `async fn cleanup()` method. Separate the fallible work into a helper function, capture its `Result`, and call `guard.cleanup()` on the error path before propagating. Only `disarm()` the guard on the happy path.

**When Drop works:** For synchronous cleanup (like `hdiutil detach` which takes <200ms), a normal `Drop` impl with `std::process::Command` is fine.

---

## 13. Always create milestone plan files in `plan/`

Every milestone should have a corresponding `plan/milestone-N-<name>.md` file written before implementation starts. This provides a persistent record of decisions and scope for each milestone, independent of the conversation context.

---

## 14. Fleet-critical: guard every resource allocation with cleanup

At scale (1000+ engineers), any resource leak path that fires on 5% of runs becomes catastrophic. After any operation that creates a persistent resource (VM, disk mount, temp file), the very next line should establish a cleanup guard. Never rely on cleanup code at the end of a long function — any `?` between allocation and cleanup is a leak.

---

## 15. Use flock for cross-process file safety, not just in-process mutexes

When multiple CLI processes can run concurrently (common in CI), shared files like caches need advisory file locking (`flock`). Pair with atomic writes (temp file + rename) so readers never see partial content. `libc::flock` on macOS is sufficient — no need for the `fs2` crate.

---

## 16. VZF snapshot/restore is only reliable with exactly 1 VirtioFS share

Apple Virtualization Framework's `--suspendable` / `tart suspend` + resume is unreliable when 2 or more VirtioFS shares are configured. Testing on macOS Sequoia with tart 2.31.0: N=1 passes 10/10; N=2 is intermittent; N≥4 fails consistently with `VZErrorDomain Code=12 "invalid argument"`.

**Implication for warm pools:** The warm pool design must use exactly 1 VirtioFS slot (project directory). Any extra shares (binary allowlists, claude binary, MCP config) must be delivered by other means. For the claude binary: inject directly into the VM disk via hdiutil at warm VM creation time. For MCP config: transfer via SSH exec after resume. Binary allowlists (`--allow-binary`) are incompatible with the warm pool; disable warm pool when binary shares are needed.

**How to detect:** The failure looks like a Code=12 error in tart's stderr immediately after `restoring VM state from a snapshot…`, causing `tart run` to exit non-zero. The VM never gets an IP.

---

## 17. hdiutil detach "Resource busy" after copying large files — use -nobrowse and -force

When using `hdiutil attach` to write files to a guest disk image, macOS Spotlight (`mds`) can start indexing newly-written files before `hdiutil detach` completes, causing `hdiutil: couldn't unmount "diskN" - Resource busy` and a non-zero exit.

**Fix:** Use two flags together:
1. `hdiutil attach -owners off -nobrowse <image>` — `-nobrowse` prevents the volume from appearing in Finder/Spotlight, suppressing auto-indexing.
2. `hdiutil detach <dev> -force` — forces unmount even if the OS has a brief hold.

This issue only manifests when copying large files (e.g. the claude binary at ~40MB). Small writes (SSH keys, config files) are fast enough that Spotlight doesn't interfere.

**Also:** When a guest disk needs multiple injections (SSH key + binary), do them in a single `hdiutil attach/detach` cycle rather than two separate cycles. A second attach after a large write is more likely to hit timing races.

---

## 18. Integration test timing assertions must account for first-run warm VM creation

A timing assertion like `assert!(boot_ms < 20_000)` on `try_warm_boot` will fail on the first run because `try_warm_boot` includes warm VM creation (~25s) when the pool is cold. Subsequent runs (pool already seeded) take ~3-10s.

**Pattern:** Run `try_warm_boot` once to seed the pool (no timing assertion), teardown the result, then run it again and assert on that timing. This correctly tests warm pool performance independent of first-run creation latency.

---

## 19. Always update the README when adding user-visible features

When a milestone adds new user-visible behavior (CLI flags, authentication mechanisms, subcommands, config changes), update `README.md` in the same implementation pass. Don't wait for the user to ask — the README is the primary user-facing document and becomes stale immediately if not updated alongside the code. This includes: sequence diagrams, usage tables, security model, project layout descriptions, and the roadmap table.

---

## 20. Do not add Claude as a co-author or contributor

Do not include `Co-Authored-By: Claude` trailers in commit messages. The user does not want AI attribution in the git history.
