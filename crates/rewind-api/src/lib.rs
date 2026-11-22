//! The HTTP API `/v1` of Rewind, as a contract (`archi/07-code-architecture.md`, "HTTP API
//! `/v1`").
//!
//! `rewind-server` serves this API, and the CLI, the TypeScript SDK and its agent tools, the
//! dashboard and the evals are its clients. This crate holds what they share, and no server
//! code:
//!
//! - the DTOs of every endpoint, request and response, one module per group of the endpoint
//!   table: [`toolsets`], [`workflows`], [`scripts`], [`runs`] (with [`events`], the journal
//!   entries and the event stream), [`tasks`], [`webhooks`], [`reading`], [`organization`],
//!   [`access`] (with the catalog of permissions and the built-in roles), [`namespaces`] (the
//!   end users, their quotas and their tokens), [`connections`] (a person's connections and the
//!   consent) and [`admin`] (the server); the dashboard's sign-in, [`auth`]; and the OAuth
//!   authorization server of the connections, [`oauth`];
//! - errors as `application/problem+json` ([`Problem`]), with a detail written for a model;
//! - the query language of the run lists ([`query`]);
//! - the headers ([`headers`]): the Bearer token, `Idempotency-Key`, `Rewind-Namespace`,
//!   `Rewind-Organization`, the webhook signature;
//! - the paths ([`routes`]);
//! - the OpenAPI 3.1 document generated from the DTOs ([`openapi`]), committed at
//!   `api/openapi.json`, from which the TypeScript SDK and the dashboard generate their types;
//! - a thin async [`Client`] for every endpoint of `/v1`, over HTTP(S) with a Bearer token, or
//!   over the embedded server's unix socket without one.
//!
//! JSON conventions: fields are camelCase, enumerated values are snake_case (as in the
//! journal), times are RFC 3339 strings, durations are written as scripts write them
//! (`"30s"`, `"1h30m"`) or as a number of milliseconds.
//!
//! Query strings: the query DTOs serialize as the server reads them; a list repeats its key
//! (`?label=agent:triage&label=conversation:c-42`), so a server reads them with a deserializer
//! that accepts repeated keys (`serde_html_form`, behind axum-extra's `Query`).

#![forbid(unsafe_code)]

pub mod access;
pub mod admin;
pub mod auth;
mod client;
pub mod common;
pub mod connections;
mod error;
pub mod events;
pub mod headers;
pub mod namespaces;
pub mod oauth;
pub mod openapi;
pub mod organization;
pub mod problem;
pub mod query;
pub mod reading;
pub mod routes;
pub mod runs;
pub mod scripts;
pub mod tasks;
pub mod testing;
pub mod toolsets;
pub mod webhooks;
pub mod workflows;

pub use client::{ChangeStream, Client, EventStream};
pub use error::Error;
pub use problem::{Problem, ProblemType};
