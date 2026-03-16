# Milestone 12 — Guest Environment Polish

## Problem

Authentication works (milestone 11), but the guest Claude experience has rough edges. Users report having to "accept a warning" during VM runs. The guest environment is missing state that the host Claude CLI expects, causing interactive prompts, permission dialogs, or warnings that break the transparent sandbox experience.

## Diagnostic Test

A new `e2e_claude_print` integration test (`tests/integration_test.rs`) exercises the full pipeline — boot VM, inject auth + settings, run `claude --print 'say hello world'` — and captures stdout/stderr. It classifies output into categories:

- `STDERR` — errors/warnings on stderr
- `PERMISSION_PROMPT` — permission acceptance dialogs
- `ONBOARDING` — first-run setup prompts
- `INTERACTIVE_PROMPT` — `[y/n]` or "press enter" prompts
- `NON_ZERO_EXIT` — non-zero exit code

Run it with:
```bash
CLAUDE_BOX_IMAGE=<image> cargo test e2e_claude_print -- --ignored --nocapture
```

Issues are also written to `/tmp/claude-box-e2e-issues.txt` for easy review.

## E2E Test Results (2026-03-13)

The `e2e_claude_print` test passes cleanly with **zero issues**:

```
[e2e] Exit code: 0
[e2e] Stdout: Hello World!
[e2e] Stderr: (empty)
[e2e] No issues detected.
```

Total time: ~13.5s (warm boot + setup + claude --print).

### Key finding: stdin must be closed for non-interactive mode

The first two test runs hung indefinitely because `exec_capture` doesn't close the SSH channel's stdin. Claude's `--print` mode still reads stdin (it supports piped input). When stdin never closes and never sends data, Claude blocks forever.

**Fix in the test:** `</dev/null` on the command line closes stdin immediately.

**Fix in production:** The PTY relay (`relay.rs`) handles this correctly — it forwards host stdin and EOF propagation. Only `exec_capture` (used in tests) has this issue, so this is test-only.

### Confirmed NOT issues

The following speculated issues from the original plan turned out to be non-issues:

1. **Permission prompts** — `--dangerously-skip-permissions` works, and the forwarded `settings.json` with permission grants is sufficient.
2. **Model/feature notices** — The 5-field `~/.claude.json` allowlist (`hasCompletedOnboarding`, `lastOnboardingVersion`, `hasShownOpus45Notice`, `hasShownOpus46Notice`, `showSpinnerTree`) is sufficient. No notices shown.
3. **Terminal capability warnings** — No warnings in stderr.
4. **Telemetry errors** — No errors; network egress through AppGate works fine.
5. **CLAUDE.md not accessible** — Project dir is mounted at the same absolute path; works correctly.
6. **Missing session dirs** — No errors about missing directories.

## Implemented Fixes

### 1. `--dangerously-skip-permissions` injected by default

`exec_claude` in `sandbox.rs` now always passes `--dangerously-skip-permissions` to the guest Claude. The VM is the sandbox — no need for a second permission layer inside it. This eliminates all tool permission prompts that could block non-interactive runs.

### 2. Broader `~/.claude.json` forwarding (pattern matching)

Replaced the static 5-key allowlist with prefix-based matching in `tools.rs`:
- Exact keys: `hasCompletedOnboarding`, `lastOnboardingVersion`, `showSpinnerTree`
- Prefix patterns: `hasShown*`, `hasCompleted*`

This automatically forwards new notice keys added by future Claude CLI versions without code changes.

### 3. Tool use verification test

Added a sub-test in `e2e_claude_print` that asks Claude to write a file using tools — verifies `--dangerously-skip-permissions` works end-to-end and Claude can use tools without hanging on permission prompts.

### 4. Suppress "Bypass Permissions mode" warning

Inject `skipDangerousModePermissionPrompt: true` into the staged `settings.json` in `prepare_claude_config()`. Since the VM is the sandbox, the bypass warning is noise — this silences it.

### 5. Forward `customApiKeyResponses` to suppress "custom API key" warning

Added `customApiKeyResponses` to `CLAUDE_JSON_FORWARD_KEYS` in `tools.rs`. This field contains fingerprints (not actual keys) of API keys the user has previously approved. Forwarding it suppresses the "Detected a custom API key in your environment" warning that appears when using `apiKeyHelper`-provided keys.

### 6. Pre-approve project trust dialog

Claude CLI shows a "Is this a project you created or one you trust?" prompt for every new workspace — `--dangerously-skip-permissions` does NOT bypass it (upstream bug [claude-code#28506](https://github.com/anthropics/claude-code/issues/28506)). Fixed by injecting `projects.<path>.hasTrustDialogAccepted: true` into the guest's `~/.claude.json` during config staging. The project path is now passed through `prepare()` → `prepare_claude_config()`.

### 7. Suppress "installMethod is native" warning

The Claude CLI checks that `~/.local/bin/claude` exists and is on PATH when it detects a "native" installation. Since we mount the binary from the host at `/opt/claude-box/claude/claude`, two fixes are applied:

- **Guest setup**: `ln -sf /opt/claude-box/claude/claude ~/.local/bin/claude` creates a symlink at the expected native location.
- **PATH**: `~/.local/bin` is always prepended to PATH in `exec_claude` so the CLI's self-check passes.
- **installMethod**: Set to `"system"` in the staged `~/.claude.json` as a belt-and-suspenders measure.

### 8. TUI warning capture test

Added sub-test 3 in `e2e_claude_print` that runs Claude in interactive TUI mode briefly using macOS `script` to record full PTY output, then pipes `/exit` to quit. This captures warnings rendered by the TUI that `--print` mode bypasses. The captured output is scanned for known warning patterns and saved to `/tmp/claude-box-tui-capture.txt` for review.

## Files Changed

| File | Change |
|------|--------|
| `src/sandbox.rs` | Always pass `--dangerously-skip-permissions` to guest Claude. Always prepend `~/.local/bin` to PATH. |
| `src/tools.rs` | `prepare()` now accepts `project_dir`. Pattern-match `hasShown*`/`hasCompleted*` keys. Forward `customApiKeyResponses`. Inject `skipDangerousModePermissionPrompt`, project trust entry, and `installMethod: "system"` into staged configs. |
| `src/vm/guest_setup.rs` | Create `~/.local/bin/claude` symlink during guest setup. |
| `tests/integration_test.rs` | Added `e2e_claude_print` test with `--print` diagnostics, tool use sub-test, and TUI `script` capture sub-test. |
| `README.md` | Document all guest polish fixes. |

## Verification

```bash
# Run the e2e diagnostic test
CLAUDE_BOX_IMAGE=<image> cargo test e2e_claude_print -- --ignored --nocapture

# Check for zero issues
cat /tmp/claude-box-e2e-issues.txt

# Full test suite still passes
cargo build && cargo clippy -- -D warnings && cargo test
```

## Success Criteria

`claude-box -- --print "hello"` produces clean output with no interactive prompts, warnings, or permission dialogs. The e2e test reports zero issues.
