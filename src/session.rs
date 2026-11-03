//! Interactive sessions in a Sandbox: a TTY command through the lock-free [`ExecTarget`], driven
//! from boxq's own terminal (jonaswre-labs/boxlite-quick#15).
//!
//! The raw mode is adapted from boxlite-cli 0.11.0's `terminal/mod.rs` (Apache-2.0).

use crate::Workspace;
use crate::boxes::{ExecTarget, SandboxCommand};
use anyhow::Context;
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcsetattr};

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
    target: &impl ExecTarget,
    ws: &Workspace,
    argv: &[&str],
) -> anyhow::Result<i32> {
    let _raw = RawMode::enable()?;
    target
        .interactive(&command(ws, argv))
        .await
        .with_context(|| {
            format!(
                "cannot run the session in the Sandbox for {}",
                ws.path().display()
            )
        })
}

/// `argv` as a session runs it: as root in the Workspace's directory, with the session's env.
pub(crate) fn command(ws: &Workspace, argv: &[&str]) -> SandboxCommand {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    SandboxCommand {
        dir: Some(ws.path().into()),
        // Claude Code refuses to skip permissions as root unless it knows it is sandboxed.
        env: vec![("IS_SANDBOX".into(), "1".into()), ("TERM".into(), term)],
        ..SandboxCommand::new(argv).as_root()
    }
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
