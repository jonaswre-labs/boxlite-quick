//! The curated, credential-free subset of the host's Claude config that every `boxq run` copies
//! into the Sandbox (jonaswre-labs/boxlite-quick#12).
//!
//! A sync merges rather than replaces: what the host has overwrites the Sandbox's copy, and
//! nothing is ever deleted, so Claude Code's own state in the Sandbox survives. Something removed
//! on the host stays in the Sandbox until `boxq rm`.

use crate::Workspace;
use crate::config::read_or_default;
use crate::sandbox::{Chunk, ExecTarget, OutputChunks};
use anyhow::Context;
use serde_json::{Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// The host's Claude config: its config dir (`~/.claude`) and its `.claude.json`.
pub struct ClaudeConfig {
    dir: PathBuf,
    json: PathBuf,
}

/// A file a sync writes into the Sandbox, at `path` relative to the guest's home.
#[derive(Debug)]
pub struct GuestFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
    pub mode: u32,
}

impl ClaudeConfig {
    pub fn new(dir: &Path, json: &Path) -> Self {
        Self {
            dir: dir.into(),
            json: json.into(),
        }
    }

    /// The host's Claude config where Claude Code looks for it: `CLAUDE_CONFIG_DIR`, holding
    /// `.claude.json` too, else `~/.claude` and `~/.claude.json`.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        if let Some(dir) = env("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
            let dir = PathBuf::from(dir);
            return Ok(Self::new(&dir, &dir.join(".claude.json")));
        }
        let home = PathBuf::from(
            env("HOME").context("cannot find the host's Claude config: HOME is not set")?,
        );
        Ok(Self::new(&home.join(".claude"), &home.join(".claude.json")))
    }

    pub fn from_process_env() -> anyhow::Result<Self> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// Copies the host's Claude config into the Sandbox `target` runs in, merging it with the
    /// Sandbox's own.
    pub async fn sync(&self, ws: &Workspace, target: &ExecTarget) -> anyhow::Result<()> {
        let guest_json = read_guest(target, ".claude.json").await?;
        let guest_settings = read_guest(target, ".claude/settings.json").await?;
        let files = self.guest_files(ws, guest_json.as_deref(), guest_settings.as_deref())?;
        let untar = ["tar", "-x", "-f", "-", "-C", GUEST_HOME];
        let untarred = run(target, &untar, tar(&files)?).await?;
        anyhow::ensure!(
            untarred.code == 0,
            "cannot copy the Claude config into the Sandbox: tar exited {}: {}",
            untarred.code,
            untarred.stderr.trim_end()
        );
        Ok(())
    }

    /// The files to write into the Sandbox's home, given the guest's own `.claude.json` and
    /// `.claude/settings.json`, if it has them.
    pub fn guest_files(
        &self,
        ws: &Workspace,
        guest_json: Option<&str>,
        guest_settings: Option<&str>,
    ) -> anyhow::Result<Vec<GuestFile>> {
        let mut files = Vec::new();
        for name in COPIED {
            collect_files(
                &self.dir.join(name),
                &guest_dir().join(name),
                &mut Vec::new(),
                &mut files,
            )?;
        }
        files.push(self.settings_json(guest_settings)?);
        files.push(self.claude_json(ws, guest_json)?);
        Ok(files)
    }

    /// The guest's `settings.json` with the host's merged in, less the keys that hold
    /// credentials or run host programs, and with bypass mode on without asking, since bypass
    /// mode is the point of a Sandbox.
    fn settings_json(&self, guest: Option<&str>) -> anyhow::Result<GuestFile> {
        let path = guest_dir().join("settings.json");
        let mut settings = parse_guest(&path, guest)?;
        let mut host = read_json(&self.dir.join("settings.json"))?;
        for key in UNSYNCED_SETTINGS {
            host.remove(key);
        }
        settings.extend(host);
        settings.insert("skipDangerousModePermissionPrompt".into(), true.into());
        object_at(&mut settings, "permissions")
            .insert("defaultMode".into(), "bypassPermissions".into());
        Ok(json_file(path, settings))
    }

    /// The guest's `.claude.json` with the host's MCP servers for this Workspace merged in, and
    /// the first-run prompts and the Workspace's trust dialog done. The rest of the host's file
    /// is its login and history, and never leaves the host.
    fn claude_json(&self, ws: &Workspace, guest: Option<&str>) -> anyhow::Result<GuestFile> {
        let path = PathBuf::from(".claude.json");
        let mut claude_json = parse_guest(&path, guest)?;
        let host = read_json(&self.json)?;
        if let Some(servers) = host.get("mcpServers") {
            claude_json.insert("mcpServers".into(), servers.clone());
        }
        claude_json.insert("hasCompletedOnboarding".into(), true.into());
        // Claude Code offers to swap the seeded bypass default for auto mode until this is set.
        claude_json.insert("hasSeenAutoDefaultNudge".into(), true.into());
        // Trust is keyed on the git root, which for a worktree is its main checkout's.
        for root in std::iter::once(ws.path().into()).chain(ws.main_checkout()) {
            let root = root.to_string_lossy();
            let project = object_at(object_at(&mut claude_json, "projects"), &root);
            let host_project = host.get("projects").and_then(|p| p.get(&*root));
            for key in SYNCED_PROJECT_KEYS {
                if let Some(value) = host_project.and_then(|p| p.get(key)) {
                    project.insert(key.into(), value.clone());
                }
            }
            project.insert("hasTrustDialogAccepted".into(), true.into());
        }
        Ok(json_file(path, claude_json))
    }
}

/// Host settings that never reach the Sandbox: `env` and the credential helpers can hold
/// secrets; hooks and the status line run host programs; plugins record host install paths.
const UNSYNCED_SETTINGS: [&str; 10] = [
    "env",
    "apiKeyHelper",
    "awsCredentialExport",
    "awsAuthRefresh",
    "gcpAuthRefresh",
    "otelHeadersHelper",
    "hooks",
    "statusLine",
    "enabledPlugins",
    "extraKnownMarketplaces",
];

/// A project's keys in `.claude.json` that are synced: its local-scope MCP servers, and which
/// of its `.mcp.json` servers are on.
const SYNCED_PROJECT_KEYS: [&str; 3] = [
    "mcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
];

/// The JSON object in the host file at `path`, or an empty one if there is no file.
fn read_json(path: &Path) -> anyhow::Result<Map<String, Value>> {
    read_or_default(path, |text| serde_json::from_str(text))
}

/// The object at `key` in `map`, made one if it is anything else.
fn object_at<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let value = map.entry(key).or_insert(Value::Null);
    if !value.is_object() {
        *value = Map::new().into();
    }
    value.as_object_mut().unwrap()
}

/// The JSON object in the guest's own file at `path`, if it has one.
fn parse_guest(path: &Path, text: Option<&str>) -> anyhow::Result<Map<String, Value>> {
    match text {
        Some(text) => serde_json::from_str(text)
            .with_context(|| format!("invalid ~/{} in the Sandbox", path.display())),
        None => Ok(Map::new()),
    }
}

/// A JSON file only the guest's user can read, since MCP servers can hold secrets.
fn json_file(path: PathBuf, value: Map<String, Value>) -> GuestFile {
    let mut contents = serde_json::to_vec_pretty(&value).unwrap();
    contents.push(b'\n');
    GuestFile {
        path,
        contents,
        mode: 0o600,
    }
}

/// What is copied from the host's Claude config dir. Everything else there is credentials,
/// history, caches or host-path state, and never leaves the host (jonaswre-labs/boxlite-quick#3).
const COPIED: [&str; 6] = [
    "CLAUDE.md",
    "rules",
    "skills",
    "agents",
    "commands",
    "output-styles",
];

/// Entries skipped inside the copied directories: Claude's deleted skills, and skills synced
/// from claude.ai, which are tied to the host's login.
fn skipped(src: &Path) -> bool {
    src.ends_with(".trash") || src.ends_with("skills/synced")
}

/// The guest's Claude config dir, relative to its home.
fn guest_dir() -> PathBuf {
    PathBuf::from(".claude")
}

/// Adds `src` to `files` at `dest`, following symlinks. Whatever doesn't exist is skipped, as is
/// a symlink back to a directory being copied, which `ancestors` holds.
fn collect_files(
    src: &Path,
    dest: &Path,
    ancestors: &mut Vec<PathBuf>,
    files: &mut Vec<GuestFile>,
) -> anyhow::Result<()> {
    let Ok(meta) = std::fs::metadata(src) else {
        return Ok(());
    };
    if skipped(src) {
        return Ok(());
    }
    if meta.is_dir() {
        let real = std::fs::canonicalize(src)
            .with_context(|| format!("cannot resolve {}", src.display()))?;
        if ancestors.contains(&real) {
            return Ok(());
        }
        ancestors.push(real);
        let entries =
            std::fs::read_dir(src).with_context(|| format!("cannot list {}", src.display()))?;
        for entry in entries {
            let name = entry?.file_name();
            collect_files(&src.join(&name), &dest.join(&name), ancestors, files)?;
        }
        ancestors.pop();
    } else if meta.is_file() {
        files.push(GuestFile {
            path: dest.into(),
            contents: std::fs::read(src)
                .with_context(|| format!("cannot read {}", src.display()))?,
            mode: meta.permissions().mode() & 0o777,
        });
    }
    Ok(())
}

/// The guest's home. The Agent runs as root in the Sandbox (jonaswre-labs/boxlite-quick#5).
const GUEST_HOME: &str = "/root";

/// The guest's own file at `path` under its home, if it has one.
async fn read_guest(target: &ExecTarget, path: &str) -> anyhow::Result<Option<String>> {
    let path = format!("{GUEST_HOME}/{path}");
    let script = r#"test -e "$1" || exit 100; cat "$1""#;
    let read = run(target, &["sh", "-c", script, "sh", &path], Vec::new()).await?;
    match read.code {
        0 => Ok(Some(read.stdout)),
        100 => Ok(None),
        code => anyhow::bail!(
            "cannot read {path} in the Sandbox: exited {code}: {}",
            read.stderr.trim_end()
        ),
    }
}

/// What a command run in the Sandbox printed, and how it exited.
struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Runs `argv` as root in the Sandbox with `stdin` as its input.
async fn run(target: &ExecTarget, argv: &[&str], stdin: Vec<u8>) -> anyhow::Result<Output> {
    let token = CancellationToken::new();
    let _stop = token.clone().drop_guard();
    let command = target
        .command(argv[0])
        .args(argv[1..].iter().copied())
        .user("root");
    let mut exec = target
        .session()
        .execution()
        .await?
        .exec(command, token)
        .await
        .with_context(|| format!("cannot run {} in the Sandbox", argv[0]))?;
    // Chunked, because BoxLite's guest refuses a stdin message over 4 MiB. Dropping the sender
    // closes the command's stdin.
    if let Some(tx) = exec.stdin_tx.take() {
        for chunk in stdin.chunks(64 << 10) {
            let _ = tx.send(chunk.to_vec());
        }
    }
    let (mut stdout, mut stderr) = (String::new(), String::new());
    let mut output = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
    while let Some(chunk) = output.next().await {
        match chunk {
            Chunk::Stdout(text) => stdout.push_str(&text),
            Chunk::Stderr(text) => stderr.push_str(&text),
        }
    }
    let result = exec
        .result_rx
        .recv()
        .await
        .with_context(|| format!("{} in the Sandbox ended without a result", argv[0]))?;
    Ok(Output {
        stdout,
        stderr,
        code: result.exit_code,
    })
}

/// `files` as a tar archive, owned by root and dated now, with their parent directories.
fn tar(files: &[GuestFile]) -> anyhow::Result<Vec<u8>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let mut archive = tar::Builder::new(Vec::new());
    let mut dirs = std::collections::BTreeSet::new();
    for file in files {
        dirs.extend(
            file.path
                .ancestors()
                .skip(1)
                .filter(|d| !d.as_os_str().is_empty()),
        );
    }
    for dir in dirs {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_mtime(now);
        header.set_size(0);
        archive.append_data(&mut header, dir, std::io::empty())?;
    }
    for file in files {
        let mut header = tar::Header::new_gnu();
        header.set_mode(file.mode);
        header.set_mtime(now);
        header.set_size(file.contents.len() as u64);
        archive.append_data(&mut header, &file.path, &*file.contents)?;
    }
    Ok(archive.into_inner()?)
}
