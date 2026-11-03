//! Claude Code in the Sandbox. boxq never installs it: it comes with the image, as with the
//! Base Image (jonaswre-labs/boxlite-quick#29), so any image with `claude` behaves the same.

use crate::Workspace;
use crate::boxes::{ExecTarget, SandboxCommand};
use crate::spec::recreate_hint;
use anyhow::Context;

/// Fails, saying what to do, unless the Sandbox for `ws` has `claude` on its PATH.
pub async fn check_installed(target: &impl ExecTarget, ws: &Workspace) -> anyhow::Result<()> {
    let command = SandboxCommand::new(&["sh", "-c", "command -v claude"]).as_root();
    let checked = target.output(&command, Vec::new()).await.with_context(|| {
        format!(
            "cannot look for `claude` in the Sandbox for {}",
            ws.path().display()
        )
    })?;
    if checked.status == 0 {
        return Ok(());
    }
    anyhow::bail!(
        "the Sandbox for {} has no `claude` on its PATH, as its image ships without Claude Code; \
         to use an image with it, such as boxq's Base Image, {}",
        ws.path().display(),
        recreate_hint(ws.path())
    )
}
