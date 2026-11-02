//! boxq's log of BoxLite's events on stderr, which can hold back what a Box start attempt logs
//! until boxq knows whether it will start the Box again (jonaswre-labs/boxlite-quick#56).

use std::io::Write;
use std::sync::{LazyLock, Mutex};

/// The log `boxq` writes BoxLite's events to: stderr.
pub static STDERR: LazyLock<Log<std::io::Stderr>> = LazyLock::new(|| Log::new(std::io::stderr()));

/// A writer for log lines that passes them on to `out`, except while held.
pub struct Log<W> {
    state: Mutex<State<W>>,
}

struct State<W> {
    out: W,
    /// What was written while held; `None` when not held.
    held: Option<Vec<u8>>,
}

impl<W: Write> Log<W> {
    pub fn new(out: W) -> Self {
        Self {
            state: Mutex::new(State { out, held: None }),
        }
    }

    /// Holds what is written until the returned [`Hold`] ends. The hold is on the whole log, not
    /// on one task, since BoxLite logs a start from tasks of its own. `boxq` starts one Box at a
    /// time; where a process starts several at once, as the real-Box tests do, their holds share
    /// what is held, and the first to end writes or discards it for all.
    pub fn hold(&self) -> Hold<'_, W> {
        self.state().held.get_or_insert_with(Vec::new);
        Hold { log: self }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State<W>> {
        // A panic while writing a log line leaves nothing half-done worth refusing.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W: Write> Write for &Log<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self.state();
        match &mut state.held {
            Some(held) => held.write(buf),
            None => state.out.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.state().out.flush()
    }
}

/// A hold on a [`Log`]. Dropped, it writes what it held; [`Hold::discard`] drops that instead.
pub struct Hold<'a, W: Write> {
    log: &'a Log<W>,
}

impl<W: Write> Hold<'_, W> {
    /// Ends the hold, never writing what it held.
    pub fn discard(self) {
        // Dropping `self` then finds nothing held, so writes nothing.
        self.log.state().held = None;
    }
}

impl<W: Write> Drop for Hold<'_, W> {
    fn drop(&mut self) {
        let mut state = self.log.state();
        if let Some(held) = state.held.take() {
            // As a log line that can't be written is lost, so is what was held.
            let _ = state.out.write_all(&held);
        }
    }
}
