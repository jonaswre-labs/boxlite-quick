use anyhow::Context;
use boxlite::runtime::options::BoxliteOptions;
use boxlite::{BoxliteError, BoxliteRuntime};
use std::path::Path;
use std::time::Duration;

/// How often to retry a busy `BOXLITE_HOME` lock.
const POLL: Duration = Duration::from_millis(100);

/// The one place boxq opens the BoxLite runtime, which takes the `home` lock.
///
/// Callers hold the runtime only long enough to find, create, start, stop, remove or list a
/// Box, then drop it before any long-lived work (jonaswre-labs/boxlite-quick#8). Another boxq doing
/// the same holds it for seconds at most (a cold create), so a busy lock is waited out, not
/// reported.
pub fn open(home: &Path) -> anyhow::Result<BoxliteRuntime> {
    let mut announced = false;
    loop {
        match BoxliteRuntime::new(BoxliteOptions {
            home_dir: home.into(),
            ..Default::default()
        }) {
            // BoxLite 0.11 reports a busy lock only through this message, wrapped in another.
            Err(BoxliteError::Internal(msg))
                if msg.contains("Another BoxliteRuntime is already") =>
            {
                if !announced {
                    eprintln!("Waiting for another boxq command to finish...");
                    announced = true;
                }
                std::thread::sleep(POLL);
            }
            opened => {
                return opened.with_context(|| {
                    format!("cannot open BoxLite's runtime in {}", home.display())
                });
            }
        }
    }
}
