//! boxq's settings: the Global Config, the credentials file and the Workspace Config, merged
//! into one validated [`Config`] (jonaswre-labs/boxlite-quick#6, ADR 0001).

use crate::Workspace;
use anyhow::Context;
use boxlite::runtime::constants::vm_defaults::MAX_CPUS;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::net::Ipv4Addr;
use std::num::NonZeroU8;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The merged, validated settings for one Workspace.
#[derive(Debug)]
pub struct Config {
    pub base_url: Option<BaseUrl>,
    pub api_key: Option<ApiKey>,
    pub image: Option<String>,
    pub setup: SetupHooks,
    pub resources: Resources,
    pub egress: Egress,
}

/// The Setup Hooks, kept apart because both run, the Global Config's first.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SetupHooks {
    pub global: Option<String>,
    pub workspace: Option<String>,
}

/// Sandbox resources. `None` leaves the choice to boxq's defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpus: Option<NonZeroU8>,
    pub memory: Option<Size>,
    pub disk: Option<Size>,
}

impl Resources {
    /// Each field of `self`, falling back to `base`'s.
    fn or(self, base: Self) -> Self {
        Self {
            cpus: self.cpus.or(base.cpus),
            memory: self.memory.or(base.memory),
            disk: self.disk.or(base.disk),
        }
    }

    /// Refuses resources no Sandbox could be created with. These are sanity limits, not a
    /// trust gate, so a Workspace Config may set resources freely (jonaswre-labs/boxlite-quick#31).
    fn check(&self) -> anyhow::Result<()> {
        if let Some(cpus) = self.cpus {
            // The guest kernel's limit, which BoxLite otherwise checks only at creation.
            anyhow::ensure!(
                cpus.get() <= MAX_CPUS,
                "cpus = {cpus} is more than the {MAX_CPUS} a Sandbox can have"
            );
        }
        if let Some(memory) = self.memory {
            let host_mib = host_ram_mib()?;
            anyhow::ensure!(
                memory.mib() <= host_mib,
                "memory = {}MiB is more than the host's {host_mib}MiB of RAM",
                memory.mib()
            );
        }
        Ok(())
    }
}

/// The host's total RAM, in whole MiB.
fn host_ram_mib() -> anyhow::Result<u64> {
    // SAFETY: an all-zero `sysinfo` is a valid value for sysinfo(2) to fill in.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `sysinfo`.
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read the host's RAM");
    }
    Ok(info.totalram * u64::from(info.mem_unit) / (1024 * 1024))
}

/// A size in whole MiB, written as a whole number of `MiB` or `GiB` (`"512MiB"`, `"8GiB"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Size {
    mib: u64,
}

impl Size {
    pub fn mib(self) -> u64 {
        self.mib
    }
}

impl TryFrom<String> for Size {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let parsed = [("GiB", 1024), ("MiB", 1)]
            .iter()
            .find_map(|&(unit, scale)| {
                let n = s.strip_suffix(unit)?;
                // `u64::from_str` also takes a leading `+`.
                n.bytes().all(|b| b.is_ascii_digit()).then_some(())?;
                n.parse::<u64>().ok()?.checked_mul(scale)
            });
        parsed.map(|mib| Self { mib }).ok_or_else(|| {
            format!("invalid size {s:?}: use a whole number of MiB or GiB, like \"8GiB\"")
        })
    }
}

/// The Egress Policy settings, before the built-ins and base-URL entries are added.
#[derive(Debug)]
pub struct Egress {
    pub builtins: bool,
    /// The Global Config's `allow` entries.
    pub allow: Vec<String>,
    pub allow_host_loopback: bool,
    /// The Workspace Config's `allow` entries, applied only with an Egress Approval.
    pub workspace_allow: Vec<String>,
}

/// The base URL the Agent's API requests go to. Only `https://` on the default port, naming a
/// hostname or IPv4 address and carrying no credentials, is one BoxLite can inject the API key
/// into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    url: String,
    host: String,
}

impl BaseUrl {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let Some(rest) = url.strip_prefix("https://") else {
            anyhow::bail!(
                "base URL {url:?} must start with https://: \
                 boxq can inject the API key only into HTTPS requests"
            );
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        // Not echoed, since the URL would print the credentials.
        anyhow::ensure!(
            !authority.contains('@'),
            "the base URL must not carry credentials: put the API key in boxq's credentials file"
        );
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        anyhow::ensure!(
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
            "base URL {url:?} must name a hostname or IPv4 address"
        );
        // BoxLite 0.11 injects secrets only on port 443, and the map (jonaswre-labs/boxlite-quick#1)
        // puts other ports out of scope ("Plain-HTTP or non-443 base URLs").
        anyhow::ensure!(
            port.is_none(),
            "base URL {url:?} must not name a port: \
             boxq can inject the API key only on HTTPS's default port"
        );
        Ok(Self {
            url: url.into(),
            host: host.to_ascii_lowercase(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The hostname or IPv4 address, lowercased.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The host's IPv4 address, if it names one rather than a hostname.
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.host.parse().ok()
    }
}

/// The API key. Its `Debug` output is redacted so it never lands in a log.
pub struct ApiKey(String);

impl ApiKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl Config {
    /// Load the Workspace's settings from boxq's real environment.
    pub fn from_process_env(workspace: &Workspace) -> anyhow::Result<Self> {
        Self::load(workspace, |key| std::env::var(key).ok())
    }

    /// Load the Workspace's settings, reading the environment through `env`.
    pub fn load(
        workspace: &Workspace,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Self> {
        let dir = config_dir(&env)?;
        let global_path = dir.join("config.toml");
        let global = File::read(&global_path)?;
        let credentials = read_credentials(&dir.join("credentials"))?;
        let ws_path = workspace.path().join(".boxq.toml");
        let ws = File::read(&ws_path)?;
        if let Some(key) = ws.global_only_key() {
            anyhow::bail!(
                "invalid {}: `{key}` can be set only in the Global Config",
                ws_path.display()
            );
        }
        let egress = global.egress.unwrap_or_default();
        let allow = egress.allow.unwrap_or_default();
        let allow_host_loopback = egress.allow_host_loopback.unwrap_or(false);
        let workspace_allow = ws.egress.and_then(|e| e.allow).unwrap_or_default();
        if let Some(entry) = workspace_allow
            .iter()
            .find(|e| entry_opens_host_loopback(e))
        {
            anyhow::bail!(
                "invalid {}: allow entry {entry:?} would open host loopback, which only the \
                 Global Config's `allow_host_loopback` can",
                ws_path.display()
            );
        }
        if let Some(entry) = allow.iter().find(|e| entry_opens_host_loopback(e))
            && !allow_host_loopback
        {
            anyhow::bail!(
                "invalid {}: allow entry {entry:?} would open host loopback; set \
                 `allow_host_loopback = true` to allow that",
                global_path.display()
            );
        }
        Ok(Self {
            // The host's ANTHROPIC_* vars are deliberately never read.
            base_url: env("BOXQ_BASE_URL")
                .or(global.base_url)
                .map(|url| BaseUrl::parse(&url))
                .transpose()?,
            api_key: env("BOXQ_API_KEY").or(credentials.api_key).map(ApiKey),
            image: ws.image.or(global.image),
            setup: SetupHooks {
                global: global.setup,
                workspace: ws.setup,
            },
            resources: ws
                .resources
                .unwrap_or_default()
                .or(global.resources.unwrap_or_default()),
            egress: Egress {
                builtins: egress.builtins.unwrap_or(true),
                allow,
                allow_host_loopback,
                workspace_allow,
            },
        })
    }
}

/// One config file as written. Every key is optional; which keys a file may set is
/// checked after parsing, so the error can say why a key is refused.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    base_url: Option<String>,
    image: Option<String>,
    setup: Option<String>,
    resources: Option<Resources>,
    egress: Option<EgressFile>,
}

impl File {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let file: Self = read_file(path)?;
        let allow = file.egress.iter().flat_map(|e| e.allow.iter().flatten());
        for entry in allow {
            anyhow::ensure!(
                is_allow_entry(entry),
                "invalid {}: allow entry {entry:?} must be a hostname, `*.` wildcard, \
                 IPv4 address or IPv4 CIDR, with no scheme or port",
                path.display()
            );
        }
        if let Some(resources) = &file.resources {
            resources
                .check()
                .with_context(|| format!("invalid {}", path.display()))?;
        }
        Ok(file)
    }

    /// The first key set here that only the Global Config may set (ADR 0001).
    fn global_only_key(&self) -> Option<&'static str> {
        let egress = self.egress.as_ref();
        if self.base_url.is_some() {
            Some("base_url")
        } else if egress.is_some_and(|e| e.builtins.is_some()) {
            Some("egress.builtins")
        } else if egress.is_some_and(|e| e.allow_host_loopback.is_some()) {
            Some("egress.allow_host_loopback")
        } else {
            None
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EgressFile {
    builtins: Option<bool>,
    allow: Option<Vec<String>>,
    allow_host_loopback: Option<bool>,
}

/// Whether `entry` is a rule BoxLite's `allow_net` honours as written. BoxLite 0.11 also
/// takes `host:port` and IPv6, but silently drops the port and never matches IPv6.
fn is_allow_entry(entry: &str) -> bool {
    if let Some((ip, prefix)) = entry.split_once('/') {
        return ip.parse::<Ipv4Addr>().is_ok()
            && !prefix.is_empty()
            && prefix.bytes().all(|b| b.is_ascii_digit())
            && prefix.parse::<u8>().is_ok_and(|p| p <= 32);
    }
    if entry.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let host = entry.strip_prefix("*.").unwrap_or(entry);
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The guest's address for the host's 127.0.0.1 (`host.boxlite.internal`).
pub(crate) const HOST_LOOPBACK: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);

/// Whether the IP or CIDR rule `entry` would open host loopback. Hostnames never do: BoxLite
/// refuses a name that resolves to loopback unless an IP rule covers it.
fn entry_opens_host_loopback(entry: &str) -> bool {
    let (ip, prefix) = entry.split_once('/').unwrap_or((entry, "32"));
    match (ip.parse(), prefix.parse()) {
        (Ok(ip), Ok(prefix)) => opens_host_loopback(ip, prefix),
        _ => false,
    }
}

/// Whether an IP rule for `ip/prefix` would open host loopback: it covers [`HOST_LOOPBACK`], or
/// overlaps 127.0.0.0/8, which a hostname rule that resolves there would then reach.
pub(crate) fn opens_host_loopback(ip: Ipv4Addr, prefix: u8) -> bool {
    let overlaps = |net: Ipv4Addr, bits: u8| {
        let bits = prefix.min(bits);
        let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
        u32::from(ip) & mask == u32::from(net) & mask
    };
    overlaps(HOST_LOOPBACK, 32) || overlaps(Ipv4Addr::LOCALHOST, 8)
}

/// `$XDG_CONFIG_HOME/boxq`, falling back to `~/.config/boxq`.
fn config_dir(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    xdg_dir(env, "XDG_CONFIG_HOME", ".config", "config")
}

/// `$XDG_STATE_HOME/boxq`, falling back to `~/.local/state/boxq`.
pub(crate) fn state_dir(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    xdg_dir(env, "XDG_STATE_HOME", ".local/state", "state")
}

/// `${var}/boxq`, falling back to `~/{fallback}/boxq`; `what` names boxq's use of it.
fn xdg_dir(
    env: impl Fn(&str) -> Option<String>,
    var: &str,
    fallback: &str,
    what: &str,
) -> anyhow::Result<PathBuf> {
    let base = match env(var).filter(|dir| Path::new(dir).is_absolute()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env("HOME")
                .with_context(|| format!("cannot find boxq's {what}: HOME is not set"))?;
            Path::new(&home).join(fallback)
        }
    };
    Ok(base.join("boxq"))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    api_key: Option<String>,
}

/// Read the credentials file, refusing it unless only its owner, the current user, can
/// access it.
fn read_credentials(path: &Path) -> anyhow::Result<Credentials> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Credentials::default()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        meta.uid() == uid,
        "refusing {}: it is not owned by the current user",
        path.display()
    );
    anyhow::ensure!(
        meta.mode() & 0o077 == 0,
        "refusing {}: other users can access it; run `chmod 600 {}`",
        path.display(),
        path.display()
    );
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    // toml's errors quote the offending line or value, which could print the key. Only an
    // unknown-key message is known to hold no value.
    toml::from_str(&text).map_err(|e| {
        let line = e
            .span()
            .map_or(1, |s| text[..s.start].matches('\n').count() + 1);
        let message = match e.message() {
            m if m.starts_with("unknown field") => m,
            _ => "expected `api_key = \"…\"`",
        };
        anyhow::anyhow!("invalid {} at line {line}: {message}", path.display())
    })
}

/// Parse a TOML file, or return an empty one if it doesn't exist.
pub(crate) fn read_file<T: Default + DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    read_or_default(path, |text| toml::from_str(text))
}

/// Parse the file at `path` with `parse`, or return `T::default()` if it doesn't exist.
pub(crate) fn read_or_default<T: Default, E: std::error::Error + Send + Sync + 'static>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> anyhow::Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}
