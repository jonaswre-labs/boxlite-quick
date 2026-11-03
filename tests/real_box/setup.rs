//! Setup Hooks in a real Box: `src/setup.rs`. Which hooks run and what their record keeps is
//! `tests/sandbox.rs`, over a fake.

use super::*;
use boxq::setup::{self, Failed, Source};

fn hooks(global: Option<&str>, workspace: Option<&str>) -> SetupHooks {
    SetupHooks {
        global: global.map(Into::into),
        workspace: workspace.map(Into::into),
    }
}

impl Fixture {
    /// Runs the Setup Hooks `up` hasn't finished, with no interrupt.
    async fn set_up(&self, up: &Up, hooks: &SetupHooks) -> Option<Failed> {
        setup::run_until(
            &self.sandboxes,
            &self.workspace,
            up,
            hooks,
            std::future::pending(),
        )
        .await
        .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_setup_hook_runs_as_root_in_the_workspace_with_the_sessions_env_and_no_stdin() {
    let f = Fixture::new();
    let hook = r#"{ pwd; id -u; echo "$IS_SANDBOX"; cat; echo end; } > set-up"#;

    let up = f.up(NON_ROOT_IMAGE).await;
    let failed = f.set_up(&up, &hooks(None, Some(hook))).await;

    assert_eq!(failed, None);
    let out = std::fs::read_to_string(f.workspace.path().join("set-up")).unwrap();
    assert_eq!(
        out,
        format!("{}\n0\n1\nend\n", f.workspace.path().display())
    );
}

/// BoxLite signals only the process it started, so the hook's own children are the test.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn an_interrupted_setup_hook_is_killed_with_its_children_and_left_to_run_again() {
    let f = Fixture::new();
    let hooks = hooks(
        Some("sleep 1000 & sleep 1000"),
        Some("touch /root/workspace-ran"),
    );

    let up = f.up(IMAGE).await;
    let interrupt = async {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        libc::SIGINT
    };
    let failed = setup::run_until(&f.sandboxes, &f.workspace, &up, &hooks, interrupt)
        .await
        .unwrap();
    let (left, _) = root_sh(
        &up.target,
        // A killed child lingers as a zombie, listed as `[sleep]`, since the Box's init never
        // reaps it.
        "ps -o args | grep -c '^sleep 1000'; test -e /root/workspace-ran && echo ran",
    )
    .await
    .unwrap();
    let reattached = f.up(IMAGE).await;

    assert_eq!(
        failed,
        Some(Failed {
            source: Source::Global,
            status: 130,
            interrupted: true,
        })
    );
    // No sleep is left, and the Workspace Setup Hook never ran.
    assert_eq!(left, "0\n");
    let setup = reattached.setup.unwrap();
    assert!(!setup.global.done);
    assert!(!setup.workspace.done);
}
