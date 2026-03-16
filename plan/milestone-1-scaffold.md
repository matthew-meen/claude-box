# Milestone 1 — Scaffold (complete)

## Goal
Establish the full project skeleton with stubs for every module so `cargo build`
and `cargo clippy` pass clean. No VM actually starts in this milestone.

## Files Created

| File | Purpose |
|------|---------|
| `Cargo.toml` | Binary crate (`claude-box`), all dependencies |
| `src/main.rs` | CLI entry: clap args, build SandboxConfig, call Sandbox::run() |
| `src/config.rs` | SandboxConfig struct |
| `src/vm/mod.rs` | Vm trait + VmConfig |
| `src/vm/tart.rs` | TartVm — tart CLI wrapper stubs |
| `src/vm/image.rs` | resolve_image(): IPSW → OCI pull fallback |
| `src/mount.rs` | DirShare + build_shares() for VirtioFS `--dir` flags |
| `src/tools.rs` | MCP allowlist filter + host binary staging |
| `src/sandbox.rs` | Sandbox::run() lifecycle orchestration |
| `src/relay.rs` | Relay stub |

## Status

- [x] All files created
- [x] `cargo build` passing
- [x] `cargo clippy -- -D warnings` clean

## Retrofitted in M7

- **`install.sh`**: Build + install script added. Builds release binary, copies to `~/.local/bin/`, checks for tart/claude/PATH/CLAUDE_BOX_IMAGE. Should have been part of the original scaffold.

## Post-review corrections (to apply alongside M2)

- **Binary name**: rename from `claude` to `claude-box` in Cargo.toml and CLI
- **Remove `tart exec`**: `TartVm::exec` is not a real tart command — remove it
- **Remove `--config` flag**: claude doesn't accept `--config` — MCP config goes
  to `~/.claude/settings.json` inside the guest via VirtioFS share
- **Fix `tart run` blocking**: `start_vm_with_shares` must `.spawn()` not `.status()`
