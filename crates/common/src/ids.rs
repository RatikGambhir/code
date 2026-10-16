//! Strongly typed identifiers, so a terminal id cannot be passed where a
//! browser tab id is expected.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

macro_rules! counter_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u64);

        impl $name {
            /// Allocates a process-unique id.
            pub fn next() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                Self(NEXT.fetch_add(1, Ordering::Relaxed))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

counter_id!(
    /// One embedded browser tab.
    BrowserTabId
);
counter_id!(
    /// One interactive terminal session.
    TerminalId
);

/// Row id of a workspace in the application database.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub i64);

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_ids_are_unique_and_increasing() {
        let a = BrowserTabId::next();
        let b = BrowserTabId::next();
        assert!(b > a);
        assert_ne!(TerminalId::next(), TerminalId::next());
    }
}
