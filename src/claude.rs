//! Claude Code in the Sandbox. boxq never installs it: it comes with the image, as with the
//! Base Image (jonaswre-labs/boxlite-quick#29), so any image with `claude` behaves the same.

use crate::Workspace;
use crate::sandbox::{ExecTarget, OutputChunks};
use crate::spec::recreate_hint;
use anyhow::Context;
use tokio_util::sync::CancellationToken;

/// Fails, saying what to do, unless the Sandbox for `ws` has `claude` on its PATH.
pub async fn check_installed(target: &ExecTarget, ws: &Workspace) -> anyhow::Result<()> {
    let token = CancellationToken::new();
    let command = target
        .command("sh")
        .args(["-c", "command -v claude"])
        .user("root");
    let mut exec = target
        .session()
        .execution()
        .await?
        .exec(command, token.clone())
        .await?;
    drop(exec.stdin_tx);
    // Read only to let the command finish; what it prints doesn't matter.
    let mut output = OutputChunks::new(exec.stdout_rx, exec.stderr_rx);
    while output.next().await.is_some() {}
    let result = exec.result_rx.recv().await;
    token.cancel();
    let result = result.context("the command ended without an exit status")?;
    if let Some(message) = result.error_message {
        anyhow::bail!("the command did not end normally: {message}");
    }
    if result.exit_code == 0 {
        return Ok(());
    }
    anyhow::bail!(
        "the Sandbox for {} has no `claude` on its PATH, as its image ships without Claude Code; \
         to use an image with it, such as boxq's Base Image, {}",
        ws.path().display(),
        recreate_hint(ws.path())
    )
}
