//! What a Sandbox is created from: its image and resources (jonaswre-labs/boxlite-quick#18). These
//! are fixed once the Sandbox exists, so a later config change only applies to a new one.

use crate::config::Resources;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// boxq's Base Image, which a Sandbox is created from when the config sets none. Pinned by
/// digest, so a cached copy is used with no registry round trip; bumped by hand to a build the
/// image workflow published (jonaswre-labs/boxlite-quick#29).
pub const BASE_IMAGE: &str = "ghcr.io/jonaswre/boxq:2.1.312-247cf26@sha256:55a29d246f63c1788b39fec42d4067c11decb5a8843200566abc07abf3165709";

/// What a Sandbox gets for a resource no config sets (jonaswre-labs/boxlite-quick#31). Filled in only
/// when the Box is built, so changing one never makes an existing Sandbox look out of date.
pub const DEFAULT_CPUS: u8 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 8 * 1024;
pub const DEFAULT_DISK_GIB: u64 = 32;

/// The image and resources to create a Sandbox with, as configured. A `None` resource gets
/// boxq's default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub disk_gib: Option<u64>,
}

impl Spec {
    /// The Spec for the config's `image` and `[resources]`.
    pub fn new(image: Option<&str>, resources: &Resources) -> anyhow::Result<Self> {
        let memory_mib = resources
            .memory
            .map(|m| {
                u32::try_from(m.mib()).map_err(|_| {
                    anyhow::anyhow!("memory {}MiB is more than a Sandbox can have", m.mib())
                })
            })
            .transpose()?;
        Ok(Self {
            image: image.unwrap_or(BASE_IMAGE).into(),
            cpus: resources.cpus.map(|n| n.get()),
            memory_mib,
            // BoxLite sizes a disk in whole GiB; rounding up never gives less than asked for.
            disk_gib: resources.disk.map(|d| d.mib().div_ceil(1024)),
        })
    }

    /// The vCPUs to build the Box with: the configured count, else boxq's default.
    pub fn cpus_or_default(&self) -> u8 {
        self.cpus.unwrap_or(DEFAULT_CPUS)
    }

    /// The memory to build the Box with: the configured MiB, else boxq's default.
    pub fn memory_mib_or_default(&self) -> u32 {
        self.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB)
    }

    /// The disk to build the Box with: the configured GiB, else boxq's default.
    pub fn disk_gib_or_default(&self) -> u64 {
        self.disk_gib.unwrap_or(DEFAULT_DISK_GIB)
    }

    /// The names of this Spec's settings that the Sandbox, created from `created`, lacks, for
    /// [`mismatch_warning`].
    pub fn changes(&self, created: &Spec) -> Vec<&'static str> {
        [
            ("image", self.image != created.image),
            ("cpus", self.cpus != created.cpus),
            ("memory", self.memory_mib != created.memory_mib),
            ("disk", self.disk_gib != created.disk_gib),
        ]
        .into_iter()
        .filter_map(|(name, changed)| changed.then_some(name))
        .collect()
    }
}

/// The one line telling the user that the Sandbox for `workspace` was created before the
/// `changed` settings changed in the config, or `None` if none did. boxq never rebuilds a
/// Sandbox on its own, since that discards its state.
pub fn mismatch_warning(workspace: &Path, changed: &[&str]) -> Option<String> {
    let (last, rest) = changed.split_last()?;
    let names = match rest {
        [] => last.to_string(),
        rest => format!("{} and {last}", rest.join(", ")),
    };
    Some(format!(
        "The Sandbox for {} was created before its {names} changed in the config; to apply \
         them, {}.",
        workspace.display(),
        recreate_hint(workspace)
    ))
}

/// How to get the Sandbox for `workspace` created afresh, to take what it was created from.
pub fn recreate_hint(workspace: &Path) -> String {
    let ws = workspace.display();
    format!("run `boxq rm {ws}` and then `boxq run {ws}`, which discards the Sandbox's state")
}
