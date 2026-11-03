//! The Boxes behind the Sandboxes, and [`BoxLite`], the adapter that runs them on BoxLite 0.11
//! and owns what that release needs worked around: its home lock, the VMs it leaks, and the
//! start failures that are worth trying again.

use crate::auth::Auth;
use crate::image_store::{is_config_mismatch, remove_bad_configs};
use crate::reap::reap_box;
use crate::sandbox::ExecTarget;
use crate::spec::Spec;
use anyhow::Context;
use boxlite::runtime::options::{
    BoxOptions, BoxliteOptions, NetworkMode, NetworkSpec, OutboundNetworkConfig, RootfsSpec,
    VolumeSpec,
};
use boxlite::{BoxStatus, BoxliteError, BoxliteRuntime, LiteBox};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// How often to retry a busy `BOXLITE_HOME` lock.
const POLL: Duration = Duration::from_millis(100);

/// Where the Boxes live.
pub trait Boxes {
    /// The Boxes, open for one command's worth of finding, creating, starting, stopping,
    /// removing or listing. Only one [`Runtime`] is open at a time, across every boxq process:
    /// opening waits for any other to close. Callers drop it before any long-lived work
    /// (jonaswre-labs/boxlite-quick#8); a command runs in a Box through its [`ExecTarget`]
    /// with no runtime open.
    fn open(&self) -> anyhow::Result<impl Runtime + '_>;
}

/// The Boxes, open. Each Box is named by its Workspace's [`crate::Workspace::box_name`]; every
/// method but `find`, `get_or_create` and `list` expects the Box to exist.
#[allow(async_fn_in_trait)]
pub trait Runtime {
    async fn find(&self, name: &str) -> anyhow::Result<Option<Found>>;

    /// The Box named `name`, created from `new` if there is none. A created Box isn't running.
    async fn get_or_create(&self, name: &str, new: &NewBox<'_>) -> anyhow::Result<Opened>;

    /// Tries once to start the Box, after ending any VM an earlier stop or failed start left
    /// running, so two never share its disk.
    async fn start(&self, name: &str) -> Result<(), StartFailure>;

    /// Makes the Box's egress `allow_net`, as an Egress Policy gives it.
    async fn allow(&self, name: &str, allow_net: &[String]) -> anyhow::Result<()>;

    /// Where a command runs in the running Box.
    async fn exec_target(&self, name: &str) -> anyhow::Result<ExecTarget>;

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
    /// Opens BoxLite's runtime, which takes the home's lock. Another boxq holds it for seconds
    /// at most (a cold create), so a busy lock is waited out, not reported.
    fn open(&self) -> anyhow::Result<impl Runtime + '_> {
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

    async fn exec_target(&self, name: &str) -> anyhow::Result<ExecTarget> {
        let b = self.get(name).await?;
        let box_dir = self.home.join("boxes").join(b.id().as_ref());
        Ok(ExecTarget::new(
            box_dir.join("sockets/box.sock"),
            container_id(&box_dir)?,
        ))
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
