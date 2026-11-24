//! The cassette document, v1 (`archi/05-testing-and-observability.md`, "Test runs").
//!
//! ```json
//! {"v":1,"matching":"sequential","calls":[
//! {"action":"github.get_issue","args":{"number":4},"outcome":{"result":{"title":"Crash"}}},
//! {"action":"slack.post","args":{"text":"…"},"outcome":{"error":{"kind":"error","message":"rate limited"}}}
//! ],"signals":[
//! {"name":"approval","payload":{"close":true},"elapsed_ms":3600000}
//! ]}
//! ```
//!
//! A cassette is self-contained and meant to be read and edited by people: args, results and
//! payloads are plain JSON values, written in their canonical form, one call or signal per
//! line. A failure is written as in a `call_failed` event.

use rewind_core::{ActionOutcome, Cassette, Matching, RecordedCall, RecordedSignal};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::events::{ActionFailureV1, CanonicalJson};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallV1 {
    action: String,
    args: CanonicalJson,
    outcome: OutcomeV1,
}

/// `{"result": …}` or `{"error": {"kind": …, "message": …, "data": …}}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OutcomeV1 {
    Result(CanonicalJson),
    Error(ActionFailureV1),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalV1 {
    name: String,
    payload: CanonicalJson,
    elapsed_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum MatchingV1 {
    Sequential,
    ByArgs,
}

/// A cassette's fields without its version, as a test document holds its world's cassette.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CassetteBodyV1 {
    matching: MatchingV1,
    calls: Vec<CallV1>,
    signals: Vec<SignalV1>,
}

impl From<&Cassette> for CassetteBodyV1 {
    fn from(cassette: &Cassette) -> Self {
        Self {
            matching: matching_dto(cassette.matching),
            calls: cassette.calls.iter().map(call_dto).collect(),
            signals: cassette.signals.iter().map(signal_dto).collect(),
        }
    }
}

impl From<CassetteBodyV1> for Cassette {
    fn from(body: CassetteBodyV1) -> Self {
        from_dtos(body.matching, body.calls, body.signals)
    }
}

fn matching_dto(matching: Matching) -> MatchingV1 {
    match matching {
        Matching::Sequential => MatchingV1::Sequential,
        Matching::ByArgs => MatchingV1::ByArgs,
    }
}

pub(crate) fn encode_cassette(cassette: &Cassette) -> String {
    let matching = matching_dto(cassette.matching);
    let mut out = format!(
        "{{\"v\":{},\"matching\":{},\"calls\":[",
        crate::VERSION,
        crate::to_json(&matching)
    );
    lines(&mut out, cassette.calls.iter().map(call_dto));
    out.push_str("],\"signals\":[");
    lines(&mut out, cassette.signals.iter().map(signal_dto));
    out.push_str("]}\n");
    out
}

/// Decodes the fields of a v1 cassette, its version already taken out.
pub(crate) fn decode_cassette(mut fields: Map<String, Value>) -> Result<Cassette, String> {
    let matching: MatchingV1 = take(&mut fields, "matching")?;
    let calls: Vec<CallV1> = take(&mut fields, "calls")?;
    let signals: Vec<SignalV1> = take(&mut fields, "signals")?;
    if let Some(unknown) = fields.keys().next() {
        return Err(format!("unknown field {unknown:?}"));
    }
    Ok(from_dtos(matching, calls, signals))
}

fn from_dtos(matching: MatchingV1, calls: Vec<CallV1>, signals: Vec<SignalV1>) -> Cassette {
    Cassette {
        calls: calls
            .into_iter()
            .map(|call| RecordedCall {
                action: call.action,
                args: call.args.0,
                outcome: match call.outcome {
                    OutcomeV1::Result(CanonicalJson(result)) => ActionOutcome::Result(result),
                    OutcomeV1::Error(failure) => ActionOutcome::Failure(failure.into()),
                },
            })
            .collect(),
        signals: signals
            .into_iter()
            .map(|signal| RecordedSignal {
                name: signal.name,
                payload: signal.payload.0,
                elapsed_ms: signal.elapsed_ms,
            })
            .collect(),
        matching: match matching {
            MatchingV1::Sequential => Matching::Sequential,
            MatchingV1::ByArgs => Matching::ByArgs,
        },
    }
}

fn call_dto(call: &RecordedCall) -> CallV1 {
    CallV1 {
        action: call.action.clone(),
        args: CanonicalJson(call.args.clone()),
        outcome: match &call.outcome {
            ActionOutcome::Result(result) => OutcomeV1::Result(CanonicalJson(result.clone())),
            ActionOutcome::Failure(failure) => OutcomeV1::Error(failure.into()),
        },
    }
}

fn signal_dto(signal: &RecordedSignal) -> SignalV1 {
    SignalV1 {
        name: signal.name.clone(),
        payload: CanonicalJson(signal.payload.clone()),
        elapsed_ms: signal.elapsed_ms,
    }
}

/// Writes one item per line between the brackets of an array.
fn lines<T: Serialize>(out: &mut String, items: impl Iterator<Item = T>) {
    let mut empty = true;
    for item in items {
        out.push_str(if empty { "\n" } else { ",\n" });
        out.push_str(&crate::to_json(&item));
        empty = false;
    }
    out.push('\n');
}

fn take<T: for<'de> Deserialize<'de>>(
    fields: &mut Map<String, Value>,
    name: &str,
) -> Result<T, String> {
    let value = fields
        .remove(name)
        .ok_or_else(|| format!("missing field {name:?}"))?;
    serde_json::from_value(value).map_err(|err| format!("{name}: {err}"))
}
