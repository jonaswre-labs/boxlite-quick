//! The Workspace's mounts, including a worktree's common `.git`: `src/workspace.rs` and the
//! mounts in `src/boxes.rs`.

use super::*;

/// Runs host git in `dir`, returning its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=boxq", "-c", "user.email=boxq@example.com"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn git_works_in_a_worktrees_sandbox_as_on_the_host() {
    let mut f = Fixture::new();
    let tmp = f.temp_dir();
    let repo = tmp.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feature", "../feature"],
    );
    let worktree = Workspace::resolve(&tmp.join("feature")).unwrap();

    let up = f
        .sandboxes
        .up(&worktree, &spec(GIT_IMAGE), &default_policy(), &f.auth)
        .await
        .unwrap();
    let script = format!(
        "cd {} && echo hi > note && git add note && \
         git -c user.name=boxq -c user.email=boxq@example.com commit -qm 'from the Sandbox' && \
         git status --short --branch",
        worktree.path().display()
    );
    let ran = sh(&up.target, &script).await.unwrap();

    assert_eq!(ran, ("## feature\n".into(), 0));
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "feature"]),
        "from the Sandbox\n"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("note")).unwrap(),
        "hi\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_workspace_with_more_files_than_boxlites_1024_reads_whole() {
    let f = Fixture::new();
    for n in 0..3000 {
        std::fs::write(f.workspace.path().join(format!("file-{n}")), "x").unwrap();
    }
    let up = f.up(IMAGE).await;
    // The guest keeps the files it has looked up cached, and each one holds a host file open
    // (jonaswre-labs/boxlite-quick#67).
    let script = format!("cd {} && cat file-* | wc -c", f.workspace.path().display());

    assert_eq!(sh(&up.target, &script).await.unwrap(), ("3000\n".into(), 0));
}
