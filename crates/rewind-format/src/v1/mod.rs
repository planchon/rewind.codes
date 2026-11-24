//! Format v1. Frozen: a change here must keep every stored v1 document decoding to the same
//! domain values (the golden fixtures in `tests/` pin it).

mod action_set;
mod cassette;
mod events;
pub(crate) mod partition;
mod test;

pub(crate) use action_set::{
    action_set_document, decode_action_set, decode_workflow, workflow_document,
};
pub(crate) use cassette::{decode_cassette, encode_cassette};
pub(crate) use events::{EventV1, HistoryEventV1, decode_event, decode_history_event};
pub(crate) use test::{decode_test, test_document};
