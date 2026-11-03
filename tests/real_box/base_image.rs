//! Claude Code in boxq's Base Image: `image/`, `spec::BASE_IMAGE` and the config keys
//! `src/claude_config.rs` seeds. Both tests unpack the Base Image and build a disk from it, so
//! each takes minutes and needs internet access.

use super::*;
use tokio_util::sync::CancellationToken;

/// What Claude Code's TUI draws in `target`'s Sandbox in its first seconds after `input` is
/// typed, escape codes and all.
async fn claude_screen(target: &BoxLiteTarget, ws: &Workspace, input: &str) -> String {
    let token = CancellationToken::new();
    let claude = target
        .command("claude")
        .arg("--dangerously-skip-permissions")
        .env("IS_SANDBOX", "1")
        .env("TERM", "xterm-256color")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("ANTHROPIC_AUTH_TOKEN", "unused")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1")
        .working_dir(ws.path().to_str().unwrap())
        .tty(true);
    let mut exec = target
        .session()
        .execution()
        .await
        .unwrap()
        .exec(claude, token.clone())
        .await
        .unwrap();
    let stdin = exec.stdin_tx.take().unwrap();
    let mut screen = String::new();
    let mut typed = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, exec.stdout_rx.recv()).await {
        screen.push_str(&chunk);
        // Typed once the prompt is drawn, so the input lands in it.
        if !typed && screen.contains("bypass permissions on") {
            typed = true;
            stdin.send(input.as_bytes().to_vec()).unwrap();
        }
    }
    token.cancel();
    screen
}

/// `screen` without its terminal escape sequences, which split highlighted text.
fn plain(screen: &str) -> String {
    let mut out = String::new();
    let mut chars = screen.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
            Some(']') => while chars.next().is_some_and(|c| c != '\x07') {},
            _ => {}
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM and internet access"]
async fn claude_code_starts_with_the_hosts_skills_and_mcp_servers_and_no_prompts() {
    let mut f = Fixture::new();
    let host = host_claude_config(&f.temp_dir(), &f.workspace);
    let up = f.up(&claude_image()).await;
    // Prompts and their keys change between releases, so this test is what proves the seeded
    // keys still skip them in the release the image ships.
    boxq::claude::check_installed(&up.target, &f.workspace)
        .await
        .unwrap();

    host.sync(&f.workspace, &up.target).await.unwrap();

    let ws = f.workspace.path().display();
    let (mcp, _) = sh(&up.target, &format!("cd {ws} && claude mcp list 2>&1"))
        .await
        .unwrap();
    assert!(
        mcp.contains("docs: docs-mcp") && mcp.contains("db: db-mcp"),
        "{mcp}"
    );
    let screen = claude_screen(&up.target, &f.workspace, "/revi").await;
    assert!(screen.contains("bypass permissions on"), "{screen}");
    // The synced skill, offered as a user-scope command.
    assert!(plain(&screen).contains("Review (user)"), "{screen}");
    // The onboarding theme picker, the trust dialog, the bypass warning and the auto-mode offer.
    for prompt in ["text style", "trust", "I accept", "auto mode"] {
        assert!(
            !screen.contains(prompt),
            "prompted with {prompt:?}: {screen}"
        );
    }
}

/// Pulls boxq's Base Image into an empty home, as a first run does, so it is slow and needs the
/// network.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM and internet access"]
async fn run_on_a_fresh_workspace_starts_claude_code_from_the_pinned_base_image() {
    let f = Fixture::cold();
    let ws = f.workspace.path().to_str().unwrap();

    let mut run = Terminal::boxq(&f, &["run", ws]);
    run.resize(40, 160);
    // The run pulls the Base Image into the fixture's empty home first.
    run.expect_within("bypass permissions on", std::time::Duration::from_secs(600));
    drop(run);
    let up = f.up(IMAGE).await;

    assert_eq!(up.as_created.unwrap().image, boxq::spec::BASE_IMAGE);
}
