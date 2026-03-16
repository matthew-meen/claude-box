# Milestone 8 — Integration Test Harness

## Context

claude-box has 7 unit tests (`tests/tools_test.rs`) covering MCP config filtering and binary resolution. There are **zero** tests that actually boot a VM. Regressions in the VM lifecycle (SSH key injection, VirtioFS mounts, guest setup, binary allowlist) go undetected until manual testing. This milestone adds a real integration test harness that launches a single shared VM and exercises the full pipeline.

## Prerequisites

- tart installed
- `CLAUDE_BOX_IMAGE` env var set (same production image)
- `ddtool` on host PATH
- Tests marked `#[ignore]` — run with `cargo test -- --ignored`

## Design: Shared VM

All integration tests share **one VM** booted once at the start. This avoids ~40s boot overhead per test. The VM is configured with the superset of what all tests need (e.g. `ddtool` allowed). Tests run sequentially against the shared session.

**Lifecycle:**
1. `#[ctor]` or `std::sync::OnceLock<TestVm>` lazily boots the VM on first test access
2. All `#[test] #[ignore]` functions call `shared_vm()` to get a `&TestVm`
3. After all tests complete, a cleanup function stops + deletes the VM

Since `cargo test` runs `#[ignore]` tests in a single thread by default when using `--test-threads=1` (and we recommend this), ordering is deterministic. Even with multiple threads, each test only reads from the shared VM — no mutation conflicts.

**Cleanup strategy:** Use `atexit` / `Drop` on a static to guarantee teardown. Alternatively, use `std::process::exit` hook or a wrapper test that boots, runs all sub-assertions, then tears down (most reliable pattern in Rust):

```rust
#[test]
#[ignore]
fn integration_tests() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let vm = TestVm::boot().await.unwrap();
        // Run all test assertions against the shared VM
        test_vm_boot_and_ssh(&vm).await;
        test_ddtool_version(&vm).await;
        test_binary_allowlist_enforcement(&vm).await;
        test_project_dir_mounted(&vm).await;
        test_project_dir_writable(&vm).await;
        test_exit_code_propagation(&vm).await;
        test_claude_binary_accessible(&vm).await;
        // Always teardown
        vm.teardown().await;
    });
}
```

This is the simplest approach — one `#[test]` function that runs all assertions sequentially, with guaranteed teardown at the end. Each sub-test is an `async fn` that panics on failure, and the VM is torn down in all cases (the `block_on` + teardown pattern). If a sub-test panics, we catch it and still teardown:

```rust
let result = std::panic::AssertUnwindSafe(async {
    test_vm_boot_and_ssh(&vm).await;
    test_ddtool_version(&vm).await;
    // ...
}).catch_unwind().await;
vm.teardown().await;
result.unwrap();
```

## Library Changes

### 8.1 — `exec_capture` on SshSession (`src/relay.rs`)

The existing `exec()` streams stdout/stderr to host stdio. Tests need to capture output. Add:

```rust
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl SshSession {
    pub async fn exec_capture(&self, command: &str) -> Result<ExecOutput>
}
```

Same event loop as `exec()` but accumulates `Data`/`ExtendedData` into `Vec<u8>` buffers instead of writing to stdio. Converts to `String` at the end.

### 8.2 — Expand `src/lib.rs` re-exports

Currently only `pub mod tools`. Tests (compiled as a separate crate) need access to:

```rust
pub mod config;
pub mod mount;
pub mod relay;
pub mod tools;
pub mod vm;
```

`gc`, `images`, `sandbox` remain private.

## Test Infrastructure

### 8.3 — TestVm + harness (`tests/integration_test.rs`)

**`TestVm`** struct:
- `vm_name: String` (uuid-based, `claude-box-test-{uuid}`)
- `session: SshSession`
- `vm: TartVm`
- `child: Child` (tart run process)
- `project_dir: TempDir`

**`TestVm::boot()`**:
1. Read `CLAUDE_BOX_IMAGE`, resolve with `PullPolicy::Never`
2. Generate ephemeral SSH key
3. Prepare tool env with `allow_binaries: ["ddtool"]`
4. Create tempdir as project dir, write `test-marker.txt` with known content
5. Build VirtioFS shares (project dir + binary shares + claude binary)
6. Clone VM, inject key, spawn `tart run`, wait for SSH
7. Run `guest_setup::setup_guest`

**`TestVm::teardown(self)`**:
- `tart stop`, wait child, `tart delete`

**Single entry point:**
```rust
#[test]
#[ignore]
fn integration_tests() { ... }
```

Calls each sub-test sequentially, catches panics, always tears down.

## Test Cases

### 8.4 — `test_vm_boot_and_ssh(vm)`
Run `echo hello` via `exec_capture`. Assert exit code 0, stdout contains "hello".
**Validates:** clone, key injection, boot, SSH connectivity.

### 8.5 — `test_ddtool_version(vm)`
Run `export PATH=/opt/claude-box/bin:$PATH && ddtool version`. Assert exit code 0, stdout non-empty.
**Validates:** binary resolution, VirtioFS mount, symlink farm, ddtool execution.

### 8.6 — `test_binary_allowlist_enforcement(vm)`
Run `ls /opt/claude-box/bin/`. Assert output contains `ddtool` and nothing unexpected.
**Validates:** symlink farm only exposes allowed binaries.

### 8.7 — `test_project_dir_mounted(vm)`
Run `cat <project_dir>/test-marker.txt`. Assert stdout matches known content written during boot.
**Validates:** VirtioFS project mount at correct guest path.

### 8.8 — `test_project_dir_writable(vm)`
Run `echo guest-wrote-this > <project_dir>/output.txt`. Check host filesystem for the file.
**Validates:** project mount is read-write.

### 8.9 — `test_exit_code_propagation(vm)`
Run `exit 42`. Assert exit code is 42.
**Validates:** exit code forwarding through SSH.

### 8.10 — `test_claude_binary_accessible(vm)`
Run `/opt/claude-box/claude/claude --version`. Assert exit code 0, stdout contains version.
**Validates:** claude binary mount works and is executable in guest.

### 8.11 (stretch) — `test_claude_runs_ddtool(vm)`
Only if `ANTHROPIC_API_KEY` is set. Run `claude --print "run ddtool version"`. Assert output contains ddtool version.
**Validates:** full end-to-end claude → tool execution inside VM.

## Files Modified

- `src/relay.rs` — add `ExecOutput` + `exec_capture()`
- `src/lib.rs` — expand public module exports
- `tests/integration_test.rs` — new: TestVm harness + all test cases
- `plan/milestone-8-integration-tests.md` — archived plan

## Execution Order

1. `exec_capture` + lib.rs exports (8.1, 8.2)
2. TestVm harness + smoke test (8.3, 8.4)
3. Remaining tests (8.5–8.11)

## Running

```bash
# Run integration tests (single shared VM)
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  cargo test integration_tests -- --ignored

# With stretch test
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
ANTHROPIC_API_KEY=sk-... \
  cargo test integration_tests -- --ignored
```

## Verification

```bash
cargo build && cargo clippy -- -D warnings && cargo test  # unit tests still pass
cargo test integration_tests -- --ignored                  # integration tests pass
```
