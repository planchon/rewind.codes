//! The idempotency keys of the tool calls, as the SDK makes them (`idempotencyKeyOf`,
//! `runIdempotencyKey` in `sdk/typescript/src/agent-tools/agent-tools.ts`).

use std::collections::BTreeMap;

use rewind_api::headers::IdempotencyKey;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// The idempotency key of a tool call: its id when the API takes it as a key, otherwise the
/// SHA-256 of the id, so the same tool call always gives the same key. `signal_run` uses it: a
/// signal's key is unique per run. `None` for a call without an id.
pub fn idempotency_key(call_id: Option<&str>) -> Option<IdempotencyKey> {
    let call_id = call_id.filter(|id| !id.is_empty())?;
    IdempotencyKey::new(call_id)
        .or_else(|_| IdempotencyKey::new(sha256_hex(call_id)))
        .ok()
}

/// The idempotency key of the run a tool call starts (`run_workflow`, `retry_run`): the SHA-256
/// of `[labels, call_id]` in compact JSON, the labels as `[key, value]` pairs sorted by key. A
/// run's key is unique per tenant, and harnesses number their tool calls alike (`call_0`,
/// `call_1`…): agents with other labels never share a key. `None` for a call without an id.
pub fn run_key(labels: &BTreeMap<String, String>, call_id: Option<&str>) -> Option<IdempotencyKey> {
    let call_id = call_id.filter(|id| !id.is_empty())?;
    let scope: Vec<Value> = labels
        .iter()
        .map(|(key, value)| json!([key, value]))
        .collect();
    IdempotencyKey::new(sha256_hex(&json!([scope, call_id]).to_string())).ok()
}

/// The SHA-256 of a text's UTF-8, in hexadecimal: a code's digest, as the server computes it.
pub(crate) fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Option<IdempotencyKey>) -> String {
        key.map(|key| key.as_str().to_owned()).unwrap_or_default()
    }

    #[test]
    fn tool_call_ids_become_keys() {
        assert_eq!(key(idempotency_key(Some("toolu_01AbC"))), "toolu_01AbC");
        let hashed = key(idempotency_key(Some("call\u{e9}")));
        assert_eq!(hashed.len(), 64);
        assert_eq!(hashed, key(idempotency_key(Some("call\u{e9}"))));
        assert!(idempotency_key(None).is_none());
        assert!(idempotency_key(Some("")).is_none());
    }

    #[test]
    fn a_run_s_key_is_scoped_by_the_agent_s_labels() {
        let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        // The same vectors as the SDK's tests (`sdk/typescript/test/agent-tools.test.ts`).
        assert_eq!(
            key(run_key(&labels(&[("agent", "a")]), Some("call_0"))),
            "60b258a9b3d4fedc3e76a4687e2f18f3a6b35dd43a5afde3743c2484c38598a5"
        );
        assert_eq!(
            key(run_key(&labels(&[("agent", "b")]), Some("call_0"))),
            "57b1f343b2298f8dca3d00127f9f7b7b280934708c580ea73c339dff509f7eb1"
        );
        assert_eq!(
            key(run_key(&labels(&[("b", "2"), ("a", "1")]), Some("call_0"))),
            "3976d8a6de96175c872c79d0bba946217ef5b466d838926513bd53cdee626186"
        );
        assert!(run_key(&labels(&[]), None).is_none());
    }
}
