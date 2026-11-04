//! Start-up timing, for the "window ready" line in the log.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

static START: OnceLock<Instant> = OnceLock::new();

/// Records the start of the process. Call first thing in `main`.
pub fn mark() {
    START.get_or_init(Instant::now);
}

/// Time since [`mark`] (zero if it was never called).
pub fn elapsed() -> Duration {
    START.get().map(Instant::elapsed).unwrap_or_default()
}
