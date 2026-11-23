//! Time: real, or virtual (`rewind server --virtual-time`), kept in the control plane's `meta` so
//! that a server started again goes on from where its clock stood.

use std::sync::Arc;

use rewind_core::Timestamp;
use rewind_engine::adapters::{SystemClock, VirtualClock};
use rewind_engine::ports::{Clock, VirtualTime};
use tokio::sync::watch;

/// The server's clock.
#[derive(Debug, Clone)]
pub(crate) enum ServerClock {
    System,
    /// Moves only when the node jumps to the next timer that wakes a run.
    Virtual(Arc<Virtual>),
}

/// A virtual clock, and where it stands for whoever saves it.
#[derive(Debug)]
pub(crate) struct Virtual {
    clock: VirtualClock,
    moved: watch::Sender<Timestamp>,
}

impl ServerClock {
    /// A virtual clock standing at `at`.
    pub(crate) fn virtual_at(at: Timestamp) -> Self {
        ServerClock::Virtual(Arc::new(Virtual {
            clock: VirtualClock::new(at),
            moved: watch::Sender::new(at),
        }))
    }

    pub(crate) fn is_virtual(&self) -> bool {
        matches!(self, ServerClock::Virtual(_))
    }

    /// Where a virtual clock stands, each time it moves.
    pub(crate) fn watch(&self) -> Option<watch::Receiver<Timestamp>> {
        match self {
            ServerClock::System => None,
            ServerClock::Virtual(clock) => Some(clock.moved.subscribe()),
        }
    }
}

impl Clock for ServerClock {
    fn now(&self) -> Timestamp {
        match self {
            ServerClock::System => SystemClock.now(),
            ServerClock::Virtual(clock) => clock.clock.now(),
        }
    }
}

impl VirtualTime for ServerClock {
    fn advance_to(&self, at: Timestamp) {
        if let ServerClock::Virtual(clock) = self {
            clock.clock.advance_to(at);
            clock.moved.send_replace(clock.clock.now());
        }
    }
}
