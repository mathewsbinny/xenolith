//! Panic-safe background job runner (`Task<T>`), modeled after `bc2-editor::task`: every background job runs on a
//! named worker thread with `catch_unwind` so a panic or early thread exit surfaces as a clean error message rather
//! than hanging the UI.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// A background job and its eventual result.
pub struct Task<T> {
    rx: Receiver<Result<T, String>>,
    what: String,
}

impl<T: Send + 'static> Task<T> {
    /// Spawns `body` on a named background thread, catching panics as `Err(String)`.
    #[must_use]
    pub fn spawn(what: &str, body: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let label = what.to_owned();
        let spawned = std::thread::Builder::new()
            .name(format!("xenolith: {what}"))
            .spawn(move || {
                let res = match catch_unwind(AssertUnwindSafe(body)) {
                    Ok(r) => r,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "internal panic".to_owned());
                        Err(format!("{label}: worker panicked ({msg})"))
                    }
                };
                let _ = tx.send(res);
            });
        if let Err(e) = spawned {
            let (tx, rx) = channel();
            let _ = tx.send(Err(format!("{what}: failed to spawn worker thread ({e})")));
            return Self {
                rx,
                what: what.to_owned(),
            };
        }
        Self {
            rx,
            what: what.to_owned(),
        }
    }
}

impl<T> Task<T> {
    /// Non-blocking poll for completion (`None` while still running).
    #[must_use]
    pub fn poll(&self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(format!("{}: worker disconnected unexpectedly", self.what))),
        }
    }

    /// Blocking wait (for unit tests).
    #[cfg(test)]
    pub fn wait(&self) -> Result<T, String> {
        self.rx
            .recv()
            .unwrap_or_else(|_| Err(format!("{}: worker disconnected unexpectedly", self.what)))
    }

    /// Human-readable description of the running job.
    #[must_use]
    pub fn what(&self) -> &str {
        &self.what
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn captures_worker_panic_as_error() {
        let t: Task<()> = Task::spawn("boom", || panic!("simulated failure"));
        let err = t.wait().unwrap_err();
        assert!(err.contains("simulated failure"));
    }
}
