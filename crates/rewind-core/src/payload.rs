use serde_json::Value;

use crate::canonical::canonical_json;
use crate::digest::Digest;

/// A JSON value carried by an event: run input, action args and results, run output, signal
/// payloads.
///
/// Payloads are content-addressed: a value is identified by the SHA-256 of its canonical JSON
/// (RFC 8785). An event holds either the value itself, or only its digest when the value is
/// kept elsewhere. The core knows a payload's identity, never where it lives
/// (`archi/07-code-architecture.md`, principle 5).
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    Value(Value),
    Digest(Digest),
}

impl Payload {
    /// The digest of a value: SHA-256 of its canonical JSON.
    pub fn digest_of(value: &Value) -> Digest {
        Digest::of(canonical_json(value).as_bytes())
    }

    /// The value, or `None` if only the digest is held.
    pub fn value(&self) -> Option<&Value> {
        match self {
            Payload::Value(value) => Some(value),
            Payload::Digest(_) => None,
        }
    }

    pub fn into_value(self) -> Option<Value> {
        match self {
            Payload::Value(value) => Some(value),
            Payload::Digest(_) => None,
        }
    }

    pub fn digest(&self) -> Digest {
        match self {
            Payload::Value(value) => Self::digest_of(value),
            Payload::Digest(digest) => *digest,
        }
    }

    /// Whether both payloads identify the same value. Two values are compared by their
    /// canonical forms, which is what comparing their digests means without hashing them.
    pub fn same_as(&self, other: &Payload) -> bool {
        match (self, other) {
            (Payload::Value(a), Payload::Value(b)) => canonical_json(a) == canonical_json(b),
            _ => self.digest() == other.digest(),
        }
    }

    /// Short rendering for error messages and traces read by models: the canonical JSON,
    /// truncated to `max_chars` characters.
    pub fn preview(&self, max_chars: usize) -> String {
        match self {
            Payload::Value(value) => truncate(&canonical_json(value), max_chars),
            Payload::Digest(digest) => format!("<payload {}>", digest.short()),
        }
    }
}

impl From<Value> for Payload {
    fn from(value: Value) -> Self {
        Payload::Value(value)
    }
}

/// `text` cut to `max_chars` characters, with an ellipsis when cut.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_digest_is_the_sha256_of_the_canonical_json() {
        assert_eq!(Payload::from(json!(null)).digest(), Digest::of(b"null"));
        let a = Payload::Value(serde_json::from_str(r#"{"b":1,"a":2.0}"#).unwrap());
        let b = Payload::Value(serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap());
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.digest(), Digest::of(br#"{"a":2,"b":1}"#));
        assert!(a.same_as(&b));
    }

    #[test]
    fn a_value_and_its_digest_are_the_same_payload() {
        let value = Payload::Value(json!({ "issues": [1, 2, 3] }));
        let digest_only = Payload::Digest(value.digest());
        assert!(value.same_as(&digest_only));
        assert!(digest_only.same_as(&value));
        assert!(!Payload::Value(json!({ "issues": [] })).same_as(&digest_only));
        assert_eq!(digest_only.value(), None);
    }

    #[test]
    fn previews_are_canonical_and_bounded() {
        let payload = Payload::Value(json!({ "b": "x".repeat(50), "a": 1 }));
        assert_eq!(payload.preview(13), "{\"a\":1,\"b\":\"x…");
        let digest = Payload::Digest(Digest::of(b"null"));
        assert_eq!(digest.preview(12), "<payload 74234e98afe7>");
    }
}
