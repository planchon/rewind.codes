//! `PartitionStore` on SlateDB over object storage (`archi/07-code-architecture.md`,
//! "`PartitionStore`"; `archi/06-storage-and-scale.md`, "A partition"): AWS S3 or any S3-compatible
//! service in production, local files embedded, memory in tests.
//!
//! - [`SlatePartitions`] opens a partition as its only writer: one SlateDB database at
//!   `partitions/{id}/`, the previous writer fenced by SlateDB's writer epoch.
//! - [`SlateLog`] reads with one point get or one range scan per call, and commits a batch as one
//!   SlateDB transaction, written as one write batch and durable when the commit returns.
//! - The records are the partition records of `rewind-format` ([`rewind_format::partition`]);
//!   their keys are [`keys`]'s, and [`codec`] converts the engine's types to and from them
//!   (public: the node link of `rewind-server` writes the same records).
//! - [`Options`] sets how SlateDB runs, and lists its timers: with the timer wheel, the only
//!   timers of the run plane.

#![forbid(unsafe_code)]

pub mod codec;
mod commit;
mod config;
mod error;
pub mod keys;
mod store;

pub use config::{Bucket, Options, S3Bucket, Timers};
pub use error::Error;
pub use store::{SlateLog, SlatePartitions};
