use rewind_core::{RunId, Seed, Timestamp};

/// The current time. Every timestamp the engine writes comes from here.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// A clock the driver may move forward: in virtual time (tests, mock mode), the clock jumps
/// straight to the next timer, so a 10-day sleep takes no wall-clock time.
pub trait VirtualTime: Clock {
    /// Moves the clock to `at`. A clock never goes backwards: an earlier `at` is ignored.
    fn advance_to(&self, at: Timestamp);
}

/// Run ids and seeds.
pub trait Entropy: Send + Sync {
    fn run_id(&self) -> RunId;
    fn seed(&self) -> Seed;
}
