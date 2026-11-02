//! BoxLite's image store: the images a BoxLite home has downloaded, under its `images/`.

use crate::digest::sha256_hex;
use anyhow::Context;
use boxlite::BoxliteError;
use std::path::Path;

/// Whether `e` is BoxLite finding a config in the image store whose bytes don't match its digest.
///
/// BoxLite 0.11 renames a config it has downloaded into place without flushing it, then reads it
/// straight back. If the write is still queued in tokio's blocking pool, BoxLite reads an empty
/// file (jonaswre-labs/boxlite-quick#54). If the write never lands, the empty file stays, and every
/// later pull of that image fails the same way, since BoxLite trusts any config that exists
/// (jonaswre-labs/boxlite-quick#60). [`remove_bad_configs`] makes the next pull download it again.
/// BoxLite says so only in the message of a `Storage` error.
pub fn is_config_mismatch(e: &BoxliteError) -> bool {
    matches!(e, BoxliteError::Storage(msg) if msg.contains("image config digest mismatch"))
}

/// Removes every config in the BoxLite home `home`'s image store whose bytes don't match the
/// digest it's named by.
pub fn remove_bad_configs(home: &Path) -> anyhow::Result<()> {
    let dir = home.join("images/configs");
    let files =
        std::fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))?;
    for file in files {
        let path = file
            .with_context(|| format!("cannot list {}", dir.display()))?
            .path();
        // BoxLite names a config `sha256-<hex>.json`, and one it's still downloading otherwise.
        let name = path.file_name().and_then(|n| n.to_str());
        let Some(hex) = name.and_then(|n| n.strip_prefix("sha256-")?.strip_suffix(".json")) else {
            continue;
        };
        // One that can't be read is gone already, or not BoxLite's to read either.
        let bad = std::fs::read(&path).is_ok_and(|bytes| sha256_hex(&[&bytes]) != hex);
        match bad.then(|| std::fs::remove_file(&path)) {
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("cannot remove {}", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}
