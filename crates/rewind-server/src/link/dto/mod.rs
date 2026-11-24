//! The wire form of the node link: JSON DTOs of every message the nodes of a deployment send each
//! other (`archi/07-code-architecture.md`, principle 4: "Between nodes, the node link's DTOs in
//! `rewind-server`").
//!
//! The DTOs are internal to a deployment, whose nodes all run the same link: the whole link is
//! versioned by one number, [`LINK_VERSION`], which every request carries in the header
//! [`VERSION_HEADER`] and which a node refuses when it differs from its own. A change to any DTO
//! changes the number.
//!
//! What the stored format already defines is written as its stored record
//! (`rewind_format::partition`, converted by `rewind_store_slatedb::codec`): a run head, a
//! history entry (its event in the stored format of events), an inbox item, a tool task, a timer,
//! a payload record, a summary, counters, gauges, a shape, an outbox entry's projection, run
//! metadata; an event alone (`run_started`) as a stored event, a sealed attempt as a journal
//! document and a pinned action set as its document. The rest is defined here:
//!
//! - [`requests`]: [`NodeRequest`] and [`NodeReply`], with what an edge asks an owner
//!   ([`PartitionRequest`], [`PartitionReply`], [`Refusal`]), what an owner asks an organization
//!   actor ([`OrgRequest`], [`OrgReply`]), the notices ([`OwnerNotice`], [`EdgeNotice`]), the
//!   subscriptions ([`Subscribe`], [`Changed`]);
//! - [`activations`]: [`ActivationRequest`] and [`ActivationResult`], for a remote executor;
//! - [`values`]: what both carry (a run, a task, a query, a page of a search, counters).
//!
//! Each DTO converts from the engine's type (`From<&T>`) and back (`TryFrom`, refusing what the
//! engine refuses: a tenant, a namespace, a node id, an event that is not the one expected), and
//! the round trip gives back the value it started from.

pub mod activations;
pub mod requests;
pub mod values;

#[cfg(test)]
mod tests;

use std::fmt;

use rewind_core::{Digest, RunId};
use rewind_engine::ports::{NamespaceName, NodeId, PartitionError, TenantId};
use serde::de::{Deserializer, Error as _};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

pub use activations::{
    Activation, ActivationRequest, ActivationResult, Child, Committed, CounterDelta, Measurement,
    ParentHistory, Purpose, SealedAttempt, StoredActionSet, StoredScript, TimerChange,
};
pub use requests::{
    Admit, Changed, Counted, CountsReport, Created, Decision, EdgeNotice, Enqueued, HostFailure,
    HostTask, HostTaskOffer, LostTask, NodeReply, NodeRequest, OrgReply, OrgRequest, OwnerNotice,
    PartitionReply, PartitionRequest, Refusal, SignalAtOwner, SlotRequest, StartAtOwner,
    StreamAttach, Subscribe, SubscriptionKey, TaskAnswer,
};
pub use values::{
    Bound, Counters, DayUsage, Found, Month, Page, Predicate, QuotaExceeded, QuotaKind, QuotaScope,
    Range, RunOrder, RunQuery, RunRef, RunView, Scope, SearchCursor, TaskRef, Term, TextMatch,
    Usage, Window,
};

/// The version of the node link this build speaks: every request carries it, and a node refuses
/// a request of another version.
pub const LINK_VERSION: u32 = 1;

/// The header that carries [`LINK_VERSION`] on every request.
pub const VERSION_HEADER: &str = "rewind-link-version";

/// A message of the node link that the engine refuses: a tenant, a namespace, a node id or a
/// record that does not read back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid message of the node link: {0}")]
pub struct Invalid(pub String);

impl Invalid {
    pub(crate) fn new(message: impl fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl From<PartitionError> for Invalid {
    fn from(error: PartitionError) -> Self {
        Self::new(error)
    }
}

pub(crate) fn tenant(text: String) -> Result<TenantId, Invalid> {
    TenantId::new(text).map_err(Invalid::new)
}

/// A namespace as the owners send it: a namespace's name, or the name under which a partition
/// keeps what its tenants' erased namespaces consumed ([`NamespaceName::erased`]).
pub(crate) fn namespace(text: String) -> Result<NamespaceName, Invalid> {
    NamespaceName::stored(text).map_err(Invalid::new)
}

pub(crate) fn node(text: String) -> Result<NodeId, Invalid> {
    NodeId::new(text).map_err(Invalid::new)
}

/// A run id, written as its hyphenated text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(pub RunId);

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse::<RunId>()
            .map(Id)
            .map_err(|error| D::Error::custom(error.to_string()))
    }
}

/// A digest, written as its lowercase hexadecimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hex(pub Digest);

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Digest::from_hex(&String::deserialize(d)?)
            .map(Hex)
            .map_err(|error| D::Error::custom(error.to_string()))
    }
}

/// An optional JSON value whose `null` is a value: a field left out is `None`, a field present
/// is `Some`, `null` included. With `skip_serializing_if = "Option::is_none"` and `default`.
pub(crate) fn present<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<rewind_core::JsonValue>, D::Error> {
    rewind_core::JsonValue::deserialize(d).map(Some)
}
