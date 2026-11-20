//! What leaves Rewind for a tenant's servers (`archi/07-code-architecture.md`, "Crates";
//! `archi/04-tools.md`, "Where MCP servers and APIs may be" and "Signed calls").
//!
//! - **The egress checks**, one implementation for every request a deployment sends where a
//!   tenant says (MCP servers, APIs, lifecycle webhooks): a name resolved once, private,
//!   loopback, link-local and cloud-metadata addresses refused unless the operator allows them
//!   (`--allow-internal-tools`), the connection to the checked address, a redirect followed only
//!   within the origin with a 307 or a 308, up to 5 times, each checked again.
//! - **The HTTP client** built on them, which the MCP client's streamable HTTP transport, the
//!   API dispatcher and the webhook sender share.
//! - **The signing of signed calls**: a JWT (`EdDSA`, the tenant's key's `kid`) per HTTP
//!   request, from the engine's claims (`iss`, `sub`, `labels`, `run`, `tool`,
//!   `idempotency_key`, the attempt) and the request's own (`aud`, `htm`, `htu`, `body`, `iat`,
//!   `exp`, `jti`).
//!
//! A skeleton: the egress checks still live in `rewind-mcp` until the outbound workstream moves
//! them here (`archi/09-roadmap.md`).

#![forbid(unsafe_code)]
