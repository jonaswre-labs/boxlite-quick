# Testing

`cargo test --locked` runs the tests that need no KVM. Among them, `tests/sandbox.rs` runs the Sandbox lifecycle in `src/sandbox.rs` (creating, reattaching, starting again, stopping, removing, listing) over a fake of the Boxes in `src/boxes.rs`, so a change there gets a test that runs in milliseconds before it gets a real-Box one. The real-Box tests in `tests/real_box/` boot real Sandboxes, so they're `#[ignore]`d. Run them with `scripts/real-box`, in the foreground: it ends within 10 minutes, so you get the result in the same command. Don't push a run into the background, and don't pass `--test-threads=1`. Every test has its own BoxLite home, so the default thread count is safe.

## `scripts/real-box`

```sh
scripts/real-box [libtest args]   # e.g. scripts/real-box egress:: auth::
```

It builds the tests, then runs `cargo test --locked --test real_box -- --ignored [libtest args]` at the default thread count. The arguments are test filters such as `egress::`, or `--skip` and `--exact`. The build and the tests share one time limit of 540 s (`REAL_BOX_TIME_LIMIT` seconds). The output also goes to `target/real-box/<timestamp>.log`.

A full run writes about 10 GB to its temporary directory. With `REAL_BOX_MIN_FREE_GB` set, the script runs the tests only if that many GB are free there after the build. On a fuller disk, tests fail in ways that look like boxq's bugs, such as `No space left on device` from `mke2fs`, and then the run can hang (jonaswre-labs/boxlite-quick#53).

The last line, starting `real-box:`, says what happened. The exit status is 0 if every test passed. It is non-zero if the build failed, there wasn't enough free disk (1), a test failed or the time limit was hit (124, or 137 if a test had to be killed). After the time limit, the script kills the VMs that stopped tests left running and removes their homes. A cold build can use up the time limit. If it does, run the script again: the build carries on from where it stopped.

## What to run

Run the groups your change touches. Each group is one module in `tests/real_box/`, and each module's doc comment names the `src/` files it covers. Filter by `<group>::`:

| group | covers |
| --- | --- |
| `lifecycle::` | `src/boxes.rs`, `src/sandbox.rs`, `src/reap.rs`, `src/log.rs`, `src/image_store.rs` |
| `mounts::` | `src/workspace.rs` and the mounts in `src/boxes.rs` |
| `spec::` | `src/spec.rs` |
| `egress::` | `src/egress.rs` |
| `auth::` | `src/auth.rs` |
| `claude_config::` | `src/claude_config.rs` |
| `session::` | `src/session.rs`, `boxq shell` |
| `claude::` | `src/claude.rs`, `boxq run` |
| `setup::` | `src/setup.rs` |
| `base_image::` | `image/`, `spec::BASE_IMAGE` (see below) |
| `fixture::` | the tests' own `Fixture` and its shared images (see below) |

When you change the CLI, also run `cargo test --locked --test cli -- --ignored`, which takes a few seconds.

## Images

A run downloads each image once and shares it among the tests' BoxLite homes (`Fixture::new`). Each home still builds its own image disk. The small images are pulled at the start, in a few seconds. The 581 MB Base Image is pulled only once a test asks for `claude_image()`. A test that must pull into an empty home, as a first `boxq run` does, uses `Fixture::cold()`. `fixture::` tests the sharing itself.

## The Base Image tests

Each of the two `base_image::` tests takes about 2 minutes. The cold-start one downloads the Base Image itself, and the other waits for the run's shared copy. Both then unpack it and build a disk from it. Together they take a full run on a 16-core host from about 30 s to about 2 minutes, well within the time limit. So run the whole suite when your change touches the image (`image/`, `spec::BASE_IMAGE`) or how Claude Code is launched (`src/claude.rs`, `src/claude_config.rs`). Otherwise `scripts/real-box --skip base_image::` saves those 2 minutes. `scripts/real-box base_image::` runs them alone.

## CI

CI's `real-box` job runs the full suite, the Base Image tests included, on every push. It builds the tests, then runs `scripts/real-box` with `REAL_BOX_MIN_FREE_GB=20`, so a stuck run ends at the time limit with its VMs killed. A runner can have as little as 10 GB of free disk. So before the build, both this job and `real-box-stress.yml` delete the runner's toolchains boxq doesn't use (`.github/actions/free-disk`), which frees about 32 GB. The job's log shows `df -h /` before and after.

## A failure only CI shows

A failing real-Box test prints BoxLite's `error` events: the failed task (`task=guest_init`) and any crash report. BoxLite's own error is often only a sentence such as `gRPC transport error: transport error`. `BOXQ_LOG=boxlite=debug,tonic=trace` prints more, but to failing tests only.

Loading this host didn't reproduce jonaswre-labs/boxlite-quick#50: starving the CPU (`taskset -c 0`, many test threads) only gives `timed out after 30s waiting for it to become ready`. GitHub's runners differ in disk latency and free disk, so build the loop for a CI-only failure on the runners:

```sh
gh workflow run real-box-stress.yml -f jobs=20 -f runs=3 [-f args='lifecycle::'] [-f log='boxlite=debug'] [-f strace=true]
gh run download <run-id> -n logs-<job> -D <dir>   # works once that job ends
```

`gh run view --log` waits for the whole run, but each job's artifact can be downloaded as soon as that job ends. `strace=true` records bind, listen and connect calls with their errno, which is how jonaswre-labs/boxlite-quick#50's missing `box.sock` was found.

BoxLite's guest agent and its vendored libkrun aren't in `~/.cargo/registry`. Read them at the pinned release:

```sh
gh api 'repos/boxlite-ai/boxlite/contents/src/guest/src/main.rs?ref=v0.11.0' --jq .content | base64 -d
gh api 'repos/boxlite-ai/boxlite/contents/src/deps/libkrun-sys/vendor?ref=v0.11.0' --jq '.[] | select(.name == "libkrun") | .sha'
gh api 'repos/boxlite-ai/libkrun/contents/src/devices/src/virtio/vsock/muxer.rs?ref=<sha>' --jq .content | base64 -d
```
