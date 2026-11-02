use anyhow::Context;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a shim gets to flush its disk after SIGTERM, as in boxlite-ai/boxlite#1240.
const GRACE: Duration = Duration::from_secs(4);
/// How long a SIGKILLed shim gets to disappear before reaping fails.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// Ends whatever still runs a Box's VM: every process whose executable is the Box's own copy of
/// `boxlite-shim` under BoxLite's `home`. Each gets SIGTERM, which makes the shim flush its disk,
/// then SIGKILL if it hasn't exited within [`GRACE`]. Returns once they have all exited.
///
/// BoxLite 0.11 reaps a detached Box's VM only through its cgroup, so wherever that cgroup can't
/// be created (system services, SSH sessions, CI, WSL) `stop` and `rm` leave the VM running
/// (jonaswre-labs/boxlite-quick#19). Call this after `stop`, after `rm`, and before every start, so a
/// leaked VM never shares a disk with a new one. Remove it once a BoxLite release reaps the tree
/// itself (boxlite-ai/boxlite#1240) and the real-Box test passes without it.
pub fn reap_box(home: &Path, box_id: &str) -> anyhow::Result<()> {
    // The kernel reports a process's executable by its canonical path.
    let home = std::fs::canonicalize(home)
        .with_context(|| format!("cannot resolve BoxLite home {}", home.display()))?;
    let shim = home.join("boxes").join(box_id).join("bin/boxlite-shim");
    let pidfds = running(&shim)?;
    // ESRCH only means it already exited, here and below.
    for pidfd in &pidfds {
        let _ = pidfd_send_signal(pidfd, Signal::TERM);
    }
    let deadline = Instant::now() + GRACE;
    for pidfd in &pidfds {
        if !exits_by(pidfd, deadline)? {
            let _ = pidfd_send_signal(pidfd, Signal::KILL);
            anyhow::ensure!(
                exits_by(pidfd, Instant::now() + KILL_GRACE)?,
                "{} survived SIGKILL",
                shim.display()
            );
        }
    }
    Ok(())
}

/// Waits until the process behind `pidfd` exits, or `deadline` passes.
fn exits_by(pidfd: &OwnedFd, deadline: Instant) -> anyhow::Result<bool> {
    loop {
        let left = Timespec::try_from(deadline.saturating_duration_since(Instant::now()))?;
        match poll(&mut [PollFd::new(pidfd, PollFlags::IN)], Some(&left)) {
            Ok(ready) => return Ok(ready > 0),
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e).context("cannot wait for a shim to exit"),
        }
    }
}

/// A pidfd for every process whose executable is `exe`.
fn running(exe: &Path) -> anyhow::Result<Vec<OwnedFd>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").context("cannot list /proc")? {
        let Some(pid) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Some(pid) = Pid::from_raw(pid) else {
            continue;
        };
        if !runs(pid, exe) {
            continue;
        }
        let Ok(pidfd) = pidfd_open(pid, PidfdFlags::empty()) else {
            continue;
        };
        // Check again now the pidfd pins the process, so a recycled pid is never signalled.
        if runs(pid, exe) {
            found.push(pidfd);
        }
    }
    Ok(found)
}

/// Whether `pid` runs `exe`, even if `exe` has since been deleted. Another user's process, or
/// one that has exited, can't be read, and is never ours.
fn runs(pid: Pid, exe: &Path) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|link| {
        let link = link.as_os_str().as_bytes();
        link.strip_suffix(b" (deleted)").unwrap_or(link) == exe.as_os_str().as_bytes()
    })
}
