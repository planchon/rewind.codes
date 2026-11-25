//! The integration tests of `rewind-engine`, built as one test binary: each file of `tests/` is a
//! module (archi/08-decisions.md, "Taken for the development loop").

// What every file of tests/ allows, for the modules they share.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod calls;
mod cluster;
mod conformance;
mod control_plane;
mod executors;
mod handles;
mod migrate;
mod model;
mod plane;
mod projections;
mod runtime;
mod secrets;
mod starts;
mod suites;
mod support;
mod testing;
mod transcript;
mod workflows;
