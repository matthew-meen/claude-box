# Milestone 2 — VM Lifecycle + SSH

## Goal

A real VM boots, is reachable over SSH, has VirtioFS shares mounted at the
correct paths, and can execute commands. By the end, `claude-box` can clone a
tart image, boot it, SSH in, run a command, and cleanly tear down.

This milestone also introduces SSH (via `russh`) as the sole guest-interaction
mechanism — there is no `tart exec`.

---

## Design decisions (from review)

- **SSH from M2**: `russh` + `russh-keys` are added now, not deferred to M4.
  M4 becomes PTY allocation, signal forwarding, and SIGWINCH only.
- **Ephemeral key pre-inject**: generate Ed25519 keypair, mount cloned VM
  disk with `hdiutil`, write public key to `authorized_keys` before first boot.
- **`tart run` is blocking**: must be `.spawn()`ed in background. Hold `Child`
  handle and kill it during teardown.
- **No `tart exec`**: remove `Vm::exec` from the trait. All guest commands via SSH.
- **Binary rename**: `claude` → `claude-box`.
- **Host claude via VirtioFS**: mount the host's `claude` binary into the guest
  as share `claude-box-claude`, no guest install needed.
- **MCP config via VirtioFS**: write filtered `settings.json` to staging dir,
  mount as `claude-box-config` share, copy into guest `~/.claude/settings.json`
  over SSH. No `--config` flag.

---

## Tasks

### 2.1 — Rename binary to `claude-box`

Update `Cargo.toml` `[[bin]]` name, CLI `#[command(name = ...)]`, README.

Files touched:
- `Cargo.toml`, `src/main.rs`, `README.md`

### 2.2 — SSH keypair generation

Generate ephemeral Ed25519 keypair using `russh-keys`. Private key in memory
only. Public key serialized in OpenSSH format.

Files touched:
- `src/vm/ssh_key.rs` (new)

### 2.3 — Key injection into cloned VM disk

After `tart clone`, mount VM disk and write public key:

1. Locate disk at `~/.tart/vms/<name>/disk.img`.
2. `hdiutil attach -mountpoint /tmp/claude-box-mount-<uuid> disk.img`.
3. Write key to `<mountpoint>/Users/admin/.ssh/authorized_keys`.
4. `hdiutil detach`.

Files touched:
- `src/vm/ssh_key.rs` — `inject_key(vm_name, pubkey)`
- `src/vm/tart.rs` — `vm_disk_path(name)` helper

### 2.4 — Spawn `tart run` in background

`.spawn()` instead of `.status()`. Return `Child` handle stored in `Sandbox`.
Pass `--dir` flags for all VirtioFS shares:
- `project:<project_dir>` — user's working directory
- `claude-box-bin:<staging_dir>` — host binaries (if any)
- `claude-box-config:<config_dir>` — filtered MCP settings (if any)
- `claude-box-claude:<claude_bin_dir>` — host's `claude` binary

Files touched:
- `src/sandbox.rs` — `start_vm_with_shares` returns `Child`
- `src/mount.rs` — add claude binary share to `build_shares()`

### 2.5 — Wait-for-SSH health loop

1. `tart ip <name> --wait 30` to get VM IP.
2. Attempt `russh` connection with ephemeral key, retry every 2 s.
3. Timeout after 120 s.

Files touched:
- `src/vm/health.rs` (new)
- `src/sandbox.rs` — call after spawn

### 2.6 — Guest-side VirtioFS mount + setup

Over SSH after health check passes:

```bash
# Mount project at original path
sudo mkdir -p /Users/matt/src/myproject
sudo mount_virtiofs project /Users/matt/src/myproject

# Mount tool shares
sudo mkdir -p /opt/claude-box/bin
sudo mount_virtiofs claude-box-bin /opt/claude-box/bin

# Mount config share + copy to expected location
sudo mkdir -p /opt/claude-box/config
sudo mount_virtiofs claude-box-config /opt/claude-box/config
cp /opt/claude-box/config/settings.json ~/.claude/settings.json

# Mount host claude binary
sudo mkdir -p /opt/claude-box/claude
sudo mount_virtiofs claude-box-claude /opt/claude-box/claude
```

Files touched:
- `src/vm/guest_setup.rs` (new)
- `src/sandbox.rs` — call after health check

### 2.7 — SSH command execution (non-interactive)

Basic `SshSession::exec(command) -> Result<i32>`. No PTY, no stdin forwarding.
stdout/stderr captured and printed to host. Sufficient for `echo hello` but
not interactive claude sessions (that's M4).

Files touched:
- `src/relay.rs` — `SshSession`, basic `exec`
- `src/sandbox.rs` — `exec_claude` uses SSH

### 2.8 — Remove `Vm::exec` and fix `--config`

- Remove `exec` from `Vm` trait and `TartVm`.
- Remove `--config` flag from `exec_claude` command builder.
- Claude is invoked as `/opt/claude-box/claude/claude <args>`.

Files touched:
- `src/vm/mod.rs`, `src/vm/tart.rs`, `src/sandbox.rs`

### 2.9 — Cleanup guard + signal handling

`VmGuard` struct that on drop:
1. Kills `tart run` child process.
2. Runs `tart stop <name>` then `tart delete <name>` (unless `--persist`).

SIGINT/SIGTERM trigger the same teardown via `tokio::signal`.
M4 will extend these handlers to also forward signals to the guest.

Files touched:
- `src/sandbox.rs` — `VmGuard`

---

## New files

| File | Purpose |
|------|---------|
| `src/vm/ssh_key.rs` | Ed25519 keypair generation + disk injection |
| `src/vm/health.rs` | SSH connection retry loop |
| `src/vm/guest_setup.rs` | VirtioFS mount + config copy via SSH |

## New dependencies

```toml
russh = "0.46"               # async SSH client
russh-keys = "0.46"          # Ed25519 keypair + OpenSSH format
```

---

## Verification

```bash
# Boot VM, SSH in, run command, tear down
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  cargo run -- --vm-name test-m2 --persist echo hello
# expect: "hello" on stdout, VM still running

tart list | grep test-m2     # should exist
tart stop test-m2 && tart delete test-m2

# Without --persist: auto-cleanup
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  cargo run -- echo hello
tart list                    # no claude-box-* entries
```
