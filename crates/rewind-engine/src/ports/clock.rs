use rewind_core::{PartitionId, RunId, Seed, Timestamp};

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

    /// A run id of `partition`, whose 16 bits after the variant hold the partition
    /// ([`RunId::in_partition`]). The 12 bits after its millisecond are the process's number and
    /// its counter within the millisecond, so that its handle's suffix is rarely another run's.
    fn run_id_in(&self, partition: PartitionId) -> RunId;

    fn seed(&self) -> Seed;
}
