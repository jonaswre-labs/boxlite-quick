//! The Setup Hooks: the Global then the Workspace Config's `setup`, each run once when a Sandbox
//! is created and retried until it succeeds (jonaswre-labs/boxlite-quick#30).

use crate::Workspace;
use crate::config::SetupHooks;
use crate::sandbox::{OutputChunks, Sandboxes, Up};
use crate::session;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::pin::pin;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

/// BoxLite signals only the process it started, though each exec gets its own process group. So
/// the hook runs in the background of a shell that, told to stop, kills the whole group.
const WRAPPER: &str = r#"trap 'kill -KILL 0' TERM; sh -c "$1" & wait $!"#;

/// Which config a Setup Hook comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Global,
    Workspace,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Global => "Global Setup Hook",
            Self::Workspace => "Workspace Setup Hook",
        })
    }
}

/// What a Sandbox's record keeps of its Setup Hooks. A new Sandbox starts with neither done.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub global: Hook,
    pub workspace: Hook,
}

/// One Setup Hook as last run in a Sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hook {
    /// The hook last run, or `None` if the config had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Whether it succeeded. One that hasn't is run again, from the current config.
    pub done: bool,
}

/// A Setup Hook that did not succeed, which the next `run` or `shell` retries.
#[derive(Debug, PartialEq, Eq)]
pub struct Failed {
    pub source: Source,
    /// Its exit status as a shell reports it, or 128 + N if boxq was stopped by signal N.
    pub status: i32,
    pub interrupted: bool,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.interrupted {
            write!(f, "The {} was interrupted", self.source)?;
        } else {
            write!(f, "The {} failed with status {}", self.source, self.status)?;
        }
        f.write_str("; the next `boxq run` or `boxq shell` runs it again.")
    }
}

impl Setup {
    fn hook_mut(&mut self, source: Source) -> &mut Hook {
        match source {
            Source::Global => &mut self.global,
            Source::Workspace => &mut self.workspace,
        }
    }

    /// The hooks that succeeded and have since changed in `hooks`, named for
    /// [`crate::spec::mismatch_warning`]. One that hasn't succeeded runs from the config anyway.
    pub fn changes(&self, hooks: &SetupHooks) -> Vec<&'static str> {
        [
            ("Global Setup Hook", &self.global, &hooks.global),
            ("Workspace Setup Hook", &self.workspace, &hooks.workspace),
        ]
        .into_iter()
        .filter_map(|(name, ran, now)| (ran.done && ran.command != *now).then_some(name))
        .collect()
    }
}

/// Runs the Sandbox's Setup Hooks that haven't succeeded, from `hooks`, stopping at the first that
/// fails. Ctrl-C, SIGHUP or SIGTERM kills the hook running and counts as its failure. A Sandbox
/// boxq has no setup record for counts as set up.
pub async fn run(
    sandboxes: &Sandboxes,
    ws: &Workspace,
    up: &Up,
    hooks: &SetupHooks,
) -> anyhow::Result<Option<Failed>> {
    // tokio keeps catching a signal once anything listens for it, which would leave Ctrl-C dead
    // until the session starts, so boxq listens only if a hook will run.
    let will_run = up.setup.as_ref().is_some_and(|setup| {
        [
            (&setup.global, &hooks.global),
            (&setup.workspace, &hooks.workspace),
        ]
        .iter()
        .any(|(ran, now)| !ran.done && now.is_some())
    });
    if !will_run {
        return run_until(sandboxes, ws, up, hooks, std::future::pending()).await;
    }
    // Caught from before a hook starts, so no signal kills boxq and orphans it.
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let interrupt = async {
        tokio::select! {
            _ = sigint.recv() => libc::SIGINT,
            _ = sigterm.recv() => libc::SIGTERM,
            _ = sighup.recv() => libc::SIGHUP,
        }
    };
    run_until(sandboxes, ws, up, hooks, interrupt).await
}

/// [`run`], with `interrupt` giving the signal that stops it in place of boxq's own signals.
pub async fn run_until(
    sandboxes: &Sandboxes,
    ws: &Workspace,
    up: &Up,
    hooks: &SetupHooks,
    interrupt: impl Future<Output = i32>,
) -> anyhow::Result<Option<Failed>> {
    let Some(mut setup) = up.setup.clone() else {
        return Ok(None);
    };
    let mut interrupt = pin!(interrupt);
    let configured = [
        (Source::Global, &hooks.global),
        (Source::Workspace, &hooks.workspace),
    ];
    for (source, command) in configured {
        if setup.hook_mut(source).done {
            continue;
        }
        // Recorded before it runs, so a hook boxq never saw finish stays unfinished.
        *setup.hook_mut(source) = Hook {
            command: command.clone(),
            done: command.is_none(),
        };
        sandboxes.record_setup(ws, &setup)?;
        let Some(command) = command else {
            continue;
        };
        eprintln!("Running the {source}...");
        let (status, interrupted) = run_hook(up, ws, command, interrupt.as_mut()).await?;
        if status != 0 {
            return Ok(Some(Failed {
                source,
                status,
                interrupted,
            }));
        }
        setup.hook_mut(source).done = true;
        sandboxes.record_setup(ws, &setup)?;
    }
    Ok(None)
}

/// Runs one hook to its end, streaming its output to stderr, and returns its exit status as a
/// shell reports it and whether `interrupt` stopped it.
async fn run_hook(
    up: &Up,
    ws: &Workspace,
    hook: &str,
    mut interrupt: std::pin::Pin<&mut impl Future<Output = i32>>,
) -> anyhow::Result<(i32, bool)> {
    let command = session::command(&up.target, ws, &["sh", "-c", WRAPPER, "sh", hook]);
    let mut execution = up.target.session().execution().await?;
    let token = CancellationToken::new();
    let exec = execution.exec(command, token.clone()).await?;
    drop(exec.stdin_tx);
    let mut output = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
    let pump = tokio::spawn(async move {
        while let Some(chunk) = output.next().await {
            eprint!("{}", chunk.text());
        }
    });
    let mut results = exec.result_rx;
    let (result, signo) = tokio::select! {
        result = results.recv() => (result, None),
        signo = &mut interrupt => {
            let _ = execution.signal(&exec.execution_id, libc::SIGTERM).await;
            (results.recv().await, Some(signo))
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pump).await;
    token.cancel();
    if let Some(signo) = signo {
        return Ok((128 + signo, true));
    }
    let result = result.context("the setup hook ended without an exit status")?;
    if let Some(message) = result.error_message {
        anyhow::bail!("the setup hook did not end normally: {message}");
    }
    Ok((session::shell_status(result.exit_code), false))
}
