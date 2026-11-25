//! The integration tests of `rewind-runtime`, built as one test binary: each file of `tests/` is a
//! module (archi/08-decisions.md, "Taken for the development loop").

// What every file of tests/ allows, for the modules they share.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cpu;
mod handlers;
mod memory;
mod sandbox;
mod stress;
mod support;
mod workflows;
