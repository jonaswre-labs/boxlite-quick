//! `boxq run` launching a stub Claude Code: `src/claude.rs`, the run command in `src/main.rs`
//! and `session::CLAUDE`. An image without `claude` is `tests/sandbox.rs`, over a fake.

use super::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn run_starts_a_stub_claude_in_the_workspace_and_shell_works_beside_it() {
    let f = Fixture::new();
    let up = f.up(IMAGE).await;
    // A stand-in for Claude Code that shows how it was started, then stays interactive.
    let stub = r#"printf '#!/bin/sh\necho "claude:$*:$PWD:$IS_SANDBOX"\nexec sh -l\n' \
        > /usr/local/bin/claude && chmod +x /usr/local/bin/claude"#;
    assert_eq!(sh(&up.target, stub).await.unwrap().1, 0);
    let ws = f.workspace.path().to_str().unwrap();

    let mut run = Terminal::boxq(&f, &["run", ws]);
    run.expect(&format!("claude:--dangerously-skip-permissions:{ws}:1"));
    run.expect_prompt(&f.workspace);
    let mut shell = Terminal::boxq(&f, &["shell", ws]);
    shell.expect_prompt(&f.workspace);
    shell.type_in("echo beside > /root/note; exit\r");
    let shell_exit = shell.exit_code();
    run.type_in("echo \"from:$(cat /root/note)\"\r");
    run.expect("from:beside");
    run.type_in("exit 4\r");

    assert_eq!((shell_exit, run.exit_code()), (0, 4));
}
