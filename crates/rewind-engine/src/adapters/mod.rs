//! Adapters that need no infrastructure: the in-memory control plane (catalog, cluster and the
//! broadcast of its changes), run plane (partitions) and archive; in-memory storage, access,
//! read model, secrets, telemetry and webhooks; a fake identity provider; static tool
//! responses, routing of tool calls by route, and type-checking; system and virtual time;
//! system and seeded entropy.
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
mod journal;
mod partitions;
mod read_model;
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
pub use journal::MemoryJournal;
pub use partitions::{MemoryPartitionLog, MemoryPartitions};
pub use read_model::MemoryReadModel;
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
