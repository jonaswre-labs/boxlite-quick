//! Creating, reattaching, stopping, removing and listing Sandboxes on real Boxes, waiting on the
//! BoxLite runtime, and starts that are tried again and what they print: `src/boxes.rs`,
//! `src/sandbox.rs`, `src/reap.rs`, `src/log.rs` and `src/image_store.rs`. What `src/sandbox.rs`
//! does over any Boxes is `tests/sandbox.rs`, over a fake.
//!
//! Where a test needs another boxq process holding the BoxLite home, a second runtime in this
//! process stands in for it: BoxLite's home lock is a `flock` on a file each runtime opens for
//! itself, so the two runtimes contend for it as two processes would.

use super::*;
use boxlite::runtime::options::{BoxOptions, RootfsSpec};
use boxq::sandbox::Listed;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_command_runs_in_a_new_sandbox_while_another_runtime_holds_the_home() {
    let f = Fixture::new();

    let up = f.up(IMAGE).await;
    let held = f.runtime();
    let ran = sh(&up.target, "echo hello from the sandbox").await.unwrap();
    drop(held);

    assert!(up.created);
    assert_eq!(ran, ("hello from the sandbox\n".into(), 0));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_stopped_sandbox_is_reattached_with_its_state() {
    let f = Fixture::new();
    let first = f.up(IMAGE).await;
    sh(&first.target, "echo kept > /root/state").await.unwrap();

    f.sandboxes.stop(&f.workspace).await.unwrap();
    let again = f.up(IMAGE).await;

    assert!(!again.created);
    let read = sh(&again.target, "cat /root/state").await.unwrap();
    assert_eq!(read, ("kept\n".into(), 0));
}

/// BoxLite 0.11 leaks the shim and VM on `stop` wherever it can't create the Box's cgroup,
/// which includes GitHub's runners (jonaswre-labs/boxlite-quick#19). Remove `reap_box` once a BoxLite
/// release reaps the tree itself (boxlite-ai/boxlite#1240), and this test still passes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn no_shim_survives_stop_or_rm() {
    let f = Fixture::new();

    f.up(IMAGE).await;
    f.sandboxes.stop(&f.workspace).await.unwrap();
    let after_stop = shims(&f.sandboxes.boxlite_home()).unwrap();
    f.up(IMAGE).await;
    f.sandboxes.rm(&f.workspace).await.unwrap();
    let after_rm = shims(&f.sandboxes.boxlite_home()).unwrap();

    assert_eq!(after_stop, Vec::<PathBuf>::new(), "shims left after stop");
    assert_eq!(after_rm, Vec::<PathBuf>::new(), "shims left after rm");
}

/// The executables of the processes running a shim from under `home`.
fn shims(home: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let boxes = std::fs::canonicalize(home)?.join("boxes");
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        if let Ok(exe) = std::fs::read_link(entry?.path().join("exe"))
            && exe.starts_with(&boxes)
            && exe.to_string_lossy().contains("boxlite-shim")
        {
            found.push(exe);
        }
    }
    Ok(found)
}

/// BoxLite 0.11 can tell the host a Box is ready before libkrun listens on its `box.sock`, so the
/// start fails with "gRPC transport error: transport error" (jonaswre-labs/boxlite-quick#50). A file
/// left where libkrun is about to listen stands in for the listener that isn't there yet.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_box_not_yet_listening_when_it_says_it_is_ready_is_started_again() {
    let f = Fixture::new();
    let blocked = block_box_sock(&f.sandboxes.boxlite_home());

    let up = f.up(IMAGE).await;

    assert!(
        blocked.join().unwrap(),
        "box.sock was not blocked before libkrun listened"
    );
    let ran = sh(&up.target, "echo hello").await.unwrap();
    assert_eq!(ran, ("hello\n".into(), 0));
    assert_eq!(shims(&f.sandboxes.boxlite_home()).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_start_that_succeeds_when_tried_again_prints_only_boxqs_line_about_it() {
    let f = Fixture::new();
    let config = f.sandboxes.boxlite_home().join("boxq");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), format!("image = {IMAGE:?}\n")).unwrap();
    let blocked = block_box_sock(&f.sandboxes.boxlite_home());
    let ws = f.workspace.path();

    let mut t = Terminal::boxq(&f, &["shell", ws.to_str().unwrap()]);
    let wait = std::time::Duration::from_secs(120);
    let before = t.expect_within(&format!("Created the Sandbox for {}", ws.display()), wait);
    t.expect_prompt(&f.workspace);

    assert!(
        blocked.join().unwrap(),
        "box.sock was not blocked before libkrun listened"
    );
    let retried = format!(
        "The Sandbox for {} wasn't ready to connect; starting it again (attempt 2 of 3).\r\n",
        ws.display()
    );
    assert!(before.ends_with(&retried), "no retry line in:\n{before}");
    // Such as "Task failed: gRPC transport error" and "Box failed. Diagnostic files preserved".
    assert!(
        !before.contains("ERROR"),
        "BoxLite's errors from the failed attempt were printed:\n{before}"
    );
}

/// BoxLite 0.11 can rename an image config into its store before the bytes are written, and if
/// they never are, every start of that image reads it back empty (jonaswre-labs/boxlite-quick#60). A
/// cold fixture's store is its own, so its config is this test's to empty.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_sandbox_whose_image_config_was_left_empty_starts() {
    let f = Fixture::cold();
    let digest = {
        let rt = f.runtime();
        let pulled = rt.images().unwrap().pull(IMAGE).await.unwrap();
        pulled.config_digest().to_owned()
    };
    let config = f
        .sandboxes
        .boxlite_home()
        .join("images/configs")
        .join(format!("{}.json", digest.replace(':', "-")));
    std::fs::write(&config, "").unwrap();

    let up = f.up(IMAGE).await;

    let ran = sh(&up.target, "echo hello").await.unwrap();
    assert_eq!(ran, ("hello\n".into(), 0));
    let bytes = std::fs::read(&config).unwrap();
    assert_eq!(format!("sha256:{}", sha256_hex(&[&bytes])), digest);
}

/// Waits for the first Box under `home` to enter its VM, then creates a plain file where libkrun
/// is about to listen on `box.sock`. Joins to whether the file went in before libkrun's socket.
fn block_box_sock(home: &Path) -> std::thread::JoinHandle<bool> {
    let boxes = home.join("boxes");
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if let Some(Ok(b)) = std::fs::read_dir(&boxes).ok().and_then(|mut d| d.next()) {
                // The shim prints this once libkrun has checked the socket paths are free.
                let stderr = std::fs::read_to_string(b.path().join("shim.stderr"));
                if stderr.is_ok_and(|s| s.contains("krun_start_enter")) {
                    let sock = b.path().join("sockets/box.sock");
                    return std::fs::File::create_new(sock).is_ok();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_removed_sandbox_is_created_afresh() {
    let f = Fixture::new();
    let first = f.up(IMAGE).await;
    sh(&first.target, "echo gone > /root/state").await.unwrap();

    f.sandboxes.rm(&f.workspace).await.unwrap();
    let again = f.up(IMAGE).await;

    assert!(again.created);
    let read = sh(&again.target, "test -e /root/state").await.unwrap();
    assert_eq!(read.1, 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn sandboxes_are_listed_by_workspace_path() {
    let mut f = Fixture::new();
    let (a, b) = (f.another_workspace(), f.another_workspace());
    let (running, stopped) = if a.path() < b.path() { (a, b) } else { (b, a) };
    f.sandboxes
        .up(&stopped, &spec(IMAGE), &default_policy(), &f.auth)
        .await
        .unwrap();
    f.sandboxes.stop(&stopped).await.unwrap();
    f.sandboxes
        .up(&running, &spec(IMAGE), &default_policy(), &f.auth)
        .await
        .unwrap();
    // A Box boxq has no record of, as if its record were lost.
    let options = BoxOptions {
        rootfs: RootfsSpec::Image(IMAGE.into()),
        auto_delete: Some(0),
        detach: true,
        ..Default::default()
    };
    let rt = f.runtime();
    rt.create(options, Some("boxq-orphan".into()))
        .await
        .unwrap();
    drop(rt);

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
            row(None, "boxq-orphan", false),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn up_waits_for_another_runtime_to_release_the_home() {
    let f = Fixture::new();
    let held = f.runtime();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(held);
    });

    let up = f.up(IMAGE).await;
    release.join().unwrap();

    assert!(up.created);
}
