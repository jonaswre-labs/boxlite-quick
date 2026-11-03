//! The Sandbox lifecycle over a fake of the Boxes, so no Box boots here: which Sandboxes `up`
//! creates or reattaches, how it tries a start again, and what `stop`, `rm` and `list` refuse and
//! report. What BoxLite itself does is the real-Box tests' `lifecycle::` group.

use boxq::Workspace;
use boxq::auth::Auth;
use boxq::boxes::{
    Boxes, Chunk, ExecTarget, Found, NewBox, Opened, Output, Runtime, SandboxCommand, StartFailure,
    Streamed,
};
use boxq::config::{BaseUrl, Egress};
use boxq::egress::Policy;
use boxq::sandbox::{Listed, NoSandbox, Sandboxes};
use boxq::setup::Setup;
use boxq::spec::Spec;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use tempfile::TempDir;

/// What the fake does for the next start of a Box.
#[derive(Clone, Copy)]
enum Start {
    Starts,
    NotReady,
    DamagedImage,
    Fails,
}

/// The Boxes, kept in memory.
#[derive(Default)]
struct Fake(Mutex<State>);

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
}

/// The fake, open.
struct Open<'a>(&'a Fake);

/// Where the fake runs commands: nowhere.
struct FakeTarget;

impl ExecTarget for FakeTarget {
    async fn output(&self, _: &SandboxCommand, _: Vec<u8>) -> anyhow::Result<Output> {
        anyhow::bail!("the fake runs no commands")
    }

    async fn stream(
        &self,
        _: &SandboxCommand,
        _: impl FnMut(Chunk),
        _: impl Future<Output = i32>,
    ) -> anyhow::Result<Streamed> {
        anyhow::bail!("the fake runs no commands")
    }

    async fn interactive(&self, _: &SandboxCommand) -> anyhow::Result<i32> {
        anyhow::bail!("the fake runs no commands")
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
        Ok(FakeTarget)
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

    async fn up(&self, ws: &Workspace) -> anyhow::Result<boxq::sandbox::Up<FakeTarget>> {
        self.sandboxes
            .up(ws, &spec(), &policy(&[]), &auth("sk-key"))
            .await
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
