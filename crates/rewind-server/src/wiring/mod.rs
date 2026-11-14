//! The adapters this composition root wires, each in its own module:
//!
//! | Module | Port | Adapter |
//! | --- | --- | --- |
//! | [`store`] | `JournalStore`, `AccessStore`, `ReadModel`, `SecretStore` | `rewind-store-turso`, `rewind-store-postgres` |
//! | [`blobs`] | `BlobStore` | `rewind-blob` (files, S3 behind the workers' cache) |
//! | [`tools`] | `ToolDispatcher`, discovery and declarations | the engine's routing dispatcher |
//! | [`checker`] | `TypeChecker` | `rewind-check` (tsgo) |
//! | [`telemetry`] | `SpanExporter`, the `tracing` subscriber | log lines on standard error |
//! | [`webhooks`] | `WebhookSender` | HTTP, Standard Webhooks (this crate) |
//! | [`clock`] | `Clock` | system or virtual time |
//! | [`identity`] | `IdentityProvider` | `rewind-workos` (WorkOS), on the api role |
//!
//! The executor is `rewind-runtime`'s `V8Executor`, and entropy the engine's system entropy.

pub(crate) mod blobs;
pub(crate) mod checker;
pub(crate) mod clock;
pub(crate) mod identity;
pub(crate) mod store;
pub(crate) mod telemetry;
pub(crate) mod tools;
pub(crate) mod webhooks;
