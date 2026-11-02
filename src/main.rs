use boxq::Workspace;
use boxq::auth::Auth;
use boxq::claude;
use boxq::claude_config::ClaudeConfig;
use boxq::config::Config;
use boxq::egress::{Approvals, Policy};
use boxq::log;
use boxq::sandbox::{NoSandbox, Sandboxes, Up};
use boxq::session;
use boxq::setup::{self, Failed};
use boxq::spec::{self, Spec};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

/// Run Claude Code against a Workspace inside an isolated Sandbox.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or reattach to the Workspace's Sandbox and start Claude Code in it.
    Run(Target),
    /// Open a shell in the Workspace's Sandbox.
    Shell(Target),
    /// List Sandboxes by Workspace path.
    Ls,
    /// Stop the Workspace's Sandbox, keeping its state.
    Stop(Target),
    /// Remove the Workspace's Sandbox and its state.
    Rm(Target),
}

#[derive(clap::Args)]
struct Target {
    /// The Workspace; defaults to the current directory.
    path: Option<PathBuf>,
}

impl Target {
    fn workspace(&self) -> anyhow::Result<Workspace> {
        Workspace::resolve(self.path.as_deref().unwrap_or(".".as_ref()))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log_to_stderr();
    let sandboxes = Sandboxes::from_process_env()?;
    match Cli::parse().command {
        Command::Run(t) => {
            let ws = t.workspace()?;
            let claude_config = ClaudeConfig::from_process_env()?;
            let (sandbox, failed) = ready_sandbox(&sandboxes, &ws).await?;
            if let Some(failed) = failed {
                eprintln!("{failed}");
                std::process::exit(failed.status);
            }
            claude::check_installed(&sandbox.target, &ws).await?;
            claude_config.sync(&ws, &sandbox.target).await?;
            let status = session::interactive(&sandbox.target, &ws, session::CLAUDE).await?;
            std::process::exit(status);
        }
        Command::Shell(t) => {
            let ws = t.workspace()?;
            let (sandbox, failed) = ready_sandbox(&sandboxes, &ws).await?;
            if let Some(failed) = failed {
                eprintln!("{failed}");
                // A signal means stop boxq, not open a shell.
                if failed.interrupted {
                    std::process::exit(failed.status);
                }
                eprintln!("Opening the shell anyway, so it can be debugged.");
            }
            let status = session::interactive(&sandbox.target, &ws, session::LOGIN_SHELL).await?;
            std::process::exit(status);
        }
        Command::Ls => {
            for s in sandboxes.list().await? {
                let state = if s.running { "running" } else { "stopped" };
                match s.workspace {
                    Some(ws) => println!("{state}  {}", ws.display()),
                    None => println!("{state}  (Workspace unknown; Box {})", s.box_name),
                }
            }
        }
        Command::Stop(t) => {
            let ws = t.workspace()?;
            sandboxes.stop(&ws).await?;
            eprintln!("Stopped the Sandbox for {}", ws.path().display());
        }
        Command::Rm(t) => {
            let ws = t.workspace()?;
            let removed = sandboxes.rm(&ws).await;
            // Forgotten even if there is no Sandbox, so `rm` always resets the Workspace, but
            // kept if removing the Sandbox failed, since it may still be there.
            let gone = match &removed {
                Ok(()) => true,
                Err(e) => e.is::<NoSandbox>(),
            };
            if gone {
                Approvals::from_process_env()?.revoke(&ws)?;
            }
            removed?;
            eprintln!("Removed the Sandbox for {}", ws.path().display());
        }
    }
    Ok(())
}

/// Prints BoxLite's `error` events on stderr, or what `BOXQ_LOG` (an `EnvFilter`) selects. BoxLite
/// returns only a short sentence for a host failure and logs the full report, with its paths and
/// fix commands, at `error` (jonaswre-labs/boxlite-quick#33).
///
/// By default they go through [`log::STDERR`], so what a Box start attempt logs is dropped if
/// boxq starts the Box again (see `Sandboxes::start`). With `BOXQ_LOG` set, and not empty, they go
/// straight to stderr: whoever asked for more wants every attempt, and wants it as it happens, even
/// if a slow start is interrupted. An empty `BOXQ_LOG`, as a workflow input left blank gives, asks
/// for nothing more.
fn log_to_stderr() {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::ERROR.into())
        .with_env_var("BOXQ_LOG")
        .from_env_lossy();
    let writer = if std::env::var_os("BOXQ_LOG").is_some_and(|v| !v.is_empty()) {
        BoxMakeWriter::new(std::io::stderr)
    } else {
        BoxMakeWriter::new(|| &*log::STDERR)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(std::io::stderr().is_terminal())
        .without_time()
        .init();
}

/// Creates the Workspace's Sandbox from the config, or reattaches to it, saying which and
/// warning where the Sandbox no longer matches the config, then runs any Setup Hook that hasn't
/// succeeded, returning the one that failed.
async fn ready_sandbox(
    sandboxes: &Sandboxes,
    ws: &Workspace,
) -> anyhow::Result<(Up, Option<Failed>)> {
    let config = Config::from_process_env(ws)?;
    let auth = Auth::from_config(&config)?;
    let spec = Spec::new(config.image.as_deref(), &config.resources)?;
    let egress = Policy::from_process_env(ws, &config)?;
    let up = sandboxes.up(ws, &spec, &egress, &auth).await?;
    let action = if up.created {
        "Created"
    } else {
        "Reattached to"
    };
    eprintln!("{action} the Sandbox for {}", ws.path().display());
    let mut changed = up.as_created.as_ref().map_or(vec![], |c| spec.changes(c));
    if let Some(setup) = &up.setup {
        changed.extend(setup.changes(&config.setup));
    }
    if let Some(warning) = spec::mismatch_warning(ws.path(), &changed) {
        eprintln!("{warning}");
    }
    if up.auth_changed {
        eprintln!(
            "The Sandbox for {} still has the base URL and API key it was created with; to \
             use the current ones, {}.",
            ws.path().display(),
            spec::recreate_hint(ws.path())
        );
    }
    let failed = setup::run(sandboxes, ws, &up, &config.setup).await?;
    Ok((up, failed))
}
