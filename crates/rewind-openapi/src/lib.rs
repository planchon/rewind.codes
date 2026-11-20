//! API tools (`archi/04-tools.md`, "API tools"; `archi/07-code-architecture.md`, "Crates").
//!
//! - **Reading a document** ([`read`]): an OpenAPI 3.0 or 3.1 document, in JSON, read at
//!   registration into a pinned [`Api`](rewind_core::Api) and one [`Tool`](rewind_core::Tool)
//!   per operation, with its [`ApiOperation`](rewind_core::ApiOperation): named by its
//!   `operationId` (or its method and path), its input the path, query and header parameters
//!   and the JSON body under `body`, its output the schema of its first `2xx` JSON response,
//!   `$ref` resolved within the document; its `auth` given, or the document's security
//!   requirement; the operations left out (a body other than JSON, a cookie parameter) reported
//!   as [`Skipped`]. [`fetch`] reads a document from its URL, through the egress checks.
//! - **[`ApiDispatcher`]**, the [`ToolDispatcher`](rewind_engine::ports::ToolDispatcher) of the
//!   `api` route: the request built from the args (the path parameters percent-encoded into the
//!   path, `traceparent`, `Idempotency-Key` when the operation declares it, the
//!   [`CallAuth`](rewind_engine::ports::CallAuth) the engine resolved), sent through
//!   `rewind-outbound`, the answer mapped to an outcome (`2xx` a result, `401` parked,
//!   `408`/`429`/`5xx` transient, another status a `ToolError`).
//!
//! The document is read as JSON values (`serde_json::Value`), not through an OpenAPI crate: none
//! maintained reads both 3.0 and 3.1, and a toolset needs only paths, parameters, bodies,
//! responses, security schemes and `$ref`, whose schemas it pins as JSON Schema
//! (`archi/08-decisions.md`).

#![forbid(unsafe_code)]

mod dispatch;
mod fetch;
mod read;
mod request;
mod schema;

pub use dispatch::{ApiConfig, ApiDispatcher, RETRY_AFTER_DATA};
pub use fetch::{FETCH_TIMEOUT, FetchError, Fetched, MAX_DOCUMENT_BYTES, fetch};
pub use read::{ReadApi, ReadError, ReadOptions, Skipped, read};
