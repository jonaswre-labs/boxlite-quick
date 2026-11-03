//! The Sandbox lifecycle over a fake of the Boxes, so no Box boots here: which Sandboxes `up`
//! creates or reattaches, how it tries a start again, and what `stop`, `rm` and `list` refuse and
//! report. What BoxLite itself does is the real-Box tests' `lifecycle::` group.
//!
//! The fake runs no command; it replies as scripted. So the tests here also cover what boxq does
//! with a command's outcome: which Setup Hooks run and what their record keeps, the check for
//! Claude Code, and the Claude config sync's reads and writes (jonaswre-labs/boxlite-quick#65).

use boxq::Workspace;
use boxq::auth::Auth;
use boxq::boxes::{
    Boxes, Chunk, ExecTarget, Found, NewBox, Opened, Output, Runtime, SandboxCommand, StartFailure,
    Streamed,
};
use boxq::claude_config::ClaudeConfig;
use boxq::config::{BaseUrl, Egress, SetupHooks};
use boxq::egress::Policy;
use boxq::sandbox::{Listed, NoSandbox, Sandboxes, Up};
use boxq::setup::{self, Failed, Setup, Source};
use boxq::spec::Spec;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// What the fake does for the next start of a Box.
#[derive(Clone, Copy)]
enum Start {
    Starts,
    NotReady,
    DamagedImage,
    Fails,
}

/// What the next command run in a Sandbox does.
enum Reply {
    /// It prints this to stdout and succeeds.
    Prints(&'static str),
    /// It exits with this status, having printed this to stderr.
    Exits(i32, &'static str),
    /// It runs until interrupted.
    RunsUntilInterrupted,
}

/// The Boxes, kept in memory, shared with the targets they hand out.
#[derive(Default)]
struct Fake(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    /// Each Box by name, and whether it runs.
    boxes: BTreeMap<String, bool>,
    /// What the next starts do, in order; a start not scripted here starts the Box.
    starts: VecDeque<Start>,
    /// How many starts were tried.
    started: u32,
    /// The `allow_net` each new Box was created with.
    created_with: Vec<Vec<String>>,
    /// The `allow_net` last applied to each Box.
    allowed: BTreeMap<String, Vec<String>>,
    /// What the next commands do, in order; a command not scripted here succeeds, printing nothing.
    replies: VecDeque<Reply>,
    /// The argv of each command run, in order.
    ran: Vec<Vec<String>>,
}

/// The fake, open.
struct Open<'a>(&'a Fake);

/// Where the fake runs commands: it records them and replies as scripted.
struct FakeTarget(Arc<Mutex<State>>);

impl FakeTarget {
    fn run(&self, command: &SandboxCommand) -> Reply {
        let mut state = self.0.lock().unwrap();
        state.ran.push(command.argv.clone());
        state.replies.pop_front().unwrap_or(Reply::Prints(""))
    }
}

impl ExecTarget for FakeTarget {
    async fn output(&self, command: &SandboxCommand, _: Vec<u8>) -> anyhow::Result<Output> {
        Ok(match self.run(command) {
            Reply::Prints(stdout) => Output {
                stdout: stdout.into(),
                ..Default::default()
            },
            Reply::Exits(status, stderr) => Output {
                stderr: stderr.into(),
                status,
                ..Default::default()
            },
            Reply::RunsUntilInterrupted => panic!("`output` has no interrupt"),
        })
    }

    async fn stream(
        &self,
        command: &SandboxCommand,
        mut sink: impl FnMut(Chunk),
        interrupt: impl Future<Output = i32>,
    ) -> anyhow::Result<Streamed> {
        Ok(match self.run(command) {
            Reply::Prints(stdout) => {
                sink(Chunk::Stdout(stdout.into()));
                Streamed::Exited(0)
            }
            Reply::Exits(status, stderr) => {
                sink(Chunk::Stderr(stderr.into()));
                Streamed::Exited(status)
            }
            Reply::RunsUntilInterrupted => Streamed::Interrupted(interrupt.await),
        })
    }

    async fn interactive(&self, command: &SandboxCommand) -> anyhow::Result<i32> {
        Ok(match self.run(command) {
            Reply::Prints(_) => 0,
            Reply::Exits(status, _) => status,
            Reply::RunsUntilInterrupted => panic!("`interactive` has no interrupt"),
        })
    }
}

impl Boxes for Fake {
    type Target = FakeTarget;

    fn open(&self) -> anyhow::Result<impl Runtime<Target = FakeTarget> + '_> {
        Ok(Open(self))
    }
}

impl Runtime for Open<'_> {
    type Target = FakeTarget;

    async fn find(&self, name: &str) -> anyhow::Result<Option<Found>> {
        let state = self.0.0.lock().unwrap();
        Ok(state.boxes.get(name).map(|&running| Found {
            name: name.into(),
            running,
        }))
    }

    async fn get_or_create(&self, name: &str, new: &NewBox<'_>) -> anyhow::Result<Opened> {
        let mut state = self.0.0.lock().unwrap();
        if let Some(&running) = state.boxes.get(name) {
            return Ok(Opened {
                created: false,
                running,
            });
        }
        state.boxes.insert(name.into(), false);
        state.created_with.push(new.allow_net.to_vec());
        Ok(Opened {
            created: true,
            running: false,
        })
    }

    async fn start(&self, name: &str) -> Result<(), StartFailure> {
        let mut state = self.0.0.lock().unwrap();
        state.started += 1;
        let failed = |what: &str| anyhow::anyhow!("{what}");
        match state.starts.pop_front().unwrap_or(Start::Starts) {
            Start::Starts => {
                state.boxes.insert(name.into(), true);
                Ok(())
            }
            Start::NotReady => Err(StartFailure::NotReady(failed("transport error"))),
            Start::DamagedImage => Err(StartFailure::DamagedImage(failed("digest mismatch"))),
            Start::Fails => Err(StartFailure::Failed(failed("no KVM"))),
        }
    }

    async fn allow(&self, name: &str, allow_net: &[String]) -> anyhow::Result<()> {
        let mut state = self.0.0.lock().unwrap();
        state.allowed.insert(name.into(), allow_net.to_vec());
        Ok(())
    }

    async fn exec_target(&self, _: &str) -> anyhow::Result<FakeTarget> {
        Ok(FakeTarget(self.0.0.clone()))
    }

    async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.0.0.lock().unwrap().boxes.insert(name.into(), false);
        Ok(())
    }

    async fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.0.0.lock().unwrap().boxes.remove(name);
        Ok(())
    }

    async fn list(&self) -> anyhow::Result<Vec<Found>> {
        let state = self.0.0.lock().unwrap();
        let found = state.boxes.iter().map(|(name, &running)| Found {
            name: name.clone(),
            running,
        });
        Ok(found.collect())
    }
}

/// Sandboxes over the fake, under a home of their own, with Workspaces to put them in.
struct Fixture {
    sandboxes: Sandboxes<Fake>,
    dirs: Vec<TempDir>,
    _home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        Self {
            sandboxes: Sandboxes::with_boxes(home.path(), Fake::default()),
            dirs: Vec::new(),
            _home: home,
        }
    }

    fn workspace(&mut self) -> Workspace {
        self.dirs.push(TempDir::new().unwrap());
        Workspace::resolve(self.dirs.last().unwrap().path()).unwrap()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.sandboxes.boxes().0.lock().unwrap()
    }

    /// The next starts do `starts`, in order.
    fn script(&self, starts: &[Start]) {
        self.state().starts.extend(starts);
    }

    /// The next commands run in a Sandbox do `replies`, in order.
    fn reply(&self, replies: impl IntoIterator<Item = Reply>) {
        self.state().replies.extend(replies);
    }

    /// The last argument of each command run, in order: the hook, for a Setup Hook.
    fn ran_last_args(&self) -> Vec<String> {
        let state = self.state();
        state
            .ran
            .iter()
            .map(|argv| argv.last().unwrap().clone())
            .collect()
    }

    async fn up(&self, ws: &Workspace) -> anyhow::Result<Up<FakeTarget>> {
        self.sandboxes
            .up(ws, &spec(), &policy(&[]), &auth("sk-key"))
            .await
    }

    /// Brings up the Workspace's Sandbox and runs the Setup Hooks it hasn't finished, with no
    /// interrupt.
    async fn set_up(&self, ws: &Workspace, hooks: &SetupHooks) -> Option<Failed> {
        let up = self.up(ws).await.unwrap();
        let interrupt = std::future::pending();
        let failed = setup::run_until(&self.sandboxes, ws, &up, hooks, interrupt).await;
        failed.unwrap()
    }
}

fn hooks(global: Option<&str>, workspace: Option<&str>) -> SetupHooks {
    SetupHooks {
        global: global.map(Into::into),
        workspace: workspace.map(Into::into),
    }
}

fn spec() -> Spec {
    Spec::new(None, &Default::default()).unwrap()
}

fn policy(allow: &[&str]) -> Policy {
    let egress = Egress {
        builtins: false,
        allow: allow.iter().map(|&e| e.into()).collect(),
        allow_host_loopback: false,
        workspace_allow: vec![],
    };
    Policy::new(&egress, None, false, |_| vec![])
}

fn auth(key: &str) -> Auth {
    Auth::new(&BaseUrl::parse("https://api.anthropic.com").unwrap(), key)
}

#[tokio::test]
async fn a_new_sandbox_is_created_from_its_spec_with_its_egress_policy_and_started() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let egress = policy(&["example.com"]);

    let up = f
        .sandboxes
        .up(&ws, &spec(), &egress, &auth("sk-key"))
        .await
        .unwrap();

    assert!(up.created);
    assert_eq!(up.as_created, Some(spec()));
    assert_eq!(up.setup, Some(Setup::default()));
    assert!(!up.auth_changed);
    let state = f.state();
    assert_eq!(state.created_with, [egress.allow_net()]);
    assert_eq!(state.allowed[&ws.box_name()], egress.allow_net());
    assert!(state.boxes[&ws.box_name()], "the Box is not running");
}

#[tokio::test]
async fn a_stopped_sandbox_is_reattached_started_and_given_the_current_egress_policy() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.up(&ws).await.unwrap();
    f.sandboxes.stop(&ws).await.unwrap();
    let changed = policy(&["example.com"]);

    let up = f
        .sandboxes
        .up(&ws, &spec(), &changed, &auth("sk-key"))
        .await
        .unwrap();

    assert!(!up.created);
    assert_eq!(up.as_created, Some(spec()));
    let state = f.state();
    assert_eq!(state.started, 2);
    assert_eq!(state.allowed[&ws.box_name()], changed.allow_net());
}

#[tokio::test]
async fn a_running_sandbox_is_reattached_without_a_start() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.up(&ws).await.unwrap();

    let up = f.up(&ws).await.unwrap();

    assert!(!up.created);
    assert_eq!(f.state().started, 1);
}

#[tokio::test]
async fn a_sandbox_whose_record_is_lost_is_reattached_with_nothing_known_of_it() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.state().boxes.insert(ws.box_name(), false);

    let up = f.up(&ws).await.unwrap();

    assert!(!up.created);
    assert_eq!(up.as_created, None);
    assert_eq!(up.setup, None);
    assert!(!up.auth_changed);
}

#[tokio::test]
async fn a_sandbox_created_with_another_api_key_says_so() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.up(&ws).await.unwrap();

    let up = f
        .sandboxes
        .up(&ws, &spec(), &policy(&[]), &auth("sk-rotated"))
        .await
        .unwrap();

    assert!(up.auth_changed);
}

#[tokio::test]
async fn a_box_not_ready_to_connect_or_with_a_damaged_image_is_started_again() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.script(&[Start::NotReady, Start::DamagedImage]);

    let up = f.up(&ws).await;

    assert!(up.is_ok(), "{:#}", up.err().unwrap());
    assert_eq!(f.state().started, 3);
}

#[tokio::test]
async fn a_box_is_started_at_most_three_times() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.script(&[Start::NotReady, Start::DamagedImage, Start::NotReady]);

    let failed = f.up(&ws).await.err().unwrap();

    assert_eq!(f.state().started, 3);
    assert_eq!(
        format!("{failed:#}"),
        format!(
            "cannot start the Sandbox for {}: transport error",
            ws.path().display()
        )
    );
}

#[tokio::test]
async fn a_start_that_fails_otherwise_is_not_tried_again() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.script(&[Start::Fails]);

    let failed = f.up(&ws).await.err().unwrap();

    assert_eq!(f.state().started, 1);
    assert_eq!(
        format!("{failed:#}"),
        format!(
            "cannot start the Sandbox for {}: no KVM",
            ws.path().display()
        )
    );
}

#[tokio::test]
async fn a_removed_sandbox_is_created_afresh() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.up(&ws).await.unwrap();

    f.sandboxes.rm(&ws).await.unwrap();
    let up = f
        .sandboxes
        .up(&ws, &spec(), &policy(&[]), &auth("sk-rotated"))
        .await
        .unwrap();

    assert!(up.created);
    assert!(!up.auth_changed);
}

#[tokio::test]
async fn stop_and_rm_refuse_a_workspace_without_a_sandbox() {
    let mut f = Fixture::new();
    let ws = f.workspace();

    let stop = f.sandboxes.stop(&ws).await.unwrap_err();
    let rm = f.sandboxes.rm(&ws).await.unwrap_err();

    assert!(stop.is::<NoSandbox>() && rm.is::<NoSandbox>());
    let expected = format!("{} has no Sandbox", ws.path().display());
    assert_eq!(
        (stop.to_string(), rm.to_string()),
        (expected.clone(), expected)
    );
}

#[tokio::test]
async fn sandboxes_are_listed_by_workspace_path_with_any_whose_record_is_lost_last() {
    let mut f = Fixture::new();
    let (a, b) = (f.workspace(), f.workspace());
    let (running, stopped) = if a.path() < b.path() { (a, b) } else { (b, a) };
    f.up(&stopped).await.unwrap();
    f.sandboxes.stop(&stopped).await.unwrap();
    f.up(&running).await.unwrap();
    f.state().boxes.insert("boxq-orphan".into(), false);
    // Named to sort before both Workspaces' Boxes.
    f.state().boxes.insert("boxq-0-orphan".into(), true);

    let listed = f.sandboxes.list().await.unwrap();

    let row = |ws: Option<&Workspace>, name: &str, running| Listed {
        workspace: ws.map(|ws| ws.path().into()),
        box_name: name.into(),
        running,
    };
    assert_eq!(
        listed,
        [
            row(Some(&running), &running.box_name(), true),
            row(Some(&stopped), &stopped.box_name(), false),
            row(None, "boxq-0-orphan", true),
            row(None, "boxq-orphan", false),
        ]
    );
}

#[tokio::test]
async fn a_failed_setup_hook_runs_again_and_one_that_succeeded_does_not() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let failing = hooks(Some("global"), Some("failing"));
    let fixed = hooks(Some("global"), Some("fixed"));
    f.reply([Reply::Prints(""), Reply::Exits(3, "")]);

    let failed = f.set_up(&ws, &failing).await;
    let retried = f.set_up(&ws, &fixed).await;
    let again = f.set_up(&ws, &fixed).await;
    let up = f.up(&ws).await.unwrap();

    assert_eq!(
        failed,
        Some(Failed {
            source: Source::Workspace,
            status: 3,
            interrupted: false,
        })
    );
    assert_eq!((retried, again), (None, None));
    assert_eq!(f.ran_last_args(), ["global", "failing", "fixed"]);
    let setup = up.setup.unwrap();
    assert!(setup.changes(&fixed).is_empty());
    assert_eq!(setup.changes(&failing), ["Workspace Setup Hook"]);
}

#[tokio::test]
async fn an_interrupted_setup_hook_ends_with_the_status_of_its_signal_and_is_left_to_run_again() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let hooks = hooks(Some("global"), Some("workspace"));
    f.reply([Reply::RunsUntilInterrupted]);

    let up = f.up(&ws).await.unwrap();
    let interrupt = async { libc::SIGINT };
    let failed = setup::run_until(&f.sandboxes, &ws, &up, &hooks, interrupt).await;
    let reattached = f.up(&ws).await.unwrap();

    assert_eq!(
        failed.unwrap(),
        Some(Failed {
            source: Source::Global,
            status: 130,
            interrupted: true,
        })
    );
    assert_eq!(f.ran_last_args(), ["global"]);
    let setup = reattached.setup.unwrap();
    assert!(!setup.global.done && !setup.workspace.done);
}

/// Sandboxes created before boxq kept a setup record must not run a hook they may never need.
#[tokio::test]
async fn a_sandbox_without_a_setup_record_counts_as_set_up() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    f.state().boxes.insert(ws.box_name(), false);

    let failed = f.set_up(&ws, &hooks(Some("exit 1"), None)).await;

    assert_eq!(failed, None);
    assert_eq!(f.ran_last_args(), Vec::<String>::new());
}

#[tokio::test]
async fn an_image_without_claude_code_is_refused_not_installed_into() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let up = f.up(&ws).await.unwrap();
    f.reply([Reply::Exits(1, "")]);

    let checked = boxq::claude::check_installed(&up.target, &ws).await;

    let error = checked.unwrap_err().to_string();
    assert!(
        error.contains("no `claude`") && error.contains("boxq rm"),
        "{error}"
    );
    assert_eq!(f.state().ran, [["sh", "-c", "command -v claude"]]);
}

/// The sync reads the guest's `.claude.json`, then its `.claude/settings.json`, then untars.
#[tokio::test]
async fn the_claude_config_sync_takes_a_missing_guest_file_as_none_and_fails_on_another_status() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let up = f.up(&ws).await.unwrap();
    let host = TempDir::new().unwrap();
    let config = ClaudeConfig::new(host.path(), &host.path().join(".claude.json"));
    f.reply([Reply::Exits(100, ""), Reply::Exits(137, "Killed")]);

    let synced = config.sync(&ws, &up.target).await;

    assert_eq!(
        format!("{:#}", synced.unwrap_err()),
        "cannot read /root/.claude/settings.json in the Sandbox: exited 137: Killed"
    );
}

#[tokio::test]
async fn the_claude_config_sync_fails_when_tar_fails_in_the_sandbox() {
    let mut f = Fixture::new();
    let ws = f.workspace();
    let up = f.up(&ws).await.unwrap();
    let host = TempDir::new().unwrap();
    let config = ClaudeConfig::new(host.path(), &host.path().join(".claude.json"));
    f.reply([
        Reply::Prints("{}"),
        Reply::Exits(100, ""),
        Reply::Exits(2, "tar: short read\n"),
    ]);

    let synced = config.sync(&ws, &up.target).await;

    assert_eq!(
        format!("{:#}", synced.unwrap_err()),
        "cannot copy the Claude config into the Sandbox: tar exited 2: tar: short read"
    );
    let untar = ["tar", "-x", "-f", "-", "-C", "/root"];
    assert_eq!(f.state().ran.last().unwrap(), &untar);
}
