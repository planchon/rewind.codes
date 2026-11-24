//! The code, the immutable and content-addressed input of a run. A run records the digest
//! it started with. The action set, the other input, is in [`crate::action_set`].

use crate::digest::Digest;

/// The TypeScript source the model wrote. Its digest is the SHA-256 of the source, so a run
/// always replays the exact code that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Code {
    source: String,
    digest: Digest,
}

impl Code {
    pub fn new(source: impl Into<String>) -> Self {
        let source = source.into();
        let digest = Digest::of(source.as_bytes());
        Self { source, digest }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn digest(&self) -> Digest {
        self.digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_content_addressed() {
        assert_eq!(Code::new("a").digest(), Code::new("a").digest());
        assert_ne!(Code::new("a").digest(), Code::new("b").digest());
        assert_eq!(Code::new("a").digest(), Digest::of(b"a"));
    }
}
