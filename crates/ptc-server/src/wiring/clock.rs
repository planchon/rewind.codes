//! Time: real, or virtual (`ptc server --virtual-time`), kept in the store's `meta` so that a
//! server started again goes on from where its clock stood.

use std::sync::Arc;

use ptc_core::Timestamp;
use ptc_engine::adapters::{SystemClock, VirtualClock};
use ptc_engine::ports::{Clock, VirtualTime};

/// The server's clock.
#[derive(Debug, Clone)]
pub(crate) enum ServerClock {
    System,
    /// Moves only when the worker jumps to the next timer, retry or deadline.
    Virtual(Arc<VirtualClock>),
}

impl ServerClock {
    pub(crate) fn is_virtual(&self) -> bool {
        matches!(self, ServerClock::Virtual(_))
    }
}

impl Clock for ServerClock {
    fn now(&self) -> Timestamp {
        match self {
            ServerClock::System => SystemClock.now(),
            ServerClock::Virtual(clock) => clock.now(),
        }
    }
}

impl VirtualTime for ServerClock {
    fn advance_to(&self, at: Timestamp) {
        if let ServerClock::Virtual(clock) = self {
            clock.advance_to(at);
        }
    }
}
