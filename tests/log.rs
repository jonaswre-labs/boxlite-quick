//! boxq's stderr log, driven by a `tracing` subscriber as `boxq` installs it, with events made up
//! in the test, so no Box boots here.

use boxq::log::Log;
use std::sync::{Arc, Mutex};

/// What a [`Log`] wrote, shared with the test.
#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Out {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `f` with a subscriber writing to `log` as `boxq` writes to stderr.
fn logging_to(log: &Arc<Log<Out>>, f: impl FnOnce()) {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, f);
}

#[test]
fn events_held_for_an_attempt_that_is_discarded_are_never_written() {
    let out = Out::default();
    let log = Arc::new(Log::new(out.clone()));

    logging_to(&log, || {
        let held = log.hold();
        tracing::error!("Task failed: gRPC transport error");
        held.discard();
        tracing::error!("after the attempt");
    });

    assert_eq!(out.text(), "ERROR log: after the attempt\n");
}

#[test]
fn events_held_for_an_attempt_that_is_not_discarded_are_written_unchanged_once_it_ends() {
    let out = Out::default();
    let log = Arc::new(Log::new(out.clone()));
    let mut while_held = String::new();

    logging_to(&log, || {
        let held = log.hold();
        tracing::error!(task = "guest_init", "Task failed: gRPC transport error");
        tracing::error!("Box failed. Diagnostic files preserved");
        while_held = out.text();
        drop(held);
    });

    assert_eq!(while_held, "");
    assert_eq!(
        out.text(),
        "ERROR log: Task failed: gRPC transport error task=\"guest_init\"\n\
         ERROR log: Box failed. Diagnostic files preserved\n"
    );
}

#[test]
fn a_hold_taken_while_another_is_held_loses_nothing_the_first_held() {
    let out = Out::default();
    let log = Arc::new(Log::new(out.clone()));

    logging_to(&log, || {
        let first = log.hold();
        tracing::error!("from the first start");
        let second = log.hold();
        tracing::error!("from the second start");
        drop(first);
        second.discard();
    });

    assert_eq!(
        out.text(),
        "ERROR log: from the first start\nERROR log: from the second start\n"
    );
}
