//! A run's handle: `<namespace>/<origin>/<suffix>`, what people read, copy and type to name a
//! run (`archi/06-storage-and-scale.md`, "Run handles"). The run's id stays its UUID; the handle
//! says for whom it ran (its namespace), what ran (its origin: the workflow, or the script's
//! title as a slug) and which run it is (its suffix).
//!
//! The suffix is the start of the run's UUID in Crockford base32, its version and variant left
//! out: its millisecond and the 12 bits after it, which the engine fills with the number of the
//! process that made the id and a counter of that process within the millisecond. The runs whose
//! UUID starts with a suffix are one range of ids for each version of run ids (7 for a run moved
//! from the previous design, 8 for a run of a partition): a handle needs no index of its own.
//! Runs that share their first 60 bits get a longer suffix, as long as it takes to tell them
//! apart.

use std::fmt;
use std::str::FromStr;

use crate::ids::RunId;

/// The characters of a suffix, which encode the millisecond (48 bits) and the 12 bits after it.
pub const SUFFIX_LEN: usize = 12;

/// The characters of the longest suffix: every bit of the UUID but its version and variant.
pub const MAX_SUFFIX_LEN: usize = 25;

/// The origin of a run without a workflow and whose script has no title.
pub const SCRIPT_ORIGIN: &str = "script";

/// The longest origin, in characters.
pub const MAX_ORIGIN_LEN: usize = 40;

/// Crockford's base32, lower case: no `i`, `l`, `o` nor `u`.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The bits of a run id that vary: 48 of milliseconds, 12 after the version, 62 after the
/// variant. The handle encodes them in this order, padded by 3 zero bits to 125 (25 characters).
const BITS: u32 = 122;
const PADDED: u32 = 125;

/// The versions of run ids, both laid out as a version 7: 7 for the runs moved from the previous
/// design, 8 for the runs of a partition ([`RunId::in_partition`]).
const VERSIONS: [u128; 2] = [0x7, 0x8];

/// The 122 bits of a run id that are not its version nor its variant, in order.
fn meaningful(id: RunId) -> u128 {
    let uuid = id.as_uuid().as_u128();
    let ms = uuid >> 80;
    let rand_a = (uuid >> 64) & 0xfff;
    let rand_b = uuid & ((1 << 62) - 1);
    (ms << 74) | (rand_a << 62) | rand_b
}

/// The run id of `version` whose varying bits are `bits`.
fn from_meaningful(bits: u128, version: u128) -> RunId {
    let ms = bits >> 74;
    let rand_a = (bits >> 62) & 0xfff;
    let rand_b = bits & ((1 << 62) - 1);
    RunId::from_u128((ms << 80) | (version << 76) | (rand_a << 64) | (0b10 << 62) | rand_b)
}

/// The run's suffix of `len` characters (clamped to 1..=25).
pub fn run_suffix(id: RunId, len: usize) -> String {
    let len = len.clamp(1, MAX_SUFFIX_LEN);
    let padded = meaningful(id) << (PADDED - BITS);
    (0..len)
        .map(|i| {
            let shift = PADDED - 5 * (u32::try_from(i).unwrap_or(0) + 1);
            char::from(ALPHABET[((padded >> shift) & 0x1f) as usize])
        })
        .collect()
}

/// The ids whose suffix of [`SUFFIX_LEN`] characters is `id`'s (their millisecond and the 12
/// bits after it), one range per version of run ids: among them, the owner of a partition looks
/// for those a new run's handle must tell it apart from.
pub fn suffix_neighbours(id: RunId) -> [(RunId, RunId); 2] {
    suffix_ranges(&run_suffix(id, SUFFIX_LEN)).unwrap_or([(id, id); 2])
}

/// The shortest suffix, at least [`SUFFIX_LEN`] characters, that `id` shares with none of
/// `others`: the length the owner of a partition gives a new run's handle, among the runs of its
/// namespace whose ids fall in the ranges of its [`SUFFIX_LEN`] characters.
pub fn suffix_len_apart(id: RunId, others: impl IntoIterator<Item = RunId>) -> usize {
    let mine = run_suffix(id, MAX_SUFFIX_LEN);
    others
        .into_iter()
        .filter(|other| *other != id)
        .map(|other| {
            let theirs = run_suffix(other, MAX_SUFFIX_LEN);
            mine.bytes()
                .zip(theirs.bytes())
                .take_while(|(a, b)| a == b)
                .count()
        })
        .fold(SUFFIX_LEN, |len, common| len.max(common + 1))
        .min(MAX_SUFFIX_LEN)
}

/// The value of a Crockford base32 character: either case; `i` and `l` read as `1`, `o` as `0`.
fn digit(c: char) -> Option<u128> {
    let c = match c.to_ascii_lowercase() {
        'i' | 'l' => '1',
        'o' => '0',
        c => c,
    };
    ALPHABET
        .iter()
        .position(|&a| char::from(a) == c)
        .map(|v| v as u128)
}

/// The ids a suffix names: for each version of run ids (7, then 8), the first and the last id
/// that start with it. `None` for a suffix shorter than [`SUFFIX_LEN`], longer than
/// [`MAX_SUFFIX_LEN`], or not base32.
pub fn suffix_ranges(suffix: &str) -> Option<[(RunId, RunId); 2]> {
    let len = suffix.chars().count();
    if !(SUFFIX_LEN..=MAX_SUFFIX_LEN).contains(&len) {
        return None;
    }
    let mut prefix: u128 = 0;
    for c in suffix.chars() {
        prefix = (prefix << 5) | digit(c)?;
    }
    let free = PADDED - 5 * u32::try_from(len).ok()?;
    let low = prefix << free;
    let high = low | ((1u128 << free) - 1);
    // The 3 padding bits are zero: a full suffix that sets one names no id.
    if free < PADDED - BITS && low & ((1 << (PADDED - BITS)) - 1) != 0 {
        return None;
    }
    let shift = PADDED - BITS;
    Some(VERSIONS.map(|version| {
        (
            from_meaningful(low >> shift, version),
            from_meaningful(high >> shift, version),
        )
    }))
}

/// The origin a script's title gives a run: lower case ASCII letters and digits, every other run
/// of characters a `-`, at most [`MAX_ORIGIN_LEN`] characters. `None` when nothing is left.
pub fn origin_slug(title: &str) -> Option<String> {
    let mut slug = String::new();
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !slug.is_empty() {
                slug.push('-');
            }
            dash = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
        if slug.len() >= MAX_ORIGIN_LEN {
            break;
        }
    }
    slug.truncate(MAX_ORIGIN_LEN);
    let slug = slug.trim_end_matches('-').to_owned();
    (!slug.is_empty()).then_some(slug)
}

/// Whether `origin` can be one: 1 to 64 characters, ASCII letters, digits, `-`, `_` and `$` (a
/// workflow's name, an identifier, or a slug).
pub fn is_origin(origin: &str) -> bool {
    (1..=64).contains(&origin.len())
        && origin
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_$".contains(&b))
}

/// A run's handle, `<namespace>/<origin>/<suffix>`. `/` occurs in none of its parts: a
/// namespace is ASCII letters, digits, `-`, `_`, `.`, `:` and `@`, an origin letters, digits,
/// `-`, `_` and `$`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunHandle {
    pub namespace: String,
    pub origin: String,
    pub suffix: String,
}

impl RunHandle {
    /// The handle of the run `id`, in `namespace`, of `origin`, with a suffix of `suffix_len`
    /// characters.
    pub fn new(namespace: &str, origin: &str, id: RunId, suffix_len: usize) -> Self {
        Self {
            namespace: namespace.to_owned(),
            origin: origin.to_owned(),
            suffix: run_suffix(id, suffix_len),
        }
    }

    /// The ids its suffix names, one range per version of run ids.
    pub fn ranges(&self) -> Option<[(RunId, RunId); 2]> {
        suffix_ranges(&self.suffix)
    }

    /// Whether this handle names the run `id` of `namespace` and `origin`, whose suffix has
    /// `suffix_len` characters.
    pub fn names(&self, namespace: &str, origin: &str, id: RunId, suffix_len: usize) -> bool {
        self.namespace == namespace
            && self.origin == origin
            && self.suffix.len() == suffix_len
            && run_suffix(id, suffix_len) == self.suffix
    }
}

impl fmt::Display for RunHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.origin, self.suffix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid run handle {0:?}: expected <namespace>/<origin>/<suffix>")]
pub struct InvalidRunHandle(pub String);

impl FromStr for RunHandle {
    type Err = InvalidRunHandle;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidRunHandle(s.to_owned());
        let mut parts = s.split('/');
        let (Some(namespace), Some(origin), Some(suffix), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        let namespace_ok = (1..=128).contains(&namespace.len())
            && namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:@".contains(&b));
        if !namespace_ok || !is_origin(origin) || suffix_ranges(suffix).is_none() {
            return Err(invalid());
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            origin: origin.to_owned(),
            suffix: suffix.to_ascii_lowercase(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PartitionId;
    use crate::time::Timestamp;

    /// The range of version 7 ids a suffix names.
    fn suffix_range(suffix: &str) -> Option<(RunId, RunId)> {
        suffix_ranges(suffix).map(|[v7, _]| v7)
    }

    fn v7(ms: u64, rand_a: u16, rand_b: u64) -> RunId {
        RunId::from_v7_parts(ms, rand_a, rand_b)
    }

    #[test]
    fn a_suffix_encodes_the_millisecond_and_the_next_twelve_bits() {
        let id = v7(1_795_383_896_000, 0b000011_000101, 0x1234_5678_9abc);
        let suffix = run_suffix(id, SUFFIX_LEN);
        assert_eq!(suffix.len(), 12);
        let (low, high) = suffix_range(&suffix).expect("a range");
        assert!(low <= id && id <= high);
        // Another counter in the same millisecond: another suffix.
        let next = v7(1_795_383_896_000, 0b000011_000110, 0x1234_5678_9abc);
        assert_ne!(run_suffix(next, SUFFIX_LEN), suffix);
        assert!(!(low <= next && next <= high));
        // The random bits after the twelve do not change it.
        assert_eq!(
            run_suffix(v7(1_795_383_896_000, 0b000011_000101, 7), 12),
            suffix
        );
    }

    #[test]
    fn a_full_suffix_names_one_id() {
        let id = v7(1_795_383_896_123, 0xabc, 0x3fff_ffff_ffff_ffff);
        let full = run_suffix(id, MAX_SUFFIX_LEN);
        assert_eq!(suffix_range(&full), Some((id, id)));
        for len in SUFFIX_LEN..=MAX_SUFFIX_LEN {
            let (low, high) = suffix_range(&run_suffix(id, len)).expect("a range");
            assert!(low <= id && id <= high, "length {len}");
        }
    }

    #[test]
    fn suffixes_sort_as_their_ids() {
        let a = v7(1_000, 5, 0);
        let b = v7(1_000, 6, 0);
        let c = v7(1_001, 0, 0);
        let s = |id| run_suffix(id, SUFFIX_LEN);
        assert!(s(a) < s(b) && s(b) < s(c));
    }

    #[test]
    fn a_suffix_reads_either_case_and_crockfords_confusables() {
        let id = v7(1_795_383_896_000, 0x041, 0);
        let suffix = run_suffix(id, SUFFIX_LEN);
        assert_eq!(
            suffix_range(&suffix.to_ascii_uppercase()),
            suffix_range(&suffix)
        );
        assert_eq!(
            suffix_range(&suffix.replace('1', "l")),
            suffix_range(&suffix)
        );
        assert_eq!(suffix_range("short"), None);
        assert_eq!(suffix_range("0123456789u!"), None);
    }

    #[test]
    fn a_handle_round_trips_and_names_its_run() {
        let id = v7(1_795_383_896_000, 0x0c5, 99);
        let handle = RunHandle::new("dana", "triage", id, SUFFIX_LEN);
        let text = handle.to_string();
        assert!(text.starts_with("dana/triage/"));
        let parsed: RunHandle = text.parse().expect("a handle");
        assert_eq!(parsed, handle);
        assert!(parsed.names("dana", "triage", id, SUFFIX_LEN));
        assert!(!parsed.names("dana", "triage", id, SUFFIX_LEN + 1));
        assert!(!parsed.names("bob", "triage", id, SUFFIX_LEN));
        assert!(!parsed.names("dana", "digest", id, SUFFIX_LEN));
        assert!("dana/triage".parse::<RunHandle>().is_err());
        assert!("da/na/triage/0123456789ab".parse::<RunHandle>().is_err());
        assert!(
            "user@x.com:1/triage-new-issues/0123456789ab"
                .parse::<RunHandle>()
                .is_ok()
        );
    }

    #[test]
    fn a_run_sharing_its_first_bits_gets_a_longer_suffix() {
        let a = v7(5_000, 0x0c5, 0x2_0000_0000_0000);
        let b = v7(5_000, 0x0c5, 0x3_0000_0000_0000);
        let c = v7(5_001, 0, 0);
        assert_eq!(suffix_len_apart(a, [c]), SUFFIX_LEN);
        let len = suffix_len_apart(b, [a, b, c]);
        assert!(len > SUFFIX_LEN);
        assert_ne!(run_suffix(a, len), run_suffix(b, len));
        assert_eq!(run_suffix(a, len - 1), run_suffix(b, len - 1));
    }

    #[test]
    fn a_title_gives_a_slug() {
        assert_eq!(
            origin_slug("Triage new GitHub issues").as_deref(),
            Some("triage-new-github-issues")
        );
        assert_eq!(
            origin_slug("  Résumé: daily -- digest!").as_deref(),
            Some("r-sum-daily-digest")
        );
        assert_eq!(origin_slug("!!!"), None);
        let long = origin_slug(&"word ".repeat(30)).expect("a slug");
        assert!(long.len() <= MAX_ORIGIN_LEN && !long.ends_with('-'));
        assert!(is_origin(&long));
    }

    #[test]
    fn a_run_of_a_partition_is_named_as_a_run_moved_with_its_id() {
        let at = Timestamp::from_millis(1_795_383_896_000);
        let rand_a: u64 = 0b000011_000101;
        let id = RunId::in_partition(PartitionId::new(41), at, (rand_a << 46) | 99);
        let suffix = run_suffix(id, SUFFIX_LEN);
        let moved = v7(1_795_383_896_000, 0b000011_000101, 7);
        assert_eq!(
            run_suffix(moved, SUFFIX_LEN),
            suffix,
            "the version is not in the suffix"
        );
        let [old, new] = suffix_ranges(&suffix).expect("ranges");
        assert!(old.0 <= moved && moved <= old.1);
        assert!(new.0 <= id && id <= new.1);
        assert!(!(old.0 <= id && id <= old.1));
        assert_eq!(suffix_neighbours(id), [old, new]);
        let handle = RunHandle::new("dana", "triage", id, SUFFIX_LEN);
        assert!(handle.names("dana", "triage", id, SUFFIX_LEN));
        let full = run_suffix(id, MAX_SUFFIX_LEN);
        assert_eq!(suffix_ranges(&full).map(|[_, new]| new), Some((id, id)));
        assert!(suffix_len_apart(id, [moved]) > SUFFIX_LEN);
    }
}
