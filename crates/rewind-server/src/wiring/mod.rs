//! The adapters this composition root wires, each in its own module
//! (`archi/07-code-architecture.md`, "Crates"):
//!
//! | Module | Port | Adapter |
//! | --- | --- | --- |
//! | [`control`] | `CatalogStore`, `AccessStore`, `SecretStore`, `Cluster` | `rewind-store-postgres`, `rewind-store-turso`; the engine's in-memory adapters for the tests |
//! | [`bucket`] | `PartitionStore`, `RunArchive`, `BlobStore` | `rewind-store-slatedb`; `rewind-blob` (files, S3 behind this process's caches) |
//! | [`link`] | `NodeLink` | in the process |
//! | [`executor`] | `CodeExecutor` | `rewind-runtime`'s `V8Executor` |
//! | [`actions`] | `ActionDispatcher`, discovery and declarations | the engine's routing dispatcher |
//! | [`checker`] | `TypeChecker` | `rewind-check` (tsgo) |
//! | [`telemetry`] | `SpanExporter`, the `tracing` subscriber | `rewind-otel` |
//! | [`webhooks`] | `WebhookSender` | HTTP, Standard Webhooks (this crate) |
//! | [`clock`] | `Clock` | system or virtual time |
//! | [`identity`] | `IdentityProvider` | `rewind-workos` (WorkOS) |
//!
//! Entropy is the engine's system entropy. [`jobs`] are the control plane's daily jobs.

pub(crate) mod actions;
pub(crate) mod bucket;
pub(crate) mod checker;
pub(crate) mod clock;
pub(crate) mod control;
pub(crate) mod executor;
pub(crate) mod identity;
pub(crate) mod jobs;
pub(crate) mod link;
pub(crate) mod telemetry;
pub(crate) mod webhooks;
