//! What leaves Rewind for a tenant's servers (`archi/07-code-architecture.md`, "Crates";
//! `archi/04-tools.md`, "Where MCP servers and APIs may be" and "Signed calls";
//! `archi/06-storage-and-scale.md`, "Security").
//!
//! - **The egress checks** ([`Egress`]), one implementation for every request a deployment
//!   sends where a tenant says (the `http` tool, MCP servers, APIs, OpenAPI documents, lifecycle
//!   webhooks): a name resolved once, private, loopback, link-local and cloud-metadata addresses
//!   refused unless the operator allows them ([`InternalAllowList`], `--allow-internal-tools`),
//!   the connection to the checked address.
//! - **The HTTP client of a tenant's servers** ([`ToolEgress::client`]), built on them: redirects
//!   followed only within the server's origin, with a 307 or a 308, up to 5 times, each checked
//!   again ([`Redirects`]), and whether it reached an internal server, whose answers are never
//!   quoted to the tenant ([`Reached`]). The MCP client's streamable HTTP transport, the API
//!   dispatcher and the reading of OpenAPI documents share it.
//! - **Placing a secret** ([`place`]): in a header, with its format and encoding, or in a
//!   variable of a stdio server.
//! - **Signing** ([`Signer`]): one EdDSA JWT per HTTP request, with the tenant's key's `kid`,
//!   from the engine's claims ([`CallIdentity`]) and the request's own (`aud`, `htm`, `htu`,
//!   `body`, `iat`, `exp`, `jti`); the JWK a server checks it with ([`jwk`]), and the checks a
//!   receiving server does ([`verify`]).
//!
//! It depends on no other crate of Rewind but `rewind-core`: what the engine resolves
//! (`CallAuth`) is translated by the dispatchers that use it.

#![forbid(unsafe_code)]

mod client;
mod egress;
mod secret;
mod signing;

pub use client::{MAX_REDIRECTS, Reached, Redirects, ToolEgress};
pub use egress::{
    AddressPolicy, Egress, InternalAllowList, NameResolver, Reach, Refused, Resolving,
    SystemResolver, domain, forbidden, is_subdomain,
};
pub use secret::{Placed, PlacementError, place};
pub use signing::{
    CallIdentity, Expected, SignedRequest, Signer, SigningError, TOKEN_LIFETIME, body_digest, jwk,
    kid_of, verify,
};
