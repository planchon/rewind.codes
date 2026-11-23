//! The node link over HTTP/2: what the nodes of a deployment send each other
//! (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`", "Inside a node";
//! `archi/06-storage-and-scale.md`, "Partitions, owners and nodes": the edge, executors;
//! `archi/08-decisions.md`, "Taken while writing the object-storage spec": the node link).
//!
//! ```text
//!  node a                                                      node b
//!  router, owners, org actors                                  link server (serve)
//!   │ NodeLink::call ──────── POST /link/call ───────────────►  NodeHandle::serve
//!   │ NodeLink::activate ──── POST /link/activate ───────────►  NodeHandle::serve_activation
//!   │ NodeLink::subscribe ─── POST /link/subscribe ──────────►  NodeHandle::subscribe
//!   │                     ◄── text/event-stream of Changed ──    (a comment every 15 s)
//!  HttpLink: one HTTP/2 connection per node, without TLS (prior knowledge), on the private
//!  network; `Authorization: Bearer <cluster secret>` and `rewind-link-version` on every request
//! ```
//!
//! - [`HttpLink`] is the engine's `NodeLink` over HTTP/2: it finds a node's address among the
//!   members of the cluster ([`Members`]), reads them again when it does not know a node, and
//!   keeps one connection per node. A request that does not reach its node is
//!   `LinkError::Unreachable`; a node that refuses it (the secret, the version) is
//!   `LinkError::Failed`.
//! - [`serve`] serves a node's side on a listener of its own: every request authenticated by the
//!   deployment's secret, compared in constant time, and of this build's [`LINK_VERSION`].
//! - [`dto`] is the wire form of every message, JSON.
//! - [`SharedMembership`] lets the node and its link share the node's membership: the node holds
//!   it for its liveness, the link reads the members through it.

mod client;
pub mod dto;
mod server;

use std::fmt;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

pub use client::{HttpLink, Members, MembersOf, RemoteChanges, SharedMembership};
pub use dto::{LINK_VERSION, VERSION_HEADER};
pub use server::{KEEP_ALIVE, serve};

/// The route of a request to a node: a [`dto::NodeRequest`], answered by a [`dto::NodeReply`].
pub const CALL: &str = "/link/call";
/// The route of an activation for a remote executor: a [`dto::ActivationRequest`], answered by a
/// [`dto::ActivationResult`].
pub const ACTIVATE: &str = "/link/activate";
/// The route of a subscription: a [`dto::Subscribe`], answered by server-sent events, each a
/// [`dto::Changed`] (event `changed`), until the subscriber closes the stream.
pub const SUBSCRIBE: &str = "/link/subscribe";

/// What sets the node link up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the cluster secret must be 1 to 1024 visible ASCII characters")]
    InvalidSecret,
    #[error("the HTTP client of the node link cannot be built: {0}")]
    Client(String),
}

/// The secret of a deployment (`--cluster-secret`): every request of the node link carries it,
/// as `Authorization: Bearer <secret>`. It is never printed.
#[derive(Clone)]
pub struct ClusterSecret(Arc<str>);

impl ClusterSecret {
    /// 1 to 1024 visible ASCII characters: it travels in a header.
    pub fn new(secret: impl Into<String>) -> Result<Self, Error> {
        let secret = secret.into();
        let valid = (1..=1024).contains(&secret.len())
            && secret.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(secret.into()))
        } else {
            Err(Error::InvalidSecret)
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is the secret, compared in constant time: the digests of both, so
    /// that neither the length nor a common prefix shows in the time it takes.
    pub(crate) fn matches(&self, presented: &str) -> bool {
        let expected = Sha256::digest(self.0.as_bytes());
        let presented = Sha256::digest(presented.as_bytes());
        expected.as_slice().ct_eq(presented.as_slice()).into()
    }
}

impl fmt::Debug for ClusterSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClusterSecret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_visible_ascii_and_never_printed() {
        let secret = ClusterSecret::new("s3cret-of-the-cluster").unwrap();
        assert!(secret.matches("s3cret-of-the-cluster"));
        assert!(!secret.matches("s3cret-of-the-clusteR"));
        assert!(!secret.matches("s3cret"));
        assert!(!secret.matches(""));
        assert_eq!(format!("{secret:?}"), "ClusterSecret(<redacted>)");
        for invalid in ["", "with space", "tab\t", "é", &"x".repeat(1025)] {
            assert_eq!(
                ClusterSecret::new(invalid).unwrap_err(),
                Error::InvalidSecret
            );
        }
    }
}
