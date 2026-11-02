//! The Egress Policy: which outbound destinations a Sandbox may reach (jonaswre-labs/boxlite-quick#13).

use crate::Workspace;
use crate::config::{
    BaseUrl, Config, Egress, HOST_LOOPBACK, opens_host_loopback, read_file, state_dir,
};
use crate::digest::sha256_hex;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

/// What the Agent needs whatever the Global Config says. The Base Image ships Claude Code, so
/// no registry is needed to install it.
const AGENT: &[&str] = &["api.anthropic.com"];

/// The forge and registries a Sandbox reaches unless the Global Config sets `builtins = false`.
/// With its nonessential traffic off, Claude Code itself needs only `registry.npmjs.org` (`npx`
/// MCP servers) and `github.com` (plugin marketplaces) from these (jonaswre-labs/boxlite-quick#3).
const BUILTINS: &[&str] = &[
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "*.crates.io",
    // apt and rustup, so a Setup Hook can use them (jonaswre-labs/boxlite-quick#30).
    "deb.debian.org",
    "security.debian.org",
    "static.rust-lang.org",
    "sh.rustup.rs",
];

/// A Sandbox's `allow_net`. Never empty, since BoxLite reads an empty list as full access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    allow_net: Vec<String>,
}

impl Policy {
    /// The Workspace's policy in boxq's real environment, asking on the terminal for an Egress
    /// Approval whenever the Workspace Config's `allow` entries have changed.
    pub fn from_process_env(ws: &Workspace, config: &Config) -> anyhow::Result<Self> {
        let approved = Approvals::from_process_env()?.ask_if_changed(
            ws,
            &config.egress.workspace_allow,
            ask_on_terminal,
        )?;
        Ok(Self::new(
            &config.egress,
            config.base_url.as_ref(),
            approved,
            resolve,
        ))
    }

    /// The policy from `egress` and the base URL, with the Workspace Config's entries only if
    /// `workspace_approved`. `resolve` gives a hostname's IPv4 addresses on the host.
    pub fn new(
        egress: &Egress,
        base_url: Option<&BaseUrl>,
        workspace_approved: bool,
        resolve: impl Fn(&str) -> Vec<Ipv4Addr>,
    ) -> Self {
        let mut allow_net: Vec<String> = AGENT.iter().map(|&e| e.into()).collect();
        if egress.builtins {
            allow_net.extend(BUILTINS.iter().map(|&e| e.into()));
        }
        let workspace = egress.workspace_allow.iter().filter(|_| workspace_approved);
        for entry in egress.allow.iter().chain(workspace) {
            // A hostname rule alone is refused when the name resolves into CGNAT space, as a
            // tailnet's does, so the exact IP goes next to it.
            if is_hostname(entry) {
                let tailnet = resolve(entry).into_iter().filter(|&ip| in_tailnet(ip));
                allow_net.extend(tailnet.map(|ip| ip.to_string()));
            }
            allow_net.push(entry.clone());
        }
        if let Some(url) = base_url {
            let ips = match url.ipv4() {
                Some(ip) => vec![ip],
                None => {
                    let ips = resolve(url.host());
                    if ips.is_empty() {
                        eprintln!(
                            "warning: cannot resolve the base URL's host {}; allowing it by \
                             name only",
                            url.host()
                        );
                    }
                    allow_net.push(url.host().into());
                    ips
                }
            };
            // An IP rule for host loopback would open it to any hostname that resolves there.
            let ips = ips.into_iter().filter(|&ip| !opens_host_loopback(ip, 32));
            allow_net.extend(ips.map(|ip| ip.to_string()));
        }
        if egress.allow_host_loopback {
            allow_net.push(HOST_LOOPBACK.to_string());
        }
        allow_net.sort();
        allow_net.dedup();
        Self { allow_net }
    }

    /// The sorted, deduplicated entries.
    pub fn allow_net(&self) -> &[String] {
        &self.allow_net
    }
}

/// The Egress Approvals: for each Workspace, a hash of the Workspace Config `allow` entries the
/// user consented to.
pub struct Approvals {
    path: PathBuf,
}

/// The approvals file as written.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalsFile {
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
}

impl Approvals {
    pub fn new(path: &Path) -> Self {
        Self { path: path.into() }
    }

    /// The approvals in boxq's real environment.
    pub fn from_process_env() -> anyhow::Result<Self> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// The approvals at `$XDG_STATE_HOME/boxq/approvals.toml`, falling back to
    /// `~/.local/state/boxq/approvals.toml`, reading the environment through `env`.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        Ok(Self::new(&state_dir(env)?.join("approvals.toml")))
    }

    /// Whether the Workspace's `entries` apply. Unless they are the ones last approved, `ask` is
    /// shown them sorted, and its yes is recorded.
    pub fn ask_if_changed(
        &self,
        ws: &Workspace,
        entries: &[String],
        ask: impl FnOnce(&[String]) -> anyhow::Result<bool>,
    ) -> anyhow::Result<bool> {
        if entries.is_empty() {
            return Ok(true);
        }
        let mut sorted = entries.to_vec();
        sorted.sort();
        sorted.dedup();
        // One entry per line.
        let hash = sha256_hex(&[sorted.join("\n").as_bytes()]);
        let mut file = self.read()?;
        let key = workspace_key(ws);
        if file.workspaces.get(&key) == Some(&hash) {
            return Ok(true);
        }
        if !ask(&sorted)? {
            return Ok(false);
        }
        file.workspaces.insert(key, hash);
        self.write(&file)?;
        Ok(true)
    }

    /// Forgets the Workspace's approval, so its entries are asked about again.
    pub fn revoke(&self, ws: &Workspace) -> anyhow::Result<()> {
        let mut file = self.read()?;
        if file.workspaces.remove(&workspace_key(ws)).is_some() {
            self.write(&file)?;
        }
        Ok(())
    }

    fn read(&self) -> anyhow::Result<ApprovalsFile> {
        read_file(&self.path)
    }

    fn write(&self, file: &ApprovalsFile) -> anyhow::Result<()> {
        let toml = toml::to_string(file)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        let dir = self.path.parent().unwrap();
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        // Renamed into place, so the file is never read half-written.
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot write {}", self.path.display()))
    }
}

fn workspace_key(ws: &Workspace) -> String {
    ws.path().to_string_lossy().into_owned()
}

/// Lists `entries` and asks y/N on the terminal. With no terminal to ask on, the answer is no.
fn ask_on_terminal(entries: &[String]) -> anyhow::Result<bool> {
    let listed = entries.join(", ");
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!(
            "warning: running without the Workspace Config's egress entries ({listed}): they \
             have no Egress Approval, and there is no terminal to ask for one"
        );
        return Ok(false);
    }
    eprintln!("This Workspace's .boxq.toml allows outbound traffic to:");
    for entry in entries {
        eprintln!("  {entry}");
    }
    eprint!("Allow it? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let yes = matches!(answer.trim(), "y" | "Y" | "yes");
    if !yes {
        eprintln!("warning: running without the Workspace Config's egress entries ({listed})");
    }
    Ok(yes)
}

/// The host's IPv4 addresses for `host`, or none if it doesn't resolve.
fn resolve(host: &str) -> Vec<Ipv4Addr> {
    let Ok(addrs) = (host, 0).to_socket_addrs() else {
        return vec![];
    };
    addrs
        .filter_map(|addr| match addr {
            SocketAddr::V4(addr) => Some(*addr.ip()),
            SocketAddr::V6(_) => None,
        })
        .collect()
}

/// Whether `entry` is an exact hostname rule, not a wildcard, IP or CIDR.
fn is_hostname(entry: &str) -> bool {
    !entry.starts_with("*.") && !entry.contains('/') && entry.parse::<Ipv4Addr>().is_err()
}

/// Whether `ip` is in 100.64.0.0/10, the CGNAT range tailnets use.
fn in_tailnet(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && b & 0xc0 == 64
}
