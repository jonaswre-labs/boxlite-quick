//! The image and resources a Sandbox is created with, and how its Spec is kept: `src/spec.rs`.

use super::*;
use boxq::spec::{DEFAULT_CPUS, DEFAULT_DISK_GIB, DEFAULT_MEMORY_MIB};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_new_sandbox_gets_the_configured_image_and_resources() {
    let f = Fixture::new();
    let spec = Spec {
        cpus: Some(2),
        memory_mib: Some(768),
        disk_gib: Some(3),
        ..spec(GIT_IMAGE)
    };

    let up = f
        .sandboxes
        .up(&f.workspace, &spec, &default_policy(), &f.auth)
        .await
        .unwrap();
    let (out, code) = sh(
        &up.target,
        "nproc; grep MemTotal /proc/meminfo | tr -dc 0-9; echo; df -k / | awk 'NR==2{print $2}'; \
         git --version >/dev/null && echo has-git",
    )
    .await
    .unwrap();

    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let [cpus, mem_kib, disk_kib, git] = lines[..] else {
        panic!("unexpected output: {out:?}");
    };
    let mib = |kib: &str| kib.parse::<u64>().unwrap() / 1024;
    assert_eq!(cpus, "2");
    // The guest kernel keeps some memory for itself.
    assert!(
        (640..=768).contains(&mib(mem_kib)),
        "MemTotal {mem_kib} KiB"
    );
    assert!(
        (2560..=3072).contains(&mib(disk_kib)),
        "disk {disk_kib} KiB"
    );
    assert_eq!(git, "has-git");
}

/// The defaults fill in only the Box: the record keeps the Spec as configured, so a later
/// change to a default never reads as a config change (jonaswre-labs/boxlite-quick#31).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_sandbox_with_no_resources_configured_gets_boxqs_defaults() {
    let f = Fixture::new();

    let up = f.up(IMAGE).await;
    let (out, code) = sh(
        &up.target,
        "nproc; grep MemTotal /proc/meminfo | tr -dc 0-9; echo; df -k / | awk 'NR==2{print $2}'",
    )
    .await
    .unwrap();

    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let [cpus, mem_kib, disk_kib] = lines[..] else {
        panic!("unexpected output: {out:?}");
    };
    let mib = |kib: &str| kib.parse::<u64>().unwrap() / 1024;
    assert_eq!(cpus, DEFAULT_CPUS.to_string());
    // The guest kernel keeps some memory for itself.
    let memory = u64::from(DEFAULT_MEMORY_MIB);
    assert!(
        (memory - 1024..=memory).contains(&mib(mem_kib)),
        "MemTotal {mem_kib} KiB"
    );
    assert!(
        ((DEFAULT_DISK_GIB - 2) * 1024..=DEFAULT_DISK_GIB * 1024).contains(&mib(disk_kib)),
        "disk {disk_kib} KiB"
    );
    assert_eq!(up.as_created, Some(spec(IMAGE)));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Box; needs KVM"]
async fn a_sandbox_keeps_the_spec_it_was_created_with_until_removed() {
    let f = Fixture::new();
    let first = spec(IMAGE);
    let changed = Spec {
        cpus: Some(2),
        ..spec(IMAGE)
    };

    let created = f
        .sandboxes
        .up(&f.workspace, &first, &default_policy(), &f.auth)
        .await
        .unwrap();
    let running = f
        .sandboxes
        .up(&f.workspace, &changed, &default_policy(), &f.auth)
        .await
        .unwrap();
    f.sandboxes.stop(&f.workspace).await.unwrap();
    let restarted = f
        .sandboxes
        .up(&f.workspace, &changed, &default_policy(), &f.auth)
        .await
        .unwrap();
    let (nproc, _) = sh(&restarted.target, "nproc").await.unwrap();
    f.sandboxes.rm(&f.workspace).await.unwrap();
    let recreated = f
        .sandboxes
        .up(&f.workspace, &changed, &default_policy(), &f.auth)
        .await
        .unwrap();

    assert_eq!(created.as_created, Some(first.clone()));
    assert_eq!(running.as_created, Some(first.clone()));
    assert_eq!(restarted.as_created, Some(first));
    assert_eq!(nproc, "4\n");
    assert_eq!(recreated.as_created, Some(changed));
}
