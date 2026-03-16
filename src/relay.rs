//! SSH relay between host and guest VM.
//!
//! `SshSession` provides non-interactive command execution (M2).
//! PTY/signal/interactive relay is added in Milestone 4.

use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle};
use russh_keys::key::{KeyPair, PublicKey};
use std::sync::Arc;
use tracing::debug;

/// Captured output from a non-interactive SSH command.
/// Used by integration tests via the library crate.
#[allow(dead_code)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

struct ClientHandler;

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept any host key — VMs are ephemeral and keys change every run
        Ok(true)
    }
}

/// A connected SSH session to the guest VM.
pub struct SshSession {
    handle: Handle<ClientHandler>,
}

impl SshSession {
    /// Connect to the VM at `ip:22` authenticating with the given keypair.
    pub async fn connect(ip: &str, key: &KeyPair) -> Result<Self> {
        let config = Arc::new(client::Config::default());
        let mut handle = client::connect(config, (ip, 22u16), ClientHandler)
            .await
            .with_context(|| format!("SSH connect to {ip}:22"))?;

        let authenticated = handle
            .authenticate_publickey("admin", Arc::new(key.clone()))
            .await
            .context("SSH authenticate_publickey")?;
        anyhow::ensure!(
            authenticated,
            "SSH public key authentication rejected by {ip}"
        );

        Ok(Self { handle })
    }

    /// Execute a command and capture stdout/stderr instead of streaming.
    /// Returns the exit code plus captured output. No PTY, no stdin.
    #[allow(dead_code)]
    pub async fn exec_capture(&self, command: &str) -> Result<ExecOutput> {
        debug!("ssh exec_capture: {command}");
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context("open SSH channel")?;
        channel
            .exec(true, command)
            .await
            .context("exec command on SSH channel")?;

        let mut exit_code = 1i32;
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut got_eof = false;
        let mut got_exit = false;
        loop {
            let Some(msg) = channel.wait().await else {
                break;
            };
            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    stdout_buf.extend_from_slice(data);
                }
                russh::ChannelMsg::ExtendedData { ref data, .. } => {
                    stderr_buf.extend_from_slice(data);
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = exit_status as i32;
                    got_exit = true;
                    if got_eof {
                        break;
                    }
                }
                // OpenSSH may send Eof before or after ExitStatus.
                // Only break when we have both, to avoid missing the exit code.
                russh::ChannelMsg::Eof => {
                    got_eof = true;
                    if got_exit {
                        break;
                    }
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        })
    }

    /// Execute a command in the guest. stdout/stderr are streamed to the
    /// host's stdout/stderr. Returns the remote exit code.
    /// No PTY, no stdin — use for setup commands only.
    pub async fn exec(&self, command: &str) -> Result<i32> {
        debug!("ssh exec: {command}");
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context("open SSH channel")?;
        channel
            .exec(true, command)
            .await
            .context("exec command on SSH channel")?;

        let mut exit_code = 1i32;
        let mut got_eof = false;
        let mut got_exit = false;
        loop {
            let Some(msg) = channel.wait().await else {
                break;
            };
            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    use std::io::Write;
                    std::io::stdout().write_all(data)?;
                    std::io::stdout().flush()?;
                }
                russh::ChannelMsg::ExtendedData { ref data, .. } => {
                    use std::io::Write;
                    std::io::stderr().write_all(data)?;
                    std::io::stderr().flush()?;
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = exit_status as i32;
                    got_exit = true;
                    if got_eof {
                        break;
                    }
                }
                russh::ChannelMsg::Eof => {
                    got_eof = true;
                    if got_exit {
                        break;
                    }
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(exit_code)
    }
}

// ── Terminal helpers (TTY mode, size) ────────────────────────────────────────

/// Return the current terminal size as `(cols, rows)`.
/// Falls back to 80×24 if the ioctl fails or returns zeroes.
fn terminal_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ is a read-only ioctl on an fd we own.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// RAII guard that puts the terminal into raw mode on construction and
/// restores the original settings on drop.
struct RawModeGuard {
    saved: libc::termios,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        // SAFETY: We pass valid pointers and check the return value.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                anyhow::bail!("tcgetattr failed");
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                anyhow::bail!("tcsetattr failed");
            }
            Ok(Self { saved })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: restoring a previously-read termios struct.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

// ── Signal name → number mapping ─────────────────────────────────────────────

fn sig_to_num(sig: &russh::Sig) -> i32 {
    match sig {
        russh::Sig::HUP => 1,
        russh::Sig::INT => 2,
        russh::Sig::QUIT => 3,
        russh::Sig::ILL => 4,
        russh::Sig::ABRT => 6,
        russh::Sig::FPE => 8,
        russh::Sig::KILL => 9,
        russh::Sig::SEGV => 11,
        russh::Sig::PIPE => 13,
        russh::Sig::ALRM => 14,
        russh::Sig::TERM => 15,
        russh::Sig::USR1 => 10,
        russh::Sig::Custom(_) => 0,
    }
}

// ── Relay ─────────────────────────────────────────────────────────────────────

/// Full interactive relay (Milestone 4).
pub struct Relay;

impl Relay {
    /// Run `command` interactively via `session`.
    ///
    /// - If stdin is a TTY: allocate a PTY, enter raw mode, relay SIGWINCH.
    /// - Always: forward stdin data, stream stdout/stderr, relay SIGINT/SIGTERM.
    ///
    /// Returns the exit code. Signal-killed exits return `128 + signal_number`.
    pub async fn run(session: &SshSession, command: &str) -> Result<i32> {
        use std::io::IsTerminal;
        use tokio::io::AsyncWriteExt;

        let is_tty = std::io::stdin().is_terminal();
        debug!("relay run (tty={is_tty}): {command}");

        let mut channel = session
            .handle
            .channel_open_session()
            .await
            .context("open SSH channel")?;

        if is_tty {
            let (cols, rows) = terminal_size();
            channel
                .request_pty(
                    false,
                    &std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
                    cols as u32,
                    rows as u32,
                    0,
                    0,
                    &[],
                )
                .await
                .context("request PTY")?;
        }

        channel
            .exec(true, command)
            .await
            .context("exec command on SSH channel")?;

        // Enter raw mode after exec so the guest owns the TTY.
        let _raw_guard = if is_tty {
            Some(RawModeGuard::enter()?)
        } else {
            None
        };

        // ── Signal handlers ───────────────────────────────────────────────────
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("install SIGINT handler")?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("install SIGTERM handler")?;
        let mut sigwinch_stream = if is_tty {
            Some(
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                    .context("install SIGWINCH handler")?,
            )
        } else {
            None
        };

        // ── Stdin forwarding task ─────────────────────────────────────────────
        // We use channel.make_writer() to get an AsyncWrite that maps writes
        // into SSH channel data messages.
        let mut channel_writer = channel.make_writer();
        let stdin_task = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let _ = tokio::io::copy(&mut stdin, &mut channel_writer).await;
            // Flush on EOF — ignore errors (channel may be gone).
            let _ = channel_writer.flush().await;
        });

        // ── Main event loop ───────────────────────────────────────────────────
        let mut exit_code = 1i32;

        loop {
            tokio::select! {
                // Channel messages from the remote side.
                msg = channel.wait() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        russh::ChannelMsg::Data { ref data } => {
                            use std::io::Write;
                            std::io::stdout().write_all(data)?;
                            std::io::stdout().flush()?;
                        }
                        russh::ChannelMsg::ExtendedData { ref data, .. } => {
                            use std::io::Write;
                            std::io::stderr().write_all(data)?;
                            std::io::stderr().flush()?;
                        }
                        russh::ChannelMsg::ExitStatus { exit_status } => {
                            exit_code = exit_status as i32;
                        }
                        russh::ChannelMsg::ExitSignal { signal_name, .. } => {
                            let num = sig_to_num(&signal_name);
                            exit_code = if num > 0 { 128 + num } else { 1 };
                        }
                        russh::ChannelMsg::Close | russh::ChannelMsg::Eof => break,
                        _ => {}
                    }
                }

                // SIGINT → forward to remote process.
                _ = sigint.recv() => {
                    debug!("forwarding SIGINT to remote");
                    if let Err(e) = channel.signal(russh::Sig::INT).await {
                        debug!("signal send failed: {e}");
                    }
                }

                // SIGTERM → forward to remote process.
                _ = sigterm.recv() => {
                    debug!("forwarding SIGTERM to remote");
                    if let Err(e) = channel.signal(russh::Sig::TERM).await {
                        debug!("signal send failed: {e}");
                    }
                }

                // SIGWINCH → relay new terminal size.
                Some(_) = async {
                    match sigwinch_stream.as_mut() {
                        Some(s) => s.recv().await.map(|_| ()),
                        None => None,
                    }
                } => {
                    let (cols, rows) = terminal_size();
                    debug!("SIGWINCH: {cols}x{rows}");
                    if let Err(e) = channel.window_change(cols as u32, rows as u32, 0, 0).await {
                        debug!("window_change failed: {e}");
                    }
                }
            }
        }

        // Clean up the stdin forwarding task — it will naturally stop when the
        // channel writer is dropped or stdin reaches EOF.
        stdin_task.abort();

        Ok(exit_code)
    }
}
