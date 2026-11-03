//! The Boxes behind the Sandboxes, and [`BoxLite`], the adapter that runs them on BoxLite 0.11
//! and owns what that release needs worked around: its home lock, the VMs it leaks, the start
//! failures that are worth trying again, and running a command with no runtime open.
//!
//! [`BoxLiteTarget::interactive`]'s terminal handling is adapted from boxlite-cli 0.11.0's
//! `terminal/mod.rs` (Apache-2.0). It drives the raw `ExecComponents` channels, because
//! `Execution`'s constructor is `pub(crate)`.

use crate::auth::Auth;
use crate::image_store::{is_config_mismatch, remove_bad_configs};
use crate::reap::reap_box;
use crate::spec::Spec;
use anyhow::Context;
use boxlite::runtime::options::{
    BoxOptions, BoxliteOptions, NetworkMode, NetworkSpec, OutboundNetworkConfig, RootfsSpec,
    VolumeSpec,
};
use boxlite::{
    BoxCommand, BoxStatus, BoxliteError, BoxliteRuntime, ExecResult, GuestSession, LiteBox,
};
use boxlite_shared::BoxTransport;
use rustix::termios::tcgetwinsize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// How often to retry a busy `BOXLITE_HOME` lock.
const POLL: Duration = Duration::from_millis(100);

/// How long a command's output may still arrive once it has ended.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);

/// Where the Boxes live.
pub trait Boxes {
    /// Where a command runs in one of these Boxes.
    type Target: ExecTarget;

    /// The Boxes, open for one command's worth of finding, creating, starting, stopping,
    /// removing or listing. Only one [`Runtime`] is open at a time, across every boxq process:
    /// opening waits for any other to close. Callers drop it before any long-lived work
    /// (jonaswre-labs/boxlite-quick#8); a command runs in a Box through its [`ExecTarget`]
    /// with no runtime open.
    fn open(&self) -> anyhow::Result<impl Runtime<Target = Self::Target> + '_>;
}

/// The Boxes, open. Each Box is named by its Workspace's [`crate::Workspace::box_name`]; every
/// method but `find`, `get_or_create` and `list` expects the Box to exist.
#[allow(async_fn_in_trait)]
pub trait Runtime {
    /// Where a command runs in one of these Boxes.
    type Target: ExecTarget;

    async fn find(&self, name: &str) -> anyhow::Result<Option<Found>>;

    /// The Box named `name`, created from `new` if there is none. A created Box isn't running.
    async fn get_or_create(&self, name: &str, new: &NewBox<'_>) -> anyhow::Result<Opened>;

    /// Tries once to start the Box, after ending any VM an earlier stop or failed start left
    /// running, so two never share its disk.
    async fn start(&self, name: &str) -> Result<(), StartFailure>;

    /// Makes the Box's egress `allow_net`, as an Egress Policy gives it.
    async fn allow(&self, name: &str, allow_net: &[String]) -> anyhow::Result<()>;

    /// Where a command runs in the running Box, once this runtime is closed.
    async fn exec_target(&self, name: &str) -> anyhow::Result<Self::Target>;

    /// Stops the Box if it runs, keeping its state, and ends whatever still runs its VM.
    async fn stop(&self, name: &str) -> anyhow::Result<()>;

    /// Removes the Box and its state, and ends whatever still runs its VM.
    async fn remove(&self, name: &str) -> anyhow::Result<()>;

    async fn list(&self) -> anyhow::Result<Vec<Found>>;
}

/// A Box that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub running: bool,
}

/// What [`Runtime::get_or_create`] found or made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    pub created: bool,
    pub running: bool,
}

/// What a new Box is created from. All of it is fixed once the Box exists, but its egress,
/// which [`Runtime::allow`] changes.
pub struct NewBox<'a> {
    pub spec: &'a Spec,
    /// The host directories bind-mounted into the Box, each at its own path.
    pub shared_dirs: &'a [PathBuf],
    /// Its egress from the first boot, so a new Box never runs with the full access BoxLite
    /// gives a Box with none.
    pub allow_net: &'a [String],
    pub auth: &'a Auth,
}

/// Why a start failed, and whether starting the Box again may succeed.
#[derive(Debug)]
pub enum StartFailure {
    /// The Box said it was ready before it was listening, and has been stopped and marked
    /// failed, so starting it again boots it afresh.
    NotReady(anyhow::Error),
    /// The Box's image was damaged in the image store, and the damaged part has been removed,
    /// so starting it again downloads that part again.
    DamagedImage(anyhow::Error),
    /// Anything else, which starting again won't fix.
    Failed(anyhow::Error),
}

impl StartFailure {
    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::NotReady(e) | Self::DamagedImage(e) | Self::Failed(e) => e,
        }
    }
}

/// A running Sandbox to run commands in. A command runs with no [`Runtime`] open, so it never
/// holds the home's lock, however long it runs (jonaswre-labs/boxlite-quick#8).
///
/// Every exit status is reported as a shell reports it: death by signal N is 128 + N. A command
/// that ends without a status, or that BoxLite says did not end normally, is an error.
#[allow(async_fn_in_trait)]
pub trait ExecTarget {
    /// Runs `command` to its end with `stdin` as its input, and returns what it printed.
    async fn output(&self, command: &SandboxCommand, stdin: Vec<u8>) -> anyhow::Result<Output>;

    /// Runs `command` with no input, handing `sink` each chunk it prints as it arrives, until it
    /// ends or `interrupt` gives a signal number. Then the command is sent SIGTERM, and waited
    /// for.
    async fn stream(
        &self,
        command: &SandboxCommand,
        sink: impl FnMut(Chunk),
        interrupt: impl Future<Output = i32>,
    ) -> anyhow::Result<Streamed>;

    /// Runs `command` on a TTY wired to boxq's own terminal until it ends, and returns its exit
    /// status. boxq's input, its terminal's size, and the signals that would end boxq go to the
    /// command; its output goes to boxq's stdout. The caller puts the terminal in raw mode.
    async fn interactive(&self, command: &SandboxCommand) -> anyhow::Result<i32>;
}

/// A command to run in a Sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    /// The program and its arguments; never empty.
    pub argv: Vec<String>,
    /// Whether root runs it, rather than the image's own user.
    pub root: bool,
    /// The directory it runs in, such as the Workspace's; `None` runs it in `/`.
    pub dir: Option<PathBuf>,
    /// Env it gets on top of the image's.
    pub env: Vec<(String, String)>,
}

impl SandboxCommand {
    /// `argv`, run by the image's own user in `/`.
    pub fn new(argv: &[&str]) -> Self {
        assert!(!argv.is_empty(), "a command needs a program");
        Self {
            argv: argv.iter().map(|&arg| arg.into()).collect(),
            root: false,
            dir: None,
            env: Vec::new(),
        }
    }

    /// The command, run by root.
    pub fn as_root(self) -> Self {
        Self { root: true, ..self }
    }

    fn program(&self) -> &str {
        &self.argv[0]
    }
}

/// What [`ExecTarget::output`] ran printed, and its exit status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

/// Some of a command's stdout or of its stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Stdout(String),
    Stderr(String),
}

impl Chunk {
    pub fn text(&self) -> &str {
        match self {
            Self::Stdout(text) | Self::Stderr(text) => text,
        }
    }
}

/// How a command [`ExecTarget::stream`] ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streamed {
    /// It ended by itself, with this exit status.
    Exited(i32),
    /// The interrupt stopped it, giving this signal number.
    Interrupted(i32),
}

/// The Boxes in one `BOXLITE_HOME`, run by BoxLite 0.11.
pub struct BoxLite {
    home: PathBuf,
}

impl BoxLite {
    pub fn new(home: &Path) -> Self {
        Self { home: home.into() }
    }

    /// The `BOXLITE_HOME`, holding the Boxes and their image store.
    pub fn home(&self) -> &Path {
        &self.home
    }
}

impl Boxes for BoxLite {
    type Target = BoxLiteTarget;

    /// Opens BoxLite's runtime, which takes the home's lock. Another boxq holds it for seconds
    /// at most (a cold create), so a busy lock is waited out, not reported.
    fn open(&self) -> anyhow::Result<impl Runtime<Target = BoxLiteTarget> + '_> {
        let mut announced = false;
        let rt = loop {
            match BoxliteRuntime::new(BoxliteOptions {
                home_dir: self.home.clone(),
                ..Default::default()
            }) {
                // BoxLite 0.11 reports a busy lock only through this message, wrapped in another.
                Err(BoxliteError::Internal(msg))
                    if msg.contains("Another BoxliteRuntime is already") =>
                {
                    if !announced {
                        eprintln!("Waiting for another boxq command to finish...");
                        announced = true;
                    }
                    std::thread::sleep(POLL);
                }
                opened => {
                    break opened.with_context(|| {
                        format!("cannot open BoxLite's runtime in {}", self.home.display())
                    })?;
                }
            }
        };
        Ok(Open {
            held: Mutex::default(),
            rt,
            home: &self.home,
        })
    }
}

/// BoxLite's runtime, open.
struct Open<'a> {
    /// Every Box `get_or_create` found or made, kept until the runtime closes: BoxLite 0.11
    /// finds a Box it hasn't yet written to its database only while something holds it. Declared first, so
    /// dropped before the runtime.
    held: Mutex<Vec<LiteBox>>,
    rt: BoxliteRuntime,
    home: &'a Path,
}

impl Open<'_> {
    async fn get(&self, name: &str) -> anyhow::Result<LiteBox> {
        self.rt
            .get(name)
            .await?
            .with_context(|| format!("there is no Box {name}"))
    }
}

impl Runtime for Open<'_> {
    type Target = BoxLiteTarget;

    async fn find(&self, name: &str) -> anyhow::Result<Option<Found>> {
        Ok(self.rt.get_info(name).await?.map(|info| Found {
            name: name.into(),
            running: info.status == BoxStatus::Running,
        }))
    }

    async fn get_or_create(&self, name: &str, new: &NewBox<'_>) -> anyhow::Result<Opened> {
        let mut options = BoxOptions {
            rootfs: RootfsSpec::Image(new.spec.image.clone()),
            cpus: Some(new.spec.cpus_or_default()),
            memory_mib: Some(new.spec.memory_mib_or_default()),
            disk_size_gb: Some(new.spec.disk_gib_or_default()),
            entrypoint: Some(vec![]),
            cmd: Some(vec!["sleep".into(), "infinity".into()]),
            volumes: new
                .shared_dirs
                .iter()
                .map(|dir| {
                    let dir = dir.to_string_lossy();
                    VolumeSpec::bind_mount(dir.clone(), dir)
                })
                .collect(),
            network: NetworkSpec::Enabled {
                allow_net: new.allow_net.to_vec(),
            },
            auto_delete: Some(0),
            detach: true,
            ..Default::default()
        };
        new.auth.apply(&mut options);
        let (b, created) = self.rt.get_or_create(options, Some(name.into())).await?;
        let info = b.info().await.context("cannot check the Box's state")?;
        self.held.lock().unwrap().push(b);
        Ok(Opened {
            created,
            running: info.status == BoxStatus::Running,
        })
    }

    /// BoxLite 0.11 can tell the host a Box is ready before libkrun listens on its `box.sock`:
    /// libkrun binds the socket on a thread of its own, which nothing orders before it passes on
    /// the guest's ready signal. Where creating the socket file is slow, as on GitHub's runners
    /// under the real-Box tests, the host's first connection finds no socket, and the start fails
    /// with "gRPC transport error: transport error" (jonaswre-labs/boxlite-quick#50).
    ///
    /// BoxLite 0.11 can also leave an image config in its store empty, which it then reads back
    /// on every start of that image (see [`is_config_mismatch`]). Removing the bad configs makes
    /// the next start download them again (jonaswre-labs/boxlite-quick#60).
    async fn start(&self, name: &str) -> Result<(), StartFailure> {
        let b = self.get(name).await.map_err(StartFailure::Failed)?;
        reap_box(self.home, b.id().as_ref()).map_err(StartFailure::Failed)?;
        match b.start().await {
            Ok(()) => Ok(()),
            Err(e @ BoxliteError::RpcTransport(_)) => Err(StartFailure::NotReady(e.into())),
            Err(e) if is_config_mismatch(&e) => match remove_bad_configs(self.home) {
                Ok(()) => Err(StartFailure::DamagedImage(e.into())),
                Err(removing) => Err(StartFailure::Failed(
                    anyhow::Error::new(e).context(format!("{removing:#}")),
                )),
            },
            Err(e) => Err(StartFailure::Failed(e.into())),
        }
    }

    async fn allow(&self, name: &str, allow_net: &[String]) -> anyhow::Result<()> {
        let b = self.get(name).await?;
        b.network()
            .update_outbound(OutboundNetworkConfig {
                mode: NetworkMode::Enabled,
                allow_net: allow_net.to_vec(),
            })
            .await?;
        Ok(())
    }

    async fn exec_target(&self, name: &str) -> anyhow::Result<BoxLiteTarget> {
        let b = self.get(name).await?;
        let box_dir = self.home.join("boxes").join(b.id().as_ref());
        Ok(BoxLiteTarget {
            socket: box_dir.join("sockets/box.sock"),
            container_id: container_id(&box_dir)?,
        })
    }

    async fn stop(&self, name: &str) -> anyhow::Result<()> {
        let b = self.get(name).await?;
        let info = b.info().await.context("cannot check the Box's state")?;
        if info.status == BoxStatus::Running {
            b.stop().await?;
        }
        reap_box(self.home, b.id().as_ref())
    }

    async fn remove(&self, name: &str) -> anyhow::Result<()> {
        let info = self.rt.get_info(name).await?;
        let info = info.with_context(|| format!("there is no Box {name}"))?;
        self.rt.remove(name, true).await?;
        reap_box(self.home, info.id.as_ref())
    }

    async fn list(&self) -> anyhow::Result<Vec<Found>> {
        let found = self.rt.list_info().await?.into_iter().map(|info| Found {
            name: info.name.unwrap_or_else(|| info.id.to_string()),
            running: info.status == BoxStatus::Running,
        });
        Ok(found.collect())
    }
}

/// The container id is `pub(crate)` in BoxLite 0.11; it is the one directory name under
/// `boxes/<id>/shared/containers/`.
fn container_id(box_dir: &Path) -> anyhow::Result<String> {
    let dir = box_dir.join("shared/containers");
    let mut ids = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(ids.len() == 1, "expected one container, found {ids:?}");
    Ok(ids.pop().unwrap())
}

/// A running Box, reached through its own socket with no runtime open, and the container in it
/// that runs the image. This leans on BoxLite internals: the socket layout, the
/// `BOXLITE_EXECUTOR` env protocol and the container-id directory.
pub struct BoxLiteTarget {
    socket: PathBuf,
    container_id: String,
}

impl BoxLiteTarget {
    /// A session on the Box's socket, for driving an exec by hand, as a test that types into a
    /// TTY does. boxq itself runs commands through [`ExecTarget`].
    pub fn session(&self) -> GuestSession {
        GuestSession::new(BoxTransport::Unix {
            socket_path: self.socket.clone(),
        })
    }

    /// `program` as a command for this Box, run in `/` by the image's user. `LiteBox::exec`
    /// would name the container itself; a session on the socket has to.
    pub fn command(&self, program: &str) -> BoxCommand {
        BoxCommand::new(program)
            .env(
                "BOXLITE_EXECUTOR",
                format!("container={}", self.container_id),
            )
            .working_dir("/")
    }

    fn box_command(&self, command: &SandboxCommand) -> BoxCommand {
        let mut built = self.command(command.program()).args(&command.argv[1..]);
        if command.root {
            built = built.user("root");
        }
        if let Some(dir) = &command.dir {
            built = built.working_dir(dir.to_string_lossy());
        }
        for (key, value) in &command.env {
            built = built.env(key, value);
        }
        built
    }
}

impl ExecTarget for BoxLiteTarget {
    async fn output(&self, command: &SandboxCommand, stdin: Vec<u8>) -> anyhow::Result<Output> {
        let token = CancellationToken::new();
        let _stop = token.clone().drop_guard();
        let mut exec = self
            .session()
            .execution()
            .await
            .with_context(|| cannot_start(command))?
            .exec(self.box_command(command), token)
            .await
            .with_context(|| cannot_start(command))?;
        // Chunked, because BoxLite's guest refuses a stdin message over 4 MiB. Dropping the sender
        // closes the command's stdin.
        if let Some(tx) = exec.stdin_tx.take() {
            for chunk in stdin.chunks(64 << 10) {
                let _ = tx.send(chunk.to_vec());
            }
        }
        let mut output = Output::default();
        let mut chunks = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Chunk::Stdout(text) => output.stdout.push_str(&text),
                Chunk::Stderr(text) => output.stderr.push_str(&text),
            }
        }
        output.status = status(exec.result_rx.recv().await, command)?;
        Ok(output)
    }

    async fn stream(
        &self,
        command: &SandboxCommand,
        mut sink: impl FnMut(Chunk),
        interrupt: impl Future<Output = i32>,
    ) -> anyhow::Result<Streamed> {
        let mut interrupt = pin!(interrupt);
        let token = CancellationToken::new();
        let _stop = token.clone().drop_guard();
        let mut execution = self
            .session()
            .execution()
            .await
            .with_context(|| cannot_start(command))?;
        let exec = execution
            .exec(self.box_command(command), token)
            .await
            .with_context(|| cannot_start(command))?;
        drop(exec.stdin_tx);
        let mut chunks = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
        let mut results = exec.result_rx;
        let mut interrupted = None;
        let result = loop {
            tokio::select! {
                result = results.recv() => break result,
                Some(chunk) = chunks.next() => sink(chunk),
                signo = &mut interrupt, if interrupted.is_none() => {
                    interrupted = Some(signo);
                    let _ = execution.signal(&exec.execution_id, libc::SIGTERM).await;
                }
            }
        };
        // Output still in flight when the command ends is passed on, unless something the
        // command left running holds it open.
        let drain = async {
            while let Some(chunk) = chunks.next().await {
                sink(chunk);
            }
        };
        let _ = tokio::time::timeout(OUTPUT_DRAIN, drain).await;
        match interrupted {
            Some(signo) => Ok(Streamed::Interrupted(signo)),
            None => Ok(Streamed::Exited(status(result, command)?)),
        }
    }

    async fn interactive(&self, command: &SandboxCommand) -> anyhow::Result<i32> {
        // Caught from before the exec starts, so no signal kills boxq and orphans the command.
        let mut sigwinch = signal(SignalKind::window_change())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sighup = signal(SignalKind::hangup())?;
        let mut sigquit = signal(SignalKind::quit())?;
        let mut execution = self
            .session()
            .execution()
            .await
            .with_context(|| cannot_start(command))?;
        let token = CancellationToken::new();
        let _stop = token.clone().drop_guard();
        let exec = execution
            .exec(self.box_command(command).tty(true), token)
            .await
            .with_context(|| cannot_start(command))?;
        let id = exec.execution_id;

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

        if let Some((rows, cols)) = terminal_size() {
            let _ = execution.resize_tty(&id, rows, cols, 0, 0).await;
        }
        let mut results = exec.result_rx;
        let result = loop {
            let signo = tokio::select! {
                result = results.recv() => break result,
                _ = sigwinch.recv() => {
                    if let Some((rows, cols)) = terminal_size() {
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
        // Output still in flight when the command exits is written before the caller restores
        // the terminal.
        let _ = tokio::time::timeout(OUTPUT_DRAIN, pump).await;
        status(result, command)
    }
}

/// The error context for a command BoxLite could not start.
fn cannot_start(command: &SandboxCommand) -> String {
    format!("cannot start `{}`", command.program())
}

/// `result`'s exit status as a shell reports it, or why `command` has none.
fn status(result: Option<ExecResult>, command: &SandboxCommand) -> anyhow::Result<i32> {
    let program = command.program();
    let result = result.with_context(|| format!("`{program}` ended without an exit status"))?;
    if let Some(message) = result.error_message {
        anyhow::bail!("`{program}` did not end normally: {message}");
    }
    Ok(shell_status(result.exit_code))
}

/// BoxLite reports death by signal N as -N; a shell reports it as 128 + N.
fn shell_status(exit_code: i32) -> i32 {
    if exit_code < 0 {
        128 - exit_code
    } else {
        exit_code
    }
}

/// boxq's terminal size as (rows, cols), if it has one.
fn terminal_size() -> Option<(u32, u32)> {
    let ws = tcgetwinsize(std::io::stdout()).ok()?;
    (ws.ws_col > 0).then(|| (ws.ws_row.into(), ws.ws_col.into()))
}

/// A command's stdout and stderr, read as one sequence of chunks in arrival order.
struct OutputChunks {
    stdout: UnboundedReceiver<String>,
    stderr: UnboundedReceiver<String>,
    stdout_open: bool,
    stderr_open: bool,
}

impl OutputChunks {
    fn new(stdout: UnboundedReceiver<String>, stderr: UnboundedReceiver<String>) -> Self {
        Self {
            stdout,
            stderr,
            stdout_open: true,
            stderr_open: true,
        }
    }

    /// The next chunk from either stream, or `None` once both have closed.
    async fn next(&mut self) -> Option<Chunk> {
        while self.stdout_open || self.stderr_open {
            let (chunk, from_stdout) = tokio::select! {
                c = self.stdout.recv(), if self.stdout_open => (c, true),
                c = self.stderr.recv(), if self.stderr_open => (c, false),
            };
            match chunk {
                Some(text) if from_stdout => return Some(Chunk::Stdout(text)),
                Some(text) => return Some(Chunk::Stderr(text)),
                None if from_stdout => self.stdout_open = false,
                None => self.stderr_open = false,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn death_by_signal_reports_as_a_shell_does() {
        assert_eq!([0, 3, -9, -15].map(shell_status), [0, 3, 137, 143]);
    }

    #[test]
    fn a_command_that_did_not_end_normally_or_has_no_status_is_an_error() {
        let tar = SandboxCommand::new(&["tar", "-x"]);
        let died = ExecResult {
            exit_code: -9,
            error_message: Some("container init died".into()),
        };

        let died = status(Some(died), &tar).unwrap_err();
        let lost = status(None, &tar).unwrap_err();

        assert_eq!(
            (died.to_string(), lost.to_string()),
            (
                "`tar` did not end normally: container init died".into(),
                "`tar` ended without an exit status".into()
            )
        );
    }
}
