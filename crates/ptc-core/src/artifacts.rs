//! The immutable inputs of a run: the script and the toolset it may call. Both are
//! content-addressed, and a run records the digests it started with.

use serde_json::{Value, json};

use crate::canonical::canonical_json;
use crate::digest::Digest;

/// The TypeScript source the model wrote. Its digest is the SHA-256 of the source, so a run
/// always replays the exact code that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    source: String,
    digest: Digest,
}

impl Script {
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

/// One tool a script may call, named as its server defines it (`github.list_issues`).
/// Schemas and policies arrive with codegen (M2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolSpec {
    pub name: String,
}

/// The tools a run may call: a sorted and deduplicated list of names, until toolsets carry
/// schemas and policies (M2).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Toolset {
    tools: Vec<ToolSpec>,
}

impl Toolset {
    /// Tools are sorted by name and deduplicated, so the digest doesn't depend on their order.
    pub fn new(tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        let mut tools: Vec<ToolSpec> = tools.into_iter().collect();
        tools.sort();
        tools.dedup();
        Self { tools }
    }

    pub fn from_names<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Self {
        Self::new(names.into_iter().map(|name| ToolSpec { name: name.into() }))
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools
            .binary_search_by(|tool| tool.name.as_str().cmp(name))
            .is_ok()
    }

    /// SHA-256 of the canonical JSON `{"tools":[{"name":…},…]}`.
    pub fn digest(&self) -> Digest {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| json!({ "name": tool.name }))
            .collect();
        Digest::of(canonical_json(&json!({ "tools": tools })).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_content_addressed() {
        assert_eq!(Script::new("a").digest(), Script::new("a").digest());
        assert_ne!(Script::new("a").digest(), Script::new("b").digest());
        assert_eq!(Script::new("a").digest(), Digest::of(b"a"));
    }

    #[test]
    fn toolset_digest_ignores_order_and_duplicates() {
        let a = Toolset::from_names(["github.get_issue", "slack.post", "github.get_issue"]);
        let b = Toolset::from_names(["slack.post", "github.get_issue"]);
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
        assert!(a.contains("slack.post"));
        assert!(!a.contains("slack"));
        assert_eq!(
            Toolset::from_names(["b", "a"]).digest(),
            Digest::of(br#"{"tools":[{"name":"a"},{"name":"b"}]}"#)
        );
    }
}
