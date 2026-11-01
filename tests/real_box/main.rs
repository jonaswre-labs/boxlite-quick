//! Boots real Boxes, so it needs Linux x86_64, KVM and BoxLite's host prerequisites. Run with
//! `scripts/real-box` (see `docs/agents/testing.md`); CI runs it on every push.
//!
//! The tests are grouped by area, one module each, so `scripts/real-box <group>::` runs one
//! group. Each module's doc says which code it covers.
//!
//! boxq execs through the Box's own `box.sock` so no runtime (and no `BOXLITE_HOME` lock) is
//! held during a session (jonaswre/boxlite-quick#8). That leans on BoxLite internals: the
//! socket layout, the `BOXLITE_EXECUTOR` env protocol and the container-id directory. These
//! tests drive boxq's own exec target, so any BoxLite bump must pass them.

mod auth;
mod base_image;
mod claude;
mod claude_config;
mod egress;
mod fixture;
mod lifecycle;
mod mounts;
mod session;
mod setup;
mod spec;

use boxlite::runtime::options::BoxliteOptions;
use boxlite::{BoxliteError, BoxliteRuntime};
use boxq::Workspace;
use boxq::auth::Auth;
use boxq::claude_config::ClaudeConfig;
use boxq::config::{BaseUrl, Egress, SetupHooks};
use boxq::egress::Policy;
use boxq::reap::reap_box;
use boxq::sandbox::{ExecTarget, Sandboxes, Up};
use boxq::spec::Spec;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio_util::sync::CancellationToken;

// Docker Hub images come through Google's mirror: every home asks the registry for each tag it
// uses, which in one run is more often than Docker Hub's anonymous limit allows
// (jonaswre/boxlite-quick#43).

/// Small and quick to pull; boxq's real image only matters to the Agent.
const IMAGE: &str = "mirror.gcr.io/library/alpine:3";

/// Alpine with git, for the tests that run git in a Sandbox.
const GIT_IMAGE: &str = "mirror.gcr.io/alpine/git:2.49.1";

/// Alpine whose user is not root, for the tests that boxq runs as root whatever the image says.
const NON_ROOT_IMAGE: &str = "mirror.gcr.io/library/haproxy:3.2-alpine";

/// `image` with boxq's default resources.
fn spec(image: &str) -> Spec {
    Spec {
        image: image.into(),
        cpus: None,
        memory_mib: None,
        disk_gib: None,
    }
}

/// The image the Claude Code tests run in: boxq's Base Image, or `BOXQ_TEST_IMAGE`, which the
/// image workflow sets to the build it is about to publish. It's pulled into [`IMAGE_STORE`]
/// when a test first asks for it, so a fixture's Box downloads none of it, and a run without
/// these tests downloads none of its 581 MB.
fn claude_image() -> String {
    static PULLED: LazyLock<String> = LazyLock::new(|| {
        let image =
            std::env::var("BOXQ_TEST_IMAGE").unwrap_or_else(|_| boxq::spec::BASE_IMAGE.into());
        pull_into_store(&IMAGE_STORE.join("claude"), &[&image]);
        image
    });
    PULLED.clone()
}

/// A key no test sends anywhere real, distinctive enough to grep a guest for.
const FAKE_KEY: &str = "boxq-fake-key-6f1c0e9a7b2d4e58";

/// Auth for tests that never reach the base URL.
fn fake_auth() -> Auth {
    Auth::new(
        &BaseUrl::parse("https://cliproxy.boxq.invalid").unwrap(),
        FAKE_KEY,
    )
}

/// A temporary boxq home with a temporary Workspace. Every Box in the home is removed on drop, so
/// a failing test never leaves a VM running.
struct Fixture {
    sandboxes: Sandboxes,
    auth: Auth,
    workspace: Workspace,
    dirs: Vec<tempfile::TempDir>,
}

impl Fixture {
    /// A fixture whose BoxLite home shares [`IMAGE_STORE`], so its Boxes download no image the
    /// run has downloaded before.
    fn new() -> Self {
        let f = Self::cold();
        share_images(&f.sandboxes.boxlite_home());
        f
    }

    /// A fixture whose BoxLite home starts empty, as on a first run, so its Boxes pull their
    /// images from the registry.
    fn cold() -> Self {
        log_boxlite();
        let home = tempfile::tempdir().unwrap();
        let mut f = Self {
            sandboxes: Sandboxes::new(home.path()),
            auth: fake_auth(),
            workspace: Workspace::resolve(home.path()).unwrap(),
            dirs: vec![home],
        };
        f.workspace = f.another_workspace();
        f
    }

    fn another_workspace(&mut self) -> Workspace {
        Workspace::resolve(&self.temp_dir()).unwrap()
    }

    /// A directory removed with the fixture.
    fn temp_dir(&mut self) -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().into();
        self.dirs.push(dir);
        path
    }

    /// The Workspace's Sandbox from `image`, created or reattached as `boxq run` would.
    async fn up(&self, image: &str) -> Up {
        self.sandboxes
            .up(&self.workspace, &spec(image), &default_policy(), &self.auth)
            .await
            .unwrap()
    }

    /// Another BoxLite runtime on the home, which holds its lock as another process's would.
    fn runtime(&self) -> BoxliteRuntime {
        open_runtime(&self.sandboxes.boxlite_home())
    }
}

/// Prints BoxLite's `error` events, or what `BOXQ_LOG` selects, in the output of the test that
/// caused them, as `boxq` prints them on stderr. BoxLite's error is often only a short sentence,
/// such as "gRPC transport error: transport error"; the step that failed, and why, is in its log
/// (jonaswre/boxlite-quick#50). The first fixture installs it; later calls do nothing.
fn log_boxlite() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::LevelFilter;
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::ERROR.into())
        .with_env_var("BOXQ_LOG")
        .from_env_lossy();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .with_ansi(false)
        .try_init();
}

/// A BoxLite runtime on the BoxLite home `home`.
fn open_runtime(home: &Path) -> BoxliteRuntime {
    BoxliteRuntime::new(BoxliteOptions {
        home_dir: home.into(),
        ..Default::default()
    })
    .unwrap()
}

/// Runs `future` on a fresh tokio runtime in a thread of its own. Code already running in a
/// tokio runtime can't block on it.
fn block_on_own_runtime<F: Future<Output: Send> + Send>(future: F) -> F::Output {
    std::thread::scope(|s| {
        s.spawn(|| tokio::runtime::Runtime::new().unwrap().block_on(future))
            .join()
            .unwrap()
    })
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let home = self.sandboxes.boxlite_home();
        let rt = self.runtime();
        let clean_up = async {
            for b in rt.list_info().await.unwrap_or_default() {
                let _ = rt.remove(b.id.as_ref(), true).await;
                let _ = reap_box(&home, b.id.as_ref());
            }
        };
        // Drop runs inside the test's own tokio runtime.
        block_on_own_runtime(clean_up);
    }
}

/// The BoxLite image directories homes share through [`IMAGE_STORE`]: downloaded blobs and
/// unpacked layers. BoxLite writes these to a temp name and renames them, so homes can share
/// them. `disk-images` is not shared: two homes building one image's disk there at once can
/// swap the file a running Box boots from.
const SHARED_IMAGE_DIRS: [&str; 4] = ["layers", "manifests", "configs", "extracted"];

/// Where a run's homes share their [`SHARED_IMAGE_DIRS`], so the run downloads each image once
/// rather than once per home (jonaswre/boxlite-quick#44). It's removed when the tests exit.
static IMAGE_STORE: LazyLock<PathBuf> = LazyLock::new(|| {
    let store = tempfile::tempdir().unwrap().keep();
    extern "C" fn remove_store() {
        let _ = std::fs::remove_dir_all(&*IMAGE_STORE);
    }
    // SAFETY: `remove_store` is a plain function that is safe to run at exit.
    unsafe { libc::atexit(remove_store) };
    for dir in SHARED_IMAGE_DIRS {
        std::fs::create_dir_all(store.join(dir)).unwrap();
    }
    store
});

/// A BoxLite home that has pulled the small images most tests use into [`IMAGE_STORE`]. A
/// fixture's home starts with a copy of its image index, so a Box finds them without asking the
/// registry for a digest, and falls back to them if the registry turns it away.
static INDEXED_HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let home = IMAGE_STORE.join("indexed");
    pull_into_store(&home, &[IMAGE, GIT_IMAGE, NON_ROOT_IMAGE]);
    home
});

/// How many times [`pull_into_store`] pulls an image whose config BoxLite read back wrong.
const PULL_ATTEMPTS: u32 = 3;

/// Pulls `images` into [`IMAGE_STORE`] through a BoxLite home of their own at `home`.
///
/// BoxLite 0.11 renames a config it has downloaded into place without flushing it, then reads it
/// straight back. If the write is still queued in tokio's blocking pool, BoxLite reads an empty
/// file and the pull fails with "image config digest mismatch" (jonaswre/boxlite-quick#54).
/// A later pull trusts any config that exists, so a bad one is removed before the image is
/// pulled again.
fn pull_into_store(home: &Path, images: &[&str]) {
    link_image_dirs(home);
    // The first test to ask runs in its own tokio runtime. The BoxLite runtime is dropped once
    // the pull ends, closing the home's database for fixtures to copy.
    block_on_own_runtime(async {
        let rt = open_runtime(home);
        for &image in images {
            let mut attempt = 1;
            loop {
                match rt.images().unwrap().pull(image).await {
                    Err(BoxliteError::Storage(e))
                        if attempt < PULL_ATTEMPTS
                            && e.contains("image config digest mismatch") =>
                    {
                        remove_bad_configs();
                        attempt += 1;
                    }
                    pulled => {
                        pulled.unwrap();
                        break;
                    }
                }
            }
        }
    });
}

/// Removes every config in [`IMAGE_STORE`] whose bytes don't match the digest it's named by.
fn remove_bad_configs() {
    for file in std::fs::read_dir(IMAGE_STORE.join("configs")).unwrap() {
        let path = file.unwrap().path();
        // BoxLite names a config `sha256-<hex>.json`, and one it's still downloading otherwise.
        let name = path.file_name().and_then(|n| n.to_str());
        let Some(hex) = name.and_then(|n| n.strip_prefix("sha256-")?.strip_suffix(".json")) else {
            continue;
        };
        if std::fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) != hex) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The SHA-256 of `bytes`, in hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Points the BoxLite home `home`'s [`SHARED_IMAGE_DIRS`] at [`IMAGE_STORE`]'s, before any
/// runtime opens it. This leans on how BoxLite lays out its home, so any BoxLite bump must pass
/// these tests.
fn link_image_dirs(home: &Path) {
    std::fs::create_dir_all(home.join("images")).unwrap();
    for dir in SHARED_IMAGE_DIRS {
        std::os::unix::fs::symlink(IMAGE_STORE.join(dir), home.join("images").join(dir)).unwrap();
    }
}

/// Gives the BoxLite home `home` the run's images: [`IMAGE_STORE`]'s, with a copy of
/// [`INDEXED_HOME`]'s database, which holds the image index.
fn share_images(home: &Path) {
    let indexed = INDEXED_HOME.join("db");
    link_image_dirs(home);
    std::fs::create_dir_all(home.join("db")).unwrap();
    for file in std::fs::read_dir(indexed).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), home.join("db").join(file.file_name())).unwrap();
    }
}

/// The Egress Policy with the Global `allow` entries and the Workspace ones, applied only if
/// `approved`. Names are left unresolved, as no test host is on a tailnet.
fn policy(allow: &[&str], workspace_allow: &[&str], approved: bool) -> Policy {
    let egress = Egress {
        builtins: true,
        allow: allow.iter().map(|&e| e.into()).collect(),
        allow_host_loopback: false,
        workspace_allow: workspace_allow.iter().map(|&e| e.into()).collect(),
    };
    Policy::new(&egress, None, approved, |_| vec![])
}

/// The policy with no config files.
fn default_policy() -> Policy {
    policy(&[], &[], false)
}

/// Runs `sh -c script` through `target` as the image's user, returning its stdout and exit code.
async fn sh(target: &ExecTarget, script: &str) -> anyhow::Result<(String, i32)> {
    exec(target, target.command("sh").args(["-c", script])).await
}

/// [`sh`] as root.
async fn root_sh(target: &ExecTarget, script: &str) -> anyhow::Result<(String, i32)> {
    exec(
        target,
        target.command("sh").args(["-c", script]).user("root"),
    )
    .await
}

async fn exec(target: &ExecTarget, command: boxlite::BoxCommand) -> anyhow::Result<(String, i32)> {
    let token = CancellationToken::new();
    let mut exec = target
        .session()
        .execution()
        .await?
        .exec(command, token.clone())
        .await?;
    drop(exec.stdin_tx);
    let mut stdout = String::new();
    while let Some(chunk) = exec.stdout_rx.recv().await {
        stdout.push_str(&chunk);
    }
    let result = exec
        .result_rx
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("exec ended without a result"))?;
    token.cancel();
    Ok((stdout, result.exit_code))
}

/// A host Claude config with a symlinked skill, settings and MCP servers, as `boxq run` copies it.
fn host_claude_config(dir: &Path, ws: &Workspace) -> ClaudeConfig {
    let skill = dir.join("dotfiles/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# Review\n").unwrap();
    std::fs::create_dir_all(dir.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&skill, dir.join(".claude/skills/review")).unwrap();
    std::fs::write(dir.join(".claude/.credentials.json"), "{}").unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        r#"{"model": "opus", "env": {"ANTHROPIC_API_KEY": "sk-secret"}}"#,
    )
    .unwrap();
    let claude_json = serde_json::json!({
        "mcpServers": {"docs": {"command": "docs-mcp"}},
        "oauthAccount": {"emailAddress": "me@example.com"},
        "projects": {ws.path().to_str().unwrap(): {"mcpServers": {"db": {"command": "db-mcp"}}}},
    });
    std::fs::write(dir.join(".claude.json"), claude_json.to_string()).unwrap();
    ClaudeConfig::new(&dir.join(".claude"), &dir.join(".claude.json"))
}

/// A `boxq` process on its own pseudo-terminal, as a user's terminal runs it.
struct Terminal {
    master: std::fs::File,
    child: std::process::Child,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
    seen: String,
}

impl Terminal {
    /// Runs `boxq args` against the fixture's home on a fresh 24x80 terminal.
    fn boxq(f: &Fixture, args: &[&str]) -> Self {
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
        use std::os::unix::process::CommandExt;

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::ffi::OsStr::from_bytes(
                ptsname(&master, Vec::new()).unwrap().as_bytes(),
            ))
            .unwrap();
        let master = std::fs::File::from(master);
        set_size(&master, 24, 80);
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_boxq"));
        cmd.args(args)
            .env("BOXQ_HOME", f.sandboxes.boxlite_home().parent().unwrap())
            .env("XDG_CONFIG_HOME", f.sandboxes.boxlite_home())
            .env("XDG_STATE_HOME", f.sandboxes.boxlite_home())
            .env("BOXQ_BASE_URL", "https://cliproxy.boxq.invalid")
            .env("BOXQ_API_KEY", FAKE_KEY)
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        // The terminal becomes boxq's controlling terminal, so resizing it sends SIGWINCH.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        // The slave stays open only in boxq, so the reader sees EOF when boxq exits.
        drop(cmd);
        let (tx, output) = std::sync::mpsc::channel();
        let mut reader = master.try_clone().unwrap();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            master,
            child,
            output,
            seen: String::new(),
        }
    }

    fn type_in(&mut self, keys: &str) {
        use std::io::Write;
        self.master.write_all(keys.as_bytes()).unwrap();
    }

    fn resize(&self, rows: u16, cols: u16) {
        set_size(&self.master, rows, cols);
    }

    /// Waits for `text` in the terminal's output, panicking with what it saw after 60 s.
    fn expect(&mut self, text: &str) {
        self.expect_within(text, std::time::Duration::from_secs(60));
    }

    /// Waits for `text` in the terminal's output, panicking with what it saw after `wait`.
    /// Returns what came before `text`.
    fn expect_within(&mut self, text: &str, wait: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + wait;
        while !self.seen.contains(text) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.output.recv_timeout(left) {
                Ok(chunk) => self.seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(_) => panic!("no {text:?} on the terminal; it shows:\n{}", self.seen),
            }
        }
        // Later expectations only look at what comes after.
        let at = self.seen.find(text).unwrap();
        let before = self.seen.drain(..at + text.len()).collect::<String>();
        before[..at].into()
    }

    /// Waits for a login shell's prompt in `ws`: Alpine's `/etc/profile` sets it to `host:cwd# `.
    fn expect_prompt(&mut self, ws: &Workspace) {
        self.expect(&format!(":{}# ", ws.path().display()));
    }

    /// Waits for boxq to exit, returning its exit code.
    fn exit_code(&mut self) -> i32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status
                    .code()
                    .unwrap_or_else(|| panic!("boxq died: {status}"));
            }
            assert!(
                std::time::Instant::now() < deadline,
                "boxq did not exit; the terminal shows:\n{}",
                self.seen
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

fn set_size(master: &std::fs::File, rows: u16, cols: u16) {
    let size = rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(master, size).unwrap();
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
