use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

/// Identifier of a run.
///
/// The core only names runs. Generating an id is non-deterministic, so it happens behind the
/// `Entropy` port, in the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(Uuid);

impl RunId {
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub const fn from_u128(value: u128) -> Self {
        Self(Uuid::from_u128(value))
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// The UUIDv7 of millisecond `ms` (its low 48 bits), whose 12 bits after the version are
    /// `rand_a` and whose 62 bits after the variant are `rand_b` (their low bits). The engine
    /// fills `rand_a` with a node's number and its counter within the millisecond, so that two
    /// ids never share their first 60 bits ([`crate::RunHandle`]).
    pub const fn from_v7_parts(ms: u64, rand_a: u16, rand_b: u64) -> Self {
        let ms = (ms as u128) & ((1 << 48) - 1);
        let rand_a = (rand_a as u128) & 0xfff;
        let rand_b = (rand_b as u128) & ((1 << 62) - 1);
        Self(Uuid::from_u128(
            (ms << 80) | (0x7 << 76) | (rand_a << 64) | (0b10 << 62) | rand_b,
        ))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid run id {0:?}: expected a UUID")]
pub struct InvalidRunId(pub String);

impl FromStr for RunId {
    type Err = InvalidRunId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|_| InvalidRunId(s.to_owned()))
    }
}

/// Per-run command number, assigned in the order the script emits commands, from 1.
///
/// The script is single-threaded and deterministic, so the same script fed the same
/// completions assigns the same `Seq` to the same call, even under `Promise.all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Seq(pub u32);

impl Seq {
    pub const FIRST: Seq = Seq(1);

    /// The next `Seq`, or `None` past `u32::MAX`.
    pub fn next(self) -> Option<Seq> {
        self.0.checked_add(1).map(Seq)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The run's 256-bit seed. It keys the sandbox's random generators (`Math.random` and
/// `crypto`), so it is as sensitive as the payloads: `Debug` does not print it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Seed([u8; 32]);

impl Seed {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Seed(<redacted>)")
    }
}

/// The determinism epoch a run started under (`archi/03-execution.md`, "Determinism epoch").
///
/// It versions together everything that can change a `seq`, a digest or a value the script
/// observes: V8 and ICU, the JS glue, the random generators, the `rewind` library and the
/// `rewind:tools` codegen. It is not the crate version: most releases leave it unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Epoch(pub u32);

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch {}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_round_trip_through_text() {
        let id = RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001);
        assert_eq!(id.to_string(), "01930000-0000-7000-8000-000000000001");
        assert_eq!(id.to_string().parse::<RunId>().unwrap(), id);
        assert_eq!(
            "not-a-uuid".parse::<RunId>(),
            Err(InvalidRunId("not-a-uuid".into()))
        );
    }

    #[test]
    fn seqs_count_from_one_and_never_wrap() {
        assert_eq!(Seq::FIRST.next(), Some(Seq(2)));
        assert_eq!(Seq(u32::MAX).next(), None);
        assert_eq!(Seq(3).to_string(), "#3");
    }

    #[test]
    fn seeds_are_never_printed() {
        let seed = Seed::from_bytes([0xab; 32]);
        assert_eq!(format!("{seed:?}"), "Seed(<redacted>)");
        assert_eq!(seed.as_bytes(), &[0xab; 32]);
    }
}
