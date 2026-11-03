//! The Sandbox lifecycle: one persistent Box per Workspace, found by its derived name
//! (jonaswre-labs/boxlite-quick#10).

use crate::Workspace;
use crate::auth::Auth;
use crate::boxes::{BoxLite, BoxLiteTarget, Boxes, Found, NewBox, Runtime, StartFailure};
use crate::config::read_or_default;
use crate::egress::Policy;
use crate::log;
use crate::setup::Setup;
use crate::spec::Spec;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many times `up` tries to start a Box whose start fails in a way another start may fix
/// (see [`Sandboxes::start`]).
const START_ATTEMPTS: u32 = 3;

/// Every Sandbox under one boxq home, each run by one of `boxes`.
pub struct Sandboxes<B = BoxLite> {
    home: PathBuf,
    boxes: B,
}

/// The Sandbox `up` made ready, where commands run through `T`.
pub struct Up<T = BoxLiteTarget> {
    /// Whether `up` created the Sandbox, rather than reattaching to it.
    pub created: bool,
    pub target: T,
    /// What the Sandbox was created from, or `None` if boxq has no record of it.
    pub as_created: Option<Spec>,
    /// Whether the Sandbox was created with another base URL or API key; `false` if boxq has no
    /// record of which. A Box's secrets and env are fixed at creation, so it keeps those until it
    /// is removed and created afresh.
    pub auth_changed: bool,
    /// How far the Sandbox's Setup Hooks got, or `None` if boxq has no record of them, which
    /// counts as set up.
    pub setup: Option<Setup>,
}

/// One Sandbox as `list` reports it.
#[derive(Debug, PartialEq, Eq)]
pub struct Listed {
    /// `None` for a Box whose record is lost, which the Workspace's next `up` restores.
    pub workspace: Option<PathBuf>,
    pub box_name: String,
    pub running: bool,
}

/// The error for a Workspace that has no Sandbox to stop or remove.
#[derive(Debug)]
pub struct NoSandbox(PathBuf);

impl std::fmt::Display for NoSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} has no Sandbox", self.0.display())
    }
}

impl std::error::Error for NoSandbox {}

/// What boxq keeps about a Sandbox that its Box can't carry.
#[derive(Serialize, Deserialize)]
struct Record {
    workspace: PathBuf,
    /// `None` for a Box created before boxq recorded its Spec, or whose record was lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_created: Option<Spec>,
    /// [`Auth::fingerprint`] of what the Sandbox was created with; `None` like `as_created`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<String>,
    /// `None` like `as_created`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setup: Option<Setup>,
}

impl Sandboxes {
    /// The Sandboxes under the boxq home `home`, whose Boxes BoxLite runs in the home's own
    /// `BOXLITE_HOME`.
    pub fn new(home: &Path) -> Self {
        Self::with_boxes(home, BoxLite::new(&home.join("boxlite")))
    }

    /// The Sandboxes under boxq's home: `BOXQ_HOME`, else `$XDG_DATA_HOME/boxq`, else
    /// `~/.local/share/boxq`. The home holds its own `BOXLITE_HOME`, so boxq never waits on
    /// another BoxLite tool's lock.
    pub fn from_process_env() -> anyhow::Result<Self> {
        let absolute = |key| std::env::var_os(key).filter(|dir| Path::new(dir).is_absolute());
        let home = match (absolute("BOXQ_HOME"), absolute("XDG_DATA_HOME")) {
            (Some(home), _) => PathBuf::from(home),
            (None, Some(data)) => Path::new(&data).join("boxq"),
            (None, None) => {
                let home =
                    std::env::var_os("HOME").context("cannot find boxq's home: HOME is not set")?;
                Path::new(&home).join(".local/share/boxq")
            }
        };
        Ok(Self::new(&home))
    }

    /// The `BOXLITE_HOME` holding the Sandboxes' Boxes and their image cache.
    pub fn boxlite_home(&self) -> PathBuf {
        self.boxes.home().into()
    }
}

impl<B: Boxes> Sandboxes<B> {
    /// The Sandboxes under the boxq home `home`, whose Boxes are `boxes`.
    pub fn with_boxes(home: &Path, boxes: B) -> Self {
        Self {
            home: home.into(),
            boxes,
        }
    }

    pub fn boxes(&self) -> &B {
        &self.boxes
    }

    /// Creates the Workspace's Sandbox from `spec` with `auth`, or reattaches to the one it has,
    /// and makes sure it runs under `egress`. A Sandbox keeps the Spec and auth it was created
    /// with.
    pub async fn up(
        &self,
        ws: &Workspace,
        spec: &Spec,
        egress: &Policy,
        auth: &Auth,
    ) -> anyhow::Result<Up<B::Target>> {
        let rt = self
            .boxes
            .open()
            .with_context(|| failed("open the runtime for", ws))?;
        let name = ws.box_name();
        // The runtime's lock keeps the Box from appearing or vanishing until it is created.
        let found = rt.find(&name).await;
        let record = match found.with_context(|| failed("look up", ws))? {
            Some(_) => self.read_record(&name)?.unwrap_or(Record {
                workspace: ws.path().into(),
                as_created: None,
                auth: None,
                setup: None,
            }),
            None => Record {
                workspace: ws.path().into(),
                as_created: Some(spec.clone()),
                auth: Some(auth.fingerprint()),
                setup: Some(Setup::default()),
            },
        };
        let auth_changed = record
            .auth
            .as_ref()
            .is_some_and(|created| *created != auth.fingerprint());
        // The record goes first, so a Box never exists without one.
        self.write_record(ws, &record)?;
        let new = NewBox {
            spec,
            shared_dirs: &ws.shared_dirs(),
            allow_net: egress.allow_net(),
            auth,
        };
        let opened = rt.get_or_create(&name, &new).await;
        let opened = opened.with_context(|| failed("create", ws))?;
        if !opened.running {
            self.start(&rt, ws).await?;
        }
        // The Global Config or an Egress Approval may have changed since the Box was created.
        rt.allow(&name, egress.allow_net())
            .await
            .with_context(|| failed("apply the Egress Policy to", ws))?;
        Ok(Up {
            created: opened.created,
            target: rt.exec_target(&name).await?,
            as_created: record.as_created,
            auth_changed,
            setup: record.setup,
        })
    }

    /// Starts the Workspace's Box, trying again, up to [`START_ATTEMPTS`] in all, while the Box
    /// fails with a [`StartFailure`] that another start may fix.
    ///
    /// What BoxLite logs during an attempt is held back from [`log::STDERR`]: dropped if the Box
    /// is started again, which boxq says in a line of its own, and written unchanged otherwise.
    /// So a start that recovers shows no failure (jonaswre-labs/boxlite-quick#56). With `BOXQ_LOG`
    /// set and not empty, `boxq` writes BoxLite's events straight to stderr, so every attempt
    /// shows.
    async fn start(&self, rt: &impl Runtime, ws: &Workspace) -> anyhow::Result<()> {
        let mut attempt = 1;
        loop {
            let held = log::STDERR.hold();
            let again = match rt.start(&ws.box_name()).await {
                Ok(()) => return Ok(()),
                Err(StartFailure::NotReady(_)) if attempt < START_ATTEMPTS => {
                    "wasn't ready to connect; starting it again"
                }
                Err(StartFailure::DamagedImage(_)) if attempt < START_ATTEMPTS => {
                    "found part of its image damaged on disk; downloading it again"
                }
                Err(e) => return Err(e.into_error().context(failed("start", ws))),
            };
            held.discard();
            attempt += 1;
            eprintln!(
                "The Sandbox for {} {again} (attempt {attempt} of {START_ATTEMPTS}).",
                ws.path().display()
            );
        }
    }

    /// Records how far the Workspace's Setup Hooks got, keeping the rest of its record.
    pub fn record_setup(&self, ws: &Workspace, setup: &Setup) -> anyhow::Result<()> {
        let record = self.read_record(&ws.box_name())?;
        let record = record.with_context(|| format!("{} has no Sandbox", ws.path().display()))?;
        self.write_record(
            ws,
            &Record {
                setup: Some(setup.clone()),
                ..record
            },
        )
    }

    /// Stops the Workspace's Sandbox, keeping its state.
    pub async fn stop(&self, ws: &Workspace) -> anyhow::Result<()> {
        let rt = self.boxes.open().with_context(|| failed("stop", ws))?;
        self.find(&rt, ws).await?;
        rt.stop(&ws.box_name())
            .await
            .with_context(|| failed("stop", ws))
    }

    /// Removes the Workspace's Sandbox and its state.
    pub async fn rm(&self, ws: &Workspace) -> anyhow::Result<()> {
        let rt = self.boxes.open().with_context(|| failed("remove", ws))?;
        self.find(&rt, ws).await?;
        rt.remove(&ws.box_name())
            .await
            .with_context(|| failed("remove", ws))?;
        let record = self.record_path(&ws.box_name());
        match std::fs::remove_file(&record) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("cannot remove {}", record.display()))
            }
            _ => Ok(()),
        }
    }

    /// Every Sandbox, ordered by Workspace path, with any whose record is lost last.
    pub async fn list(&self) -> anyhow::Result<Vec<Listed>> {
        let rt = self.boxes.open().context("cannot list the Sandboxes")?;
        let mut listed = Vec::new();
        for found in rt.list().await.context("cannot list the Sandboxes")? {
            listed.push(Listed {
                workspace: self.read_record(&found.name)?.map(|r| r.workspace),
                box_name: found.name,
                running: found.running,
            });
        }
        listed.sort_by(|a, b| {
            (a.workspace.is_none(), &a.workspace, &a.box_name).cmp(&(
                b.workspace.is_none(),
                &b.workspace,
                &b.box_name,
            ))
        });
        Ok(listed)
    }

    /// The Workspace's Box, or [`NoSandbox`] if it has none.
    async fn find(&self, rt: &impl Runtime, ws: &Workspace) -> anyhow::Result<Found> {
        let found = rt.find(&ws.box_name()).await;
        found
            .with_context(|| failed("look up", ws))?
            .ok_or_else(|| NoSandbox(ws.path().into()).into())
    }

    fn record_path(&self, box_name: &str) -> PathBuf {
        self.home.join("sandboxes").join(format!("{box_name}.toml"))
    }

    fn write_record(&self, ws: &Workspace, record: &Record) -> anyhow::Result<()> {
        let path = self.record_path(&ws.box_name());
        let toml = toml::to_string(record)
            .with_context(|| format!("cannot record Workspace {}", ws.path().display()))?;
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        // Renamed into place, so a record is never read half-written.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))
    }

    fn read_record(&self, box_name: &str) -> anyhow::Result<Option<Record>> {
        read_or_default(&self.record_path(box_name), |s| toml::from_str(s).map(Some))
    }
}

/// The error context for a failed `step` on the Workspace's Sandbox: "cannot `step` the Sandbox
/// for <path>".
fn failed(step: &str, ws: &Workspace) -> String {
    format!("cannot {step} the Sandbox for {}", ws.path().display())
}
