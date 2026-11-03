//! The Egress Policy applied to a Sandbox: `src/egress.rs`. Needs internet access.

use super::*;

/// Whether an HTTPS GET of `url` from the Sandbox succeeds.
async fn reaches(target: &BoxLiteTarget, url: &str) -> bool {
    let script = format!("wget -q -T 15 -O /dev/null {url}");
    sh(target, &script).await.unwrap().1 == 0
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM and internet access"]
async fn listed_hosts_are_reachable_and_unlisted_ones_refused() {
    let f = Fixture::new();
    let policy = policy(&["example.com"], &["example.org"], false);

    let up = f
        .sandboxes
        .up(&f.workspace, &spec(IMAGE), &policy, &f.auth)
        .await
        .unwrap();

    assert!(
        reaches(&up.target, "https://example.com").await,
        "a Global entry"
    );
    assert!(
        reaches(&up.target, "https://registry.npmjs.org/").await,
        "a built-in entry"
    );
    assert!(
        !reaches(&up.target, "https://example.net").await,
        "an unlisted host"
    );
    assert!(
        !reaches(&up.target, "https://example.org").await,
        "an unapproved Workspace entry"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM and internet access"]
async fn a_running_sandbox_gets_the_policy_of_each_up() {
    let f = Fixture::new();
    let first = f
        .sandboxes
        .up(
            &f.workspace,
            &spec(IMAGE),
            &policy(&["example.com"], &["example.org"], false),
            &f.auth,
        )
        .await
        .unwrap();
    let refused_before = !reaches(&first.target, "https://example.org").await;

    let again = f
        .sandboxes
        .up(
            &f.workspace,
            &spec(IMAGE),
            &policy(&[], &["example.org"], true),
            &f.auth,
        )
        .await
        .unwrap();

    assert!(refused_before);
    assert!(!again.created);
    assert!(
        reaches(&again.target, "https://example.org").await,
        "approved since"
    );
    assert!(
        !reaches(&again.target, "https://example.com").await,
        "dropped since"
    );
}
