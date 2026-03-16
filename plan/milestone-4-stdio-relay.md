# Milestone 4 — Interactive Stdio Relay

## Goal

Upgrade the M2 non-interactive SSH exec to a full transparent relay with PTY
support, signal forwarding, and terminal resize sync. After this milestone,
interactive `claude-box` sessions (including TUI/streaming output) work
identically to running `claude` natively on the host.

---

## Background

Milestone 2 introduced `russh`-based SSH command execution, but without:
1. **PTY allocation** — needed for interactive/TUI claude sessions.
2. **stdin forwarding** — M2 exec doesn't connect host stdin to guest.
3. **Signal forwarding** — Ctrl-C on the host doesn't reach the guest process.
4. **Terminal resize** — SIGWINCH not relayed.

This milestone adds all four, replacing the M2 `SshSession::exec` with a
full `Relay::run` for the main claude execution path. The M2 non-interactive
exec remains available for guest setup commands (mount, config copy).

---

## Tasks

### 4.1 — PTY allocation

Detect whether the host's stdin is a TTY (`std::io::IsTerminal`). If so,
request a PTY on the SSH exec channel with the host terminal's current
dimensions.

Files touched:
- `src/relay.rs` — extend `SshSession` with `exec_with_pty`

### 4.2 — Async stdio forwarding

Three concurrent tokio tasks:

```
task A: tokio::io::stdin()  →  SSH channel stdin
task B: SSH channel stdout  →  tokio::io::stdout()
task C: SSH channel stderr  →  tokio::io::stderr()
```

When PTY is active, put host stdin into raw mode (disable line buffering,
echo). Restore on exit via a drop guard.

Task A handles EOF correctly (host stdin closed → send EOF on channel).

Files touched:
- `src/relay.rs` — `Relay::run(session, command) -> Result<i32>`

### 4.3 — Signal forwarding

Extend M2's signal handlers (which currently trigger VM cleanup only) to also
forward signals to the guest process:

- SIGINT → send `signal("INT")` on SSH channel.
- SIGTERM → send `signal("TERM")` on SSH channel.
- If guest doesn't exit within 5 s → `signal("KILL")`.
- After guest exits (or kill timeout) → proceed with VM cleanup from M2.9.

The signal handler chain is: forward to guest → wait → cleanup VM.

Files touched:
- `src/relay.rs` — signal forwarding task
- `src/sandbox.rs` — integrate relay signal handling with `VmGuard`

### 4.4 — Exit code propagation

Read exit status from SSH channel close event. Map:
- Normal exit → return exit code directly.
- Killed by signal → return 128 + signal number (POSIX convention).
- Channel error → return 1.

Files touched:
- `src/relay.rs`

### 4.5 — Wire relay into main exec path

Replace `SshSession::exec` in `exec_claude` with `Relay::run`. The non-interactive
`exec` is kept for guest setup commands (M2.6 mount operations).

Files touched:
- `src/sandbox.rs` — `exec_claude` calls `Relay::run`

### 4.6 — SIGWINCH / terminal resize

When the host terminal is resized, send a `window-change` request on the SSH
channel to keep the guest PTY in sync.

Files touched:
- `src/relay.rs` — SIGWINCH handler

---

## New dependencies

```toml
# russh/russh-keys already added in M2
# No new crates required
```

---

## Verification

```bash
# Interactive session — streaming output must render correctly
CLAUDE_BOX_IMAGE=... claude-box "what files are in this directory?"

# Ctrl-C must propagate and exit cleanly (exit code 130)
CLAUDE_BOX_IMAGE=... claude-box "run a long task"
# press Ctrl-C
echo $?   # expect 130

# Non-interactive (piped) must work without PTY
echo "list files" | CLAUDE_BOX_IMAGE=... claude-box -p

# Terminal resize: resize host terminal during a session
# Guest output should reflow correctly
```
