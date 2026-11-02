//! Interactive sessions in a Sandbox: a TTY exec through the lock-free `ExecTarget`, driven from
//! boxq's own terminal (jonaswre-labs/boxlite-quick#15).
//!
//! The terminal handling is adapted from boxlite-cli 0.11.0's `terminal/mod.rs` (Apache-2.0). It
//! drives the raw `ExecComponents` channels, because `Execution`'s constructor is `pub(crate)`.

use crate::Workspace;
use crate::sandbox::{ExecTarget, OutputChunks};
use anyhow::Context;
use boxlite::BoxCommand;
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};
use std::io::Read;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

/// The Agent, with every permission prompt off: the Sandbox is the boundary.
pub const CLAUDE: &[&str] = &["claude", "--dangerously-skip-permissions"];

/// Root's own login shell, falling back to `sh` where the image names none.
pub const LOGIN_SHELL: &[&str] = &[
    "sh",
    "-c",
    r#"shell=$(getent passwd root | cut -d: -f7); exec "${shell:-/bin/sh}" -l"#,
];

/// Runs `argv` as root on a TTY in the Workspace's directory, wired to boxq's own terminal until
/// it exits, and returns its exit status as a shell reports it. The Sandbox keeps running.
pub async fn interactive(
    target: &ExecTarget,
    ws: &Workspace,
    argv: &[&str],
) -> anyhow::Result<i32> {
    let command = command(target, ws, argv).tty(true);
    // Caught from before the exec starts, so no signal kills boxq and orphans the session.
    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigquit = signal(SignalKind::quit())?;
    let mut execution = target.session().execution().await?;
    let token = CancellationToken::new();
    let exec = execution.exec(command, token.clone()).await?;
    let id = exec.execution_id;

    let raw = RawMode::enable()?;
    // Guest stderr shares the TTY with stdout, so it all lands on boxq's stdout.
    let mut output = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
    let pump = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(chunk) = output.next().await {
            if stdout.write_all(chunk.text().as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    // A plain thread, not tokio's blocking pool: a parked read(2) can't be cancelled, and the
    // runtime would wait on it at exit.
    if let Some(stdin) = exec.stdin_tx {
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            while let Ok(n @ 1..) = std::io::stdin().read(&mut buf) {
                if stdin.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }

    if let Some((rows, cols)) = size() {
        let _ = execution.resize_tty(&id, rows, cols, 0, 0).await;
    }
    let mut results = exec.result_rx;
    let result = loop {
        let signo = tokio::select! {
            result = results.recv() => break result,
            _ = sigwinch.recv() => {
                if let Some((rows, cols)) = size() {
                    let _ = execution.resize_tty(&id, rows, cols, 0, 0).await;
                }
                continue;
            }
            _ = sigint.recv() => libc::SIGINT,
            _ = sigterm.recv() => libc::SIGTERM,
            _ = sighup.recv() => libc::SIGHUP,
            _ = sigquit.recv() => libc::SIGQUIT,
        };
        let _ = execution.signal(&id, signo).await;
    };
    // Output still in flight when the process exits is flushed before the terminal is restored.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pump).await;
    drop(raw);
    token.cancel();

    let result = result.context("the session ended without an exit status")?;
    if let Some(message) = result.error_message {
        anyhow::bail!("the session did not end normally: {message}");
    }
    Ok(shell_status(result.exit_code))
}

/// `argv` as a session runs it: as root in the Workspace's directory, with the session's env.
pub(crate) fn command(target: &ExecTarget, ws: &Workspace, argv: &[&str]) -> BoxCommand {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    target
        .command(argv[0])
        .args(argv[1..].iter().copied())
        .working_dir(ws.path().to_string_lossy())
        .user("root")
        // Claude Code refuses to skip permissions as root unless it knows it is sandboxed.
        .env("IS_SANDBOX", "1")
        .env("TERM", term)
}

/// BoxLite reports death by signal N as -N; a shell reports it as 128 + N.
pub(crate) fn shell_status(exit_code: i32) -> i32 {
    if exit_code < 0 {
        128 - exit_code
    } else {
        exit_code
    }
}

/// boxq's terminal size as (rows, cols), if it has one.
fn size() -> Option<(u32, u32)> {
    let ws = tcgetwinsize(std::io::stdout()).ok()?;
    (ws.ws_col > 0).then(|| (ws.ws_row.into(), ws.ws_col.into()))
}

/// boxq's terminal in raw mode, so every key reaches the guest's TTY; restored on drop.
struct RawMode(Option<Termios>);

impl RawMode {
    fn enable() -> anyhow::Result<Self> {
        let stdin = std::io::stdin();
        if !isatty(&stdin) {
            return Ok(Self(None));
        }
        let saved = tcgetattr(&stdin).context("cannot read the terminal's mode")?;
        let mut raw = saved.clone();
        raw.make_raw();
        tcsetattr(&stdin, OptionalActions::Now, &raw).context("cannot set raw mode")?;
        Ok(Self(Some(saved)))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shell_status;

    #[test]
    fn death_by_signal_reports_as_a_shell_does() {
        assert_eq!([0, 3, -9, -15].map(shell_status), [0, 3, 137, 143]);
    }
}
