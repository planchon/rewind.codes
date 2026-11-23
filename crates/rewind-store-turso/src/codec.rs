//! How values the store keeps in columns are written: JSON values in their canonical form, label
//! maps, a role's permissions, principals and the event types of a webhook subscription. The same
//! encodings as the production store's, so that a value reads the same whatever the backend.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{JsonValue, canonical_json};
use rewind_engine::ports::{Principal, WebhookEventType};
use serde_json::{Map, Value};

use crate::Error;

fn corrupt(what: &str, found: &str) -> Error {
    Error::Corrupt(format!("unknown {what} {found:?}"))
}

pub(crate) fn decode_value(json: &str) -> Result<JsonValue, Error> {
    serde_json::from_str(json).map_err(|err| Error::Corrupt(format!("stored JSON value: {err}")))
}

// -------------------------------------------------------------------------------------------
// Labels

/// A string map, as a JSON object.
pub(crate) fn encode_labels(labels: &BTreeMap<String, String>) -> String {
    let object: Map<String, Value> = labels
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    canonical_json(&Value::Object(object))
}

pub(crate) fn decode_labels(json: &str) -> Result<BTreeMap<String, String>, Error> {
    let Value::Object(object) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("labels {json:?}")));
    };
    object
        .into_iter()
        .map(|(key, value)| match value {
            Value::String(value) => Ok((key, value)),
            other => Err(Error::Corrupt(format!("label {key:?} is {other}"))),
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Access

/// A role's permissions: a JSON array of strings, sorted.
pub(crate) fn encode_permissions(permissions: &BTreeSet<String>) -> String {
    let names: Vec<Value> = permissions.iter().cloned().map(Value::String).collect();
    canonical_json(&Value::Array(names))
}

pub(crate) fn decode_permissions(json: &str) -> Result<BTreeSet<String>, Error> {
    let Value::Array(names) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("permissions {json:?}")));
    };
    names
        .into_iter()
        .map(|name| match name {
            Value::String(name) => Ok(name),
            other => Err(Error::Corrupt(format!("permission {other}"))),
        })
        .collect()
}

pub(crate) fn parse_principal(text: &str) -> Result<Principal, Error> {
    Principal::parse(text).map_err(|err| Error::Corrupt(err.to_string()))
}

// -------------------------------------------------------------------------------------------
// Webhook subscriptions

pub(crate) fn encode_event_types(events: &BTreeSet<WebhookEventType>) -> String {
    let names: Vec<Value> = events
        .iter()
        .map(|event| Value::String(event.as_str().to_owned()))
        .collect();
    canonical_json(&Value::Array(names))
}

pub(crate) fn decode_event_types(json: &str) -> Result<BTreeSet<WebhookEventType>, Error> {
    let Value::Array(names) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("webhook events {json:?}")));
    };
    names
        .iter()
        .map(|name| {
            let name = name.as_str().unwrap_or_default();
            WebhookEventType::ALL
                .into_iter()
                .find(|event| event.as_str() == name)
                .ok_or_else(|| corrupt("webhook event", name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip() {
        let labels = BTreeMap::from([
            ("agent".to_owned(), "triage".to_owned()),
            ("a,b".to_owned(), "c\"d".to_owned()),
        ]);
        assert_eq!(decode_labels(&encode_labels(&labels)), Ok(labels));
        assert!(decode_labels("{\"n\":1}").is_err());
    }

    #[test]
    fn permissions_and_event_types_round_trip() {
        let permissions = BTreeSet::from(["runs:read".to_owned(), "runs:write".to_owned()]);
        assert_eq!(
            decode_permissions(&encode_permissions(&permissions)),
            Ok(permissions)
        );
        let events: BTreeSet<WebhookEventType> = WebhookEventType::ALL.into_iter().collect();
        assert_eq!(decode_event_types(&encode_event_types(&events)), Ok(events));
        assert!(decode_event_types("[\"run.paused\"]").is_err());
    }
}
