use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

use crate::time::Timestamp;

/// One partition of the run plane (`archi/06-storage-and-scale.md`, "Partitions, owners and
/// nodes"): a deployment has a fixed number of them, and every run of a namespace lives in the
/// namespace's partition. It prints as its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartitionId(u16);

impl PartitionId {
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier of a run.
///
/// The core only names runs. Generating an id is non-deterministic, so it happens behind the
/// `Entropy` port, in the engine.
///
/// A run id is a UUID version 8 (RFC 9562, the custom layout) laid out as a version 7 is, whose 16
/// bits right after the variant hold the run's [`PartitionId`]
/// (`archi/06-storage-and-scale.md`, "Partitions, owners and nodes"): any process finds a run's
/// partition from its id alone. From the most significant bit:
///
/// ```text
/// unix_ts_ms (48) · version 8 (4) · rand_a (12) · variant 0b10 (2) · partition (16) · rand_b (46)
/// ```
///
/// The time comes first, so ids sort by the millisecond they were drawn at. A run id of another
/// version, a run moved from the tables of the previous design, carries no partition: the
/// control plane knows where it lives ([`RunId::partition`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(Uuid);

/// The partition's place in the 128 bits of a run id: right after the variant bits.
const PARTITION_SHIFT: u32 = 46;
/// The random bits a run id carries besides its time and its partition: 12 in `rand_a`, 46 in
/// `rand_b`.
const RANDOM_BITS: u32 = 58;
const RAND_B_BITS: u32 = 46;
const MILLIS_MASK: u64 = (1 << 48) - 1;

impl RunId {
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub const fn from_u128(value: u128) -> Self {
        Self(Uuid::from_u128(value))
    }

    /// The run id of `partition`, drawn at `at` (its milliseconds since the Unix epoch, on 48
    /// bits) with the low 58 bits of `random`.
    pub const fn in_partition(partition: PartitionId, at: Timestamp, random: u64) -> Self {
        let millis = (at.as_millis() & MILLIS_MASK) as u128;
        let random = random & ((1 << RANDOM_BITS) - 1);
        let rand_a = (random >> RAND_B_BITS) as u128;
        let rand_b = (random & ((1 << RAND_B_BITS) - 1)) as u128;
        let value = (millis << 80)
            | (0x8 << 76)
            | (rand_a << 64)
            | (0b10 << 62)
            | ((partition.0 as u128) << PARTITION_SHIFT)
            | rand_b;
        Self(Uuid::from_u128(value))
    }

    /// The partition the run lives in, the 16 bits after the variant, when the id is of version 8;
    /// `None` for an id of another version, whose partition the control plane keeps.
    pub const fn partition(&self) -> Option<PartitionId> {
        let value = self.0.as_u128();
        if (value >> 76) & 0xF != 0x8 {
            return None;
        }
        Some(PartitionId(((value >> PARTITION_SHIFT) & 0xFFFF) as u16))
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

/// Per-run command number, assigned in the order the code emits commands, from 1.
///
/// The code is single-threaded and deterministic, so the same code fed the same
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
/// It versions together everything that can change a `seq`, a digest or a value the code
/// observes: V8 and ICU, the JS glue, the random generators, the `rewind` library and the
/// `rewind:actions` codegen. It is not the crate version: most releases leave it unchanged.
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
    fn an_id_of_another_version_carries_no_partition() {
        let v7 = RunId::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001);
        assert_eq!(v7.as_uuid().get_version_num(), 7);
        assert_eq!(v7.partition(), None);
    }

    #[test]
    fn partitions_print_as_their_number() {
        let partition = PartitionId::new(63);
        assert_eq!(partition.get(), 63);
        assert_eq!(partition.to_string(), "63");
        assert!(PartitionId::new(2) < PartitionId::new(10));
    }

    #[test]
    fn a_run_id_in_a_partition_round_trips_through_text() {
        let at = Timestamp::from_millis(1_794_675_600_000);
        let id = RunId::in_partition(PartitionId::new(0x1234), at, 0x02aa_aaaa_aaaa_aaaa);
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(text.matches('-').count(), 4);
        let parsed: RunId = text.parse().unwrap();
        assert_eq!(parsed, id);
        assert_eq!(parsed.partition(), Some(PartitionId::new(0x1234)));
        assert_eq!(id.as_uuid().get_version_num(), 8);
        assert_eq!(id.as_uuid().get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn every_partition_is_carried_by_its_run_ids() {
        let at = Timestamp::from_millis(1_794_675_600_000);
        for partition in 0..=u16::MAX {
            let partition = PartitionId::new(partition);
            for random in [0, u64::MAX, 0x0155_5555_5555_5555] {
                let id = RunId::in_partition(partition, at, random);
                assert_eq!(id.partition(), Some(partition));
                assert_eq!(id.as_uuid().get_version_num(), 8);
                assert_eq!(id.as_uuid().get_variant(), uuid::Variant::RFC4122);
                let millis = u64::try_from(id.as_uuid().as_u128() >> 80).unwrap();
                assert_eq!(millis, at.as_millis(), "the time is kept");
            }
        }
    }

    #[test]
    fn the_random_bits_are_kept_apart_from_the_partition() {
        let at = Timestamp::from_millis(1);
        let a = RunId::in_partition(PartitionId::new(7), at, 1);
        let b = RunId::in_partition(PartitionId::new(7), at, 2);
        let c = RunId::in_partition(PartitionId::new(7), at, 1 << 57);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            RunId::in_partition(PartitionId::new(7), at, 1 << 58),
            RunId::in_partition(PartitionId::new(7), at, 0),
            "only the low 58 bits of the random value are used"
        );
        assert_eq!(c.partition(), Some(PartitionId::new(7)));
    }

    #[test]
    fn run_ids_sort_by_the_time_they_were_drawn_at() {
        let mut previous: Option<RunId> = None;
        for (step, millis) in [0_u64, 1, 2, 1_000, 1_794_675_600_000, (1 << 48) - 1]
            .into_iter()
            .enumerate()
        {
            // Later ids carry smaller partitions and random bits: the time still wins.
            let partition = PartitionId::new(u16::MAX - u16::try_from(step).unwrap());
            let id =
                RunId::in_partition(partition, Timestamp::from_millis(millis), u64::MAX >> step);
            if let Some(previous) = previous {
                assert!(previous < id, "{previous} sorts before {id}");
                assert!(previous.to_string() < id.to_string(), "as text too");
            }
            previous = Some(id);
        }
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

/// The characters of a workflow's id after `wf_`: Crockford's base32, lower case, without `i`,
/// `l`, `o` and `u`, as a run's handle writes its suffix.
const WORKFLOW_ID_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// How many characters follow `wf_` in a workflow's id.
pub const WORKFLOW_ID_CHARS: usize = 12;

/// How many of them a workflow's short id keeps, after `wf-`.
pub const WORKFLOW_SHORT_ID_CHARS: usize = 6;

/// The id the API gives a workflow when it creates it (`archi/01-product.md`, "Workflows"):
/// `wf_` and 12 characters of Crockford's base32 in lower case, `wf_7k2m9q4x8d1c`. It never
/// changes, whatever its name, its description or its versions. A workflow's name never starts
/// with `wf_`, so a name never reads as an id.
///
/// The core only names workflows: drawing an id is non-deterministic, and happens behind the
/// engine's `Entropy` port.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkflowId(String);

/// The prefix of every workflow's id.
pub const WORKFLOW_ID_PREFIX: &str = "wf_";

impl WorkflowId {
    /// The id whose 60 bits are the low 60 bits of `bits`, the first character the highest.
    pub fn from_bits(bits: u64) -> Self {
        let mut id = String::with_capacity(WORKFLOW_ID_PREFIX.len() + WORKFLOW_ID_CHARS);
        id.push_str(WORKFLOW_ID_PREFIX);
        for index in (0..WORKFLOW_ID_CHARS).rev() {
            let digit = (bits >> (5 * index)) & 0x1f;
            id.push(char::from(WORKFLOW_ID_ALPHABET[digit as usize]));
        }
        Self(id)
    }

    /// The id a workflow saved before workflows had ids takes when the catalog moves to the
    /// workflow model: the low 60 bits of the first 8 bytes of the SHA-256 of
    /// `rewind:workflow:<tenant>/<name>`, read big-endian. The same in every store, so a migration in SQL and one in
    /// Rust give a saved workflow the same id, and what pins it can be rewritten without reading
    /// the catalog.
    pub fn of_saved(tenant: &str, name: &str) -> Self {
        let digest = crate::Digest::of(format!("rewind:workflow:{tenant}/{name}").as_bytes());
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest.as_bytes()[..8]);
        Self::from_bits(u64::from_be_bytes(first))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Its short id, how a workflow without a name shows and the origin of its runs' handles:
    /// `wf-` and the first 6 characters after `wf_` (`wf-7k2m9q`).
    pub fn short(&self) -> String {
        let chars = &self.0[WORKFLOW_ID_PREFIX.len()..];
        format!("wf-{}", &chars[..WORKFLOW_SHORT_ID_CHARS])
    }

    /// Whether `text` is shaped as an id, so that it names a workflow by its id, never by a
    /// name.
    pub fn is_id(text: &str) -> bool {
        text.parse::<WorkflowId>().is_ok()
    }
}

impl fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid workflow id {0:?}: expected wf_ and 12 characters of Crockford's base32 in lower \
     case, such as wf_7k2m9q4x8d1c"
)]
pub struct InvalidWorkflowId(pub String);

impl FromStr for WorkflowId {
    type Err = InvalidWorkflowId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.strip_prefix(WORKFLOW_ID_PREFIX).is_some_and(|chars| {
            chars.len() == WORKFLOW_ID_CHARS
                && chars.bytes().all(|b| WORKFLOW_ID_ALPHABET.contains(&b))
        });
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidWorkflowId(s.to_owned()))
        }
    }
}

#[cfg(test)]
mod workflow_id_tests {
    use super::*;

    #[test]
    fn a_workflow_id_is_wf_and_12_base32_characters() {
        let id = WorkflowId::from_bits(0x0123_4567_89ab_cdef);
        assert_eq!(id.as_str().len(), 15);
        assert!(id.as_str().starts_with("wf_"));
        assert_eq!(id.as_str().parse::<WorkflowId>(), Ok(id.clone()));
        assert_eq!(id.short().len(), 9);
        assert!(id.short().starts_with("wf-"));
        assert_eq!(&id.short()[3..], &id.as_str()[3..9]);
        assert_eq!(
            WorkflowId::of_saved("acme", "triage"),
            WorkflowId::of_saved("acme", "triage")
        );
        assert_eq!(
            WorkflowId::of_saved("acme", "triage").as_str(),
            "wf_e2nzwv04tf3f",
            "the stores' migrations compute the same"
        );
        assert_eq!(
            WorkflowId::of_saved("dax-staging", "prod_gym").as_str(),
            "wf_rs1wp4r9g4hf"
        );
        let wf: WorkflowId = "wf_7k2m9q4x8d1c".parse().unwrap();
        assert_eq!(wf.short(), "wf-7k2m9q");
        for bad in [
            "wf_7k2m9q4x8d1",
            "wf_7k2m9q4x8d1cc",
            "wf_7K2M9Q4X8D1C",
            "wf_7k2m9q4x8dlc",
            "triage",
        ] {
            assert!(bad.parse::<WorkflowId>().is_err(), "{bad}");
        }
    }
}
