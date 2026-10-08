//! A cheap clonable cancellation token: a shared flag plus an optional
//! deadline, checked between files and batches (docs/specs/library.md §3.4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::error::{Error, ErrorKind};

#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>, Option<Instant>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancelled once `d` has passed (or when `cancel` is called).
    pub fn with_deadline(d: Instant) -> Self {
        Cancel(Arc::default(), Some(d))
    }

    /// Cancel this token and every clone of it.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed) || self.1.is_some_and(|d| Instant::now() >= d)
    }

    /// `Err(ErrorKind::Cancelled)` once cancelled.
    pub fn check(&self) -> Result<(), Error> {
        match self.is_cancelled() {
            true => Err(Error::new(ErrorKind::Cancelled, "cancelled")),
            false => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn flag_and_deadline() {
        let c = Cancel::new();
        let d = c.clone();
        assert!(c.check().is_ok());
        d.cancel();
        assert_eq!(c.check().unwrap_err().kind(), ErrorKind::Cancelled);

        let past = Cancel::with_deadline(Instant::now());
        assert!(past.is_cancelled());
        let future = Cancel::with_deadline(Instant::now() + Duration::from_secs(3600));
        assert!(!future.is_cancelled());
    }
}
