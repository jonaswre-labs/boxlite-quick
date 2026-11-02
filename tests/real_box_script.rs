//! `scripts/real-box`, the one command that runs the real-Box tests (jonaswre-labs/boxlite-quick#42).
//! A stub `cargo` stands in for the build and the tests, so no Box boots here.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A stub `cargo` on a temporary `PATH`, with its own temporary and target directories.
struct StubCargo {
    tmp: tempfile::TempDir,
}

impl StubCargo {
    /// A stub whose `cargo test --no-run` runs `build` and whose `cargo test` runs `tests`, both
    /// as shell scripts.
    fn new(build: &str, tests: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(
            &cargo,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n*' --no-run '*)\n{build}\n;;\n*)\n{tests}\n;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(tmp.path().join("tmp")).unwrap();
        Self { tmp }
    }

    /// Runs `scripts/real-box args` with a time limit of `limit` seconds.
    fn run(&self, limit: u32, args: &[&str]) -> Output {
        self.run_with(limit, args, &[])
    }

    /// [`StubCargo::run`] with `env` added to the stub's environment.
    fn run_with(&self, limit: u32, args: &[&str], env: &[(&str, &OsStr)]) -> Output {
        let path = format!(
            "{}:{}",
            self.tmp.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/real-box"))
            .args(args)
            .env("PATH", path)
            .env("TMPDIR", self.tmp.path().join("tmp"))
            .env("CARGO_TARGET_DIR", self.tmp.path().join("target"))
            .env("REAL_BOX_TIME_LIMIT", limit.to_string())
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    /// How many files the run left in the temporary directory.
    fn temporary_files(&self) -> usize {
        std::fs::read_dir(self.tmp.path().join("tmp"))
            .unwrap()
            .count()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The build's and the tests' output, which the script copies to the log.
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_passing_run_saves_its_output_to_a_log_and_leaves_no_temporary_files() {
    let cargo = StubCargo::new(
        "echo built",
        "mktemp -d >/dev/null; echo \"test egress::x ... ok $*\"",
    );
    let out = cargo.run(60, &["egress::"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let logs: Vec<_> = std::fs::read_dir(cargo.tmp.path().join("target/real-box"))
        .unwrap()
        .collect();
    let log = std::fs::read_to_string(logs[0].as_ref().unwrap().path()).unwrap();
    assert!(log.contains("built"), "{log}");
    assert!(log.contains("--ignored egress::"), "{log}");
    assert_eq!(cargo.temporary_files(), 0);
    assert!(stderr(&out).contains("passed"), "{}", stderr(&out));
}

#[test]
fn a_run_that_cannot_create_its_directory_stops_before_the_build() {
    let cargo = StubCargo::new("echo build ran >&2", "echo tests ran >&2");
    let missing = cargo.tmp.path().join("missing");
    let out = cargo.run_with(60, &[], &[("TMPDIR", missing.as_os_str())]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot create a directory for the run"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("ran"), "{}", stderr(&out));
}

#[test]
fn a_run_with_less_free_disk_than_it_asks_for_stops_before_the_tests() {
    let cargo = StubCargo::new("exit 0", "echo tests ran >&2");
    let out = cargo.run_with(60, &[], &[("REAL_BOX_MIN_FREE_GB", OsStr::new("1000000"))]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("the tests need 1000000 GB"),
        "{}",
        stderr(&out)
    );
    assert!(!stdout(&out).contains("tests ran"), "{}", stdout(&out));
    assert_eq!(cargo.temporary_files(), 0);
}

#[test]
fn a_run_with_the_free_disk_it_asks_for_runs_the_tests() {
    let cargo = StubCargo::new("exit 0", "echo tests ran >&2");
    let out = cargo.run_with(60, &[], &[("REAL_BOX_MIN_FREE_GB", OsStr::new("0"))]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("tests ran"), "{}", stdout(&out));
}

#[test]
fn a_failing_test_exits_non_zero_and_says_a_test_failed() {
    let cargo = StubCargo::new("exit 0", "echo 'test egress::x ... FAILED'; exit 101");
    let out = cargo.run(60, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("a test failed"), "{}", stderr(&out));
}

#[test]
fn a_failed_build_exits_non_zero_without_running_the_tests() {
    let cargo = StubCargo::new("exit 101", "echo tests ran >&2");
    let out = cargo.run(60, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("the build failed"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("tests ran"), "{}", stderr(&out));
}

#[test]
fn a_build_past_the_time_limit_is_stopped_and_says_the_build_hit_it() {
    let cargo = StubCargo::new("sleep 30", "echo tests ran >&2");
    let out = cargo.run(1, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("the build hit the time limit of 1 s"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("tests ran"), "{}", stderr(&out));
}

#[test]
fn a_run_past_the_time_limit_is_stopped_and_says_the_limit_was_hit() {
    let cargo = StubCargo::new("exit 0", "sleep 30");
    let start = std::time::Instant::now();
    let out = cargo.run(2, &[]);
    assert!(start.elapsed().as_secs() < 10);
    assert_eq!(out.status.code(), Some(124));
    assert!(
        stderr(&out).contains("the tests hit the time limit of 2 s"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_run_that_ignores_the_stop_signal_is_killed_and_says_the_limit_was_hit() {
    let cargo = StubCargo::new("exit 0", "trap '' TERM; sleep 60");
    let start = std::time::Instant::now();
    let out = cargo.run(2, &[]);
    assert!(start.elapsed().as_secs() < 20);
    assert_eq!(out.status.code(), Some(137));
    assert!(
        stderr(&out).contains("the tests hit the time limit of 2 s"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_test_killed_within_the_time_limit_says_a_test_failed_not_that_the_limit_was_hit() {
    // As the kernel's OOM killer would.
    let cargo = StubCargo::new("exit 0", "kill -KILL $$");
    let out = cargo.run(60, &[]);
    assert_eq!(out.status.code(), Some(137));
    assert!(stderr(&out).contains("a test failed"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("time limit"), "{}", stderr(&out));
}

#[test]
fn a_vms_jailer_writing_after_the_vm_dies_leaves_no_temporary_files() {
    // BoxLite's bwrap jailer names the Box's home on its command line and outlives its VM.
    let cargo = StubCargo::new(
        "exit 0",
        "home=$(mktemp -d); cp \"$(command -v sleep)\" \"$home/boxlite-shim\"
         setsid sh -c '\"$0/boxlite-shim\" 600; sleep 0.5; mkdir -p \"$0/exit\"' \"$home\" \
           >/dev/null 2>&1 &
         sleep 30",
    );
    let out = cargo.run(2, &[]);
    assert!(!out.status.success());
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(cargo.temporary_files(), 0);
}

#[test]
fn a_vm_left_by_a_stopped_test_is_killed_with_the_run() {
    // A Box's VM runs a copy of boxlite-shim in the test's home, in its own session.
    let cargo = StubCargo::new(
        "exit 0",
        "home=$(mktemp -d); cp \"$(command -v sleep)\" \"$home/boxlite-shim\"
         setsid \"$home/boxlite-shim\" 600 & echo $! > \"$PIDFILE\"; sleep 30",
    );
    let pidfile = cargo.tmp.path().join("pid");
    let start = std::time::Instant::now();
    let out = cargo.run_with(2, &[], &[("PIDFILE", pidfile.as_os_str())]);
    assert!(start.elapsed().as_secs() < 10);
    assert!(!out.status.success());
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        std::fs::read_link(format!("/proc/{}/exe", pid.trim())).is_err(),
        "the shim {} still runs",
        pid.trim()
    );
}
