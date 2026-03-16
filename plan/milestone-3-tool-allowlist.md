# Milestone 3 — Tool Allowlist

## Goal

Two allowlist mechanisms are fully operational:
1. **MCP server filter** — only approved servers are available to `claude` inside the VM.
2. **Host binary forwarding** — named host binaries are staged and accessible inside the VM.

This milestone hardens the stubs in `tools.rs` into tested implementations.

---

## Design decisions (from review)

- **No `--deny-binary`**: the security model is explicit-allow only. No binaries
  are forwarded unless named with `--allow-binary`. There is no default
  pass-through to deny from.
- **No `--config` flag**: MCP config reaches the guest via VirtioFS share
  (set up in M2.6), copied to `~/.claude/settings.json` inside the guest.
  This is already wired in M2 — M3 hardens the filtering and staging logic.
- **Fail on missing binary**: if `--allow-binary foo` is given and `foo` is
  not found on the host PATH, this is a hard error, not a warning.

---

## Tasks

### 3.1 — Harden MCP config filtering

The M2 stub reads `~/.claude/settings.json` and filters `mcpServers`.
What's missing:

- Also check `~/.claude.json` as fallback (Claude Code uses both locations).
- Fix the temp file leak (`.keep()` in current code). Replace with a `TempDir`
  in the config staging dir whose lifetime is tied to `ToolEnv`.
- When `allow_tools` is empty, copy the full config unchanged (current behaviour
  is correct but add a test).
- Handle edge cases: no `mcpServers` key, empty `mcpServers`, malformed JSON.

Files touched:
- `src/tools.rs` — `prepare_mcp_config` hardening

### 3.2 — Harden host binary staging

The M2 stub copies binaries to a `TempDir`. What's missing:

- **Hard error on missing binary**: change from `warn` + skip to `anyhow::bail!`
  when a requested binary is not on PATH.
- **Dylib handling**: inspect Mach-O load commands with `otool -L`, copy
  required `.dylib`s into staging dir, set `DYLD_LIBRARY_PATH` in the guest
  environment.
- **Shell scripts**: detect and copy as-is (already executable).
- **Symlink resolution**: resolve symlinks before copying so the actual binary
  lands in staging (not a dangling link).

Files touched:
- `src/tools.rs` — `stage_binaries` hardening + `copy_dylibs()` helper

### 3.3 — Integration tests (no VM required)

Unit/integration tests for the allowlist logic:

- `prepare_mcp_config` with fixture `settings.json` files and various allow lists.
- `stage_binaries` with known host binaries (`/bin/sh`, `/usr/bin/env`).
- Edge cases: empty allow list, non-existent binary (expect error), binary
  without dylib deps.

Files touched:
- `tests/tools_test.rs` (new)

---

## New dependencies

```toml
# none — otool is part of Xcode CLT, called via tokio::process::Command
```

---

## Verification

```bash
cargo test

# Manual: verify MCP filtering
CLAUDE_BOX_IMAGE=... cargo run -- \
  --allow-tool filesystem \
  --allow-binary jq \
  -- sh -c 'cat ~/.claude/settings.json | jq .mcpServers && jq --version'

# Verify hard error on missing binary
CLAUDE_BOX_IMAGE=... cargo run -- \
  --allow-binary nonexistent_tool_xyz \
  -- echo hello
# expect: error message, no VM created
```
