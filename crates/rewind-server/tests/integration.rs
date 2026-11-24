//! The integration tests of `rewind-server`, built as one test binary: each file of `tests/` is a
//! module (archi/08-decisions.md, "Taken for the development loop").

// What every file of tests/ allows, for the modules they share.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod access;
mod api;
mod api_tools;
mod changes;
mod connections;
mod handles;
mod limits;
mod link;
mod master_keys;
mod mcp_egress;
mod migrate_runs;
mod namespaces;
mod nodes;
mod organizations;
mod secrets;
mod secrets_end_to_end;
mod sign_in;
mod stop;
mod suites;
mod support;
mod workflows;
