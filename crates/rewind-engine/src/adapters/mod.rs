//! Adapters that need no infrastructure: in-memory storage, read model, secrets, telemetry
//! and webhooks; static tool responses, routing of tool calls by route, and type-checking;
//! system and virtual time; system and seeded entropy. Wired together with a virtual clock, they make the engine's mock mode and its
//! test harness (`archi/07-code-architecture.md`, principle 6). Each adapter of a port with a
//! conformance suite passes it: they are the reference implementations of the contracts.

mod blobs;
mod checker;
mod dispatcher;
mod entropy;
mod journal;
mod read_model;
mod routing;
mod secrets;
mod telemetry;
mod time;
mod webhooks;

pub use blobs::MemoryBlobs;
pub use checker::StaticTypeChecker;
pub use dispatcher::{StaticDispatcher, StaticResponsesError};
pub use entropy::{SeededEntropy, SystemEntropy};
pub use journal::MemoryJournal;
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
