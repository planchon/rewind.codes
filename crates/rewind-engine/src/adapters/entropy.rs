use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_core::{PartitionId, RunId, Seed, Timestamp};

use crate::ports::Entropy;

/// Run ids sortable by creation time, and seeds from the operating system's CSPRNG.
///
/// The 12 bits after an id's millisecond are this process's node number (6 bits, drawn at random
/// when it makes its first id) and a counter of its ids within that millisecond (6 bits; a 65th
/// id moves to the next millisecond, as does a clock that steps back). Two ids of one process
/// therefore never share their first 60 bits, which a run's handle encodes
/// (`archi/06-storage-and-scale.md`, "Run handles"); the owner of a partition lengthens the
/// handle of the rare run that shares them with another run of its namespace.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEntropy;

/// The millisecond of the last id this process made, and how many it made in it.
struct Sequence {
    ms: u64,
    count: u16,
}

const NODE_BITS: u16 = 6;
const COUNTER_MAX: u16 = (1 << 6) - 1;
/// The random bits of a run id of a partition after its partition: what [`RunId::in_partition`]
/// takes below the 12 bits of the node and its counter.
const RAND_B_BITS: u32 = 46;

fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system's random generator is available");
    u64::from_le_bytes(bytes)
}

impl SystemEntropy {
    /// This process's node number, from 0 to 63.
    fn node() -> u16 {
        static NODE: OnceLock<u16> = OnceLock::new();
        *NODE.get_or_init(|| (random_u64() & ((1 << NODE_BITS) - 1)) as u16)
    }

    /// The millisecond and the counter of the next id.
    fn next_slot() -> (u64, u16) {
        static SEQUENCE: Mutex<Sequence> = Mutex::new(Sequence { ms: 0, count: 0 });
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut sequence = SEQUENCE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now > sequence.ms {
            sequence.ms = now;
            sequence.count = 0;
        } else if sequence.count < COUNTER_MAX {
            sequence.count += 1;
        } else {
            sequence.ms += 1;
            sequence.count = 0;
        }
        (sequence.ms, sequence.count)
    }
}

impl Entropy for SystemEntropy {
    fn run_id(&self) -> RunId {
        let (ms, count) = Self::next_slot();
        RunId::from_v7_parts(ms, (Self::node() << NODE_BITS) | count, random_u64())
    }

    /// The millisecond and the counter of [`SystemEntropy::run_id`], the node number, the
    /// partition, and random bits from the operating system's CSPRNG.
    fn run_id_in(&self, partition: PartitionId) -> RunId {
        let (ms, count) = Self::next_slot();
        let rand_a = u64::from((Self::node() << NODE_BITS) | count);
        RunId::in_partition(
            partition,
            Timestamp::from_millis(ms),
            (rand_a << RAND_B_BITS) | (random_u64() & ((1 << RAND_B_BITS) - 1)),
        )
    }

    fn seed(&self) -> Seed {
        let mut bytes = [0u8; 32];
        // Without a CSPRNG, seeds would be predictable, and so would `crypto` in scripts.
        getrandom::fill(&mut bytes).expect("the operating system's random generator is available");
        Seed::from_bytes(bytes)
    }
}

/// Deterministic run ids and seeds, derived from a base and a counter: tests and recorded
/// scenarios get the same ids and seeds on every run.
#[derive(Debug)]
pub struct SeededEntropy {
    base: u64,
    counter: AtomicU64,
    /// The millisecond and the random bits every run id of a partition shares, but the last 8.
    shared: Option<(Timestamp, u64)>,
    /// The run ids of partitions drawn so far.
    drawn: AtomicU64,
}

impl SeededEntropy {
    pub fn new(base: u64) -> Self {
        Self {
            base,
            counter: AtomicU64::new(0),
            shared: None,
            drawn: AtomicU64::new(0),
        }
    }

    /// Run ids of partitions that share all their bits but the last 8: their millisecond and
    /// the 12 bits after it, and so the 12 characters of their handle's suffix, and 23 in all
    /// (`archi/06-storage-and-scale.md`, "Run handles"). The first id's last 8 bits are 0, the
    /// next ones' `0x40`, `0x41`, …: the second and the third share 24 characters, which the
    /// first does not. What two processes rarely draw, made certain for a test.
    pub fn sharing_suffixes(base: u64) -> Self {
        let mut entropy = Self::new(base);
        let at = Timestamp::from_millis(entropy.next() >> 16);
        entropy.shared = Some((at, entropy.next()));
        entropy
    }

    fn next(&self) -> u64 {
        splitmix64(self.base ^ self.counter.fetch_add(1, Ordering::SeqCst).rotate_left(32))
    }
}

impl Entropy for SeededEntropy {
    /// A version 7 UUID whose time and random bits come from the counter.
    fn run_id(&self) -> RunId {
        let bits = (u128::from(self.next()) << 64) | u128::from(self.next());
        let uuid = uuid::Builder::from_u128(bits)
            .with_variant(uuid::Variant::RFC4122)
            .with_version(uuid::Version::SortRand)
            .into_uuid();
        RunId::from_uuid(uuid)
    }

    /// A run id of the partition whose time and random bits come from the counter, but those
    /// the ids share when they share their suffixes.
    fn run_id_in(&self, partition: PartitionId) -> RunId {
        match self.shared {
            Some((at, random)) => {
                let drawn = self.drawn.fetch_add(1, Ordering::SeqCst);
                let last = if drawn == 0 {
                    0
                } else {
                    0x40 | ((drawn - 1) & 0x3f)
                };
                RunId::in_partition(partition, at, (random & !0xff) | last)
            }
            None => RunId::in_partition(
                partition,
                Timestamp::from_millis(self.next() >> 16),
                self.next(),
            ),
        }
    }

    fn seed(&self) -> Seed {
        let mut bytes = [0u8; 32];
        for chunk in bytes.as_chunks_mut::<8>().0 {
            *chunk = self.next().to_le_bytes();
        }
        Seed::from_bytes(bytes)
    }
}

/// SplitMix64: a fast, well-distributed mix of a 64-bit state. Not cryptographic.
fn splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_entropy_is_reproducible() {
        let a = SeededEntropy::new(7);
        let b = SeededEntropy::new(7);
        assert_eq!(a.run_id(), b.run_id());
        assert_eq!(a.seed(), b.seed());
        assert_ne!(a.run_id(), a.run_id());
        assert_ne!(SeededEntropy::new(8).seed(), SeededEntropy::new(7).seed());
    }

    #[test]
    fn system_entropy_is_not() {
        assert_ne!(SystemEntropy.seed(), SystemEntropy.seed());
        assert_ne!(SystemEntropy.run_id(), SystemEntropy.run_id());
        let partition = PartitionId::new(41);
        assert_ne!(
            SystemEntropy.run_id_in(partition),
            SystemEntropy.run_id_in(partition)
        );
    }

    #[test]
    fn run_ids_are_drawn_for_their_partition() {
        let seeded = SeededEntropy::new(7);
        for partition in [0, 1, 63, u16::MAX].map(PartitionId::new) {
            assert_eq!(seeded.run_id_in(partition).partition(), Some(partition));
            assert_eq!(
                SystemEntropy.run_id_in(partition).partition(),
                Some(partition)
            );
        }
        assert_eq!(
            SeededEntropy::new(3).run_id_in(PartitionId::new(5)),
            SeededEntropy::new(3).run_id_in(PartitionId::new(5)),
            "seeded run ids are reproducible"
        );
    }

    #[test]
    fn a_process_never_makes_two_ids_of_the_same_suffix() {
        let partition = PartitionId::new(9);
        let ids: Vec<RunId> = (0..2_000)
            .map(|n| {
                if n % 2 == 0 {
                    SystemEntropy.run_id()
                } else {
                    SystemEntropy.run_id_in(partition)
                }
            })
            .collect();
        let suffixes: std::collections::HashSet<String> = ids
            .iter()
            .map(|id| rewind_core::run_suffix(*id, rewind_core::SUFFIX_LEN))
            .collect();
        assert_eq!(suffixes.len(), ids.len());
        // Ordered as they were made, whatever their version.
        assert!(ids.windows(2).all(|w| {
            rewind_core::run_suffix(w[0], rewind_core::SUFFIX_LEN)
                < rewind_core::run_suffix(w[1], rewind_core::SUFFIX_LEN)
        }));
        assert!(
            ids.iter()
                .step_by(2)
                .all(|id| id.as_uuid().get_version_num() == 7)
        );
        assert!(
            ids.iter()
                .skip(1)
                .step_by(2)
                .all(|id| id.partition() == Some(partition))
        );
    }
}
