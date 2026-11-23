use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_core::{RunId, Seed};

use crate::ports::Entropy;

/// UUIDv7 run ids (sortable by creation time) and seeds from the operating system's CSPRNG.
///
/// The 12 bits after an id's millisecond are this process's node number (6 bits, drawn at random
/// when it makes its first id) and a counter of its ids within that millisecond (6 bits; a 65th
/// id moves to the next millisecond, as does a clock that steps back). Two ids of one process
/// therefore never share their first 60 bits, which a run's handle encodes
/// (`archi/06-storage-and-scale.md`, "Run handles"); the store lengthens the handle of the rare
/// run that shares them with another process's.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEntropy;

/// The millisecond of the last id this process made, and how many it made in it.
struct Sequence {
    ms: u64,
    count: u16,
}

const NODE_BITS: u16 = 6;
const COUNTER_MAX: u16 = (1 << 6) - 1;

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
}

impl SeededEntropy {
    pub fn new(base: u64) -> Self {
        Self {
            base,
            counter: AtomicU64::new(0),
        }
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
    }

    #[test]
    fn a_process_never_makes_two_ids_of_the_same_suffix() {
        let ids: Vec<RunId> = (0..2_000).map(|_| SystemEntropy.run_id()).collect();
        let suffixes: std::collections::HashSet<String> = ids
            .iter()
            .map(|id| rewind_core::run_suffix(*id, rewind_core::SUFFIX_LEN))
            .collect();
        assert_eq!(suffixes.len(), ids.len());
        // Ordered as they were made, and version 7.
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert!(ids.iter().all(|id| id.as_uuid().get_version_num() == 7));
    }
}
