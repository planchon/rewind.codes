use std::sync::atomic::{AtomicU64, Ordering};

use rewind_core::{RunId, Seed};
use uuid::Uuid;

use crate::ports::Entropy;

/// UUIDv7 run ids (sortable by creation time) and seeds from the operating system's CSPRNG.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn run_id(&self) -> RunId {
        RunId::from_uuid(Uuid::now_v7())
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
}
