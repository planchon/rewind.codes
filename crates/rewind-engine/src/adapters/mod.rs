//! Adapters that need no infrastructure: the in-memory control plane (catalog, cluster and the
//! broadcast of its changes, access, secrets), run plane (partitions), archive and blobs, and the
//! link between the nodes of one process; in-memory telemetry and webhooks; a fake identity
//! provider; static action responses, routing of calls by route, and type-checking; system
//! and virtual time; system and seeded entropy; the set-aside runs `migrate-runs` moves.
//! Wired together with a virtual clock, they make the engine's mock mode and its test harness
//! (`archi/07-code-architecture.md`, principle 6), and the server's tests. Each adapter of a
//! port with a conformance suite passes it: they are the reference implementations of the
//! contracts.

mod access;
mod archive;
mod blobs;
mod catalog;
mod checker;
mod cluster;
mod dispatcher;
mod entropy;
mod identity;
mod legacy;
mod link;
mod partitions;
mod routing;
mod secrets;
mod telemetry;
mod time;
mod webhooks;

pub use access::MemoryAccess;
pub use archive::MemoryArchive;
pub use blobs::MemoryBlobs;
pub use catalog::MemoryCatalog;
pub use checker::StaticTypeChecker;
pub use cluster::{ChangeStream, ControlPlaneChanges, MemoryCluster, MemoryMembership};
pub use dispatcher::{StaticDispatcher, StaticResponsesError};
pub use entropy::{SeededEntropy, SystemEntropy};
pub use identity::{ACCESS_TOKEN_LIFETIME, INVITATION_LIFETIME, MemoryIdentity};
pub use legacy::MemoryLegacy;
pub use link::LocalLink;
pub use partitions::{MemoryPartitionLog, MemoryPartitions};
pub use routing::{RoutingDispatcher, Unrouted};
pub use secrets::MemorySecrets;
pub use telemetry::MemorySpanExporter;
pub use time::{SystemClock, VirtualClock};
pub use webhooks::MemoryWebhooks;

/// An unguessable token: `prefix` and 256 bits from the operating system's CSPRNG, in hex.
pub(crate) fn random_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    // Without a CSPRNG, tokens would be guessable, and a token is a capability.
    getrandom::fill(&mut bytes).expect("the operating system's random generator is available");
    let mut token = String::with_capacity(prefix.len() + 64);
    token.push_str(prefix);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}
