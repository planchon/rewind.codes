//! The test document, v1 (`archi/05-testing-and-observability.md`, "Test runs").
//!
//! ```json
//! {"expect":{"calls":[{"times":1,"action":"github.close_issue"}],"output":{"closed":1},
//! "status":"completed"},"input":{"repo":"acme/api"},"name":"closes once approved",
//! "signals":[{"name":"approval","payload":{"close":true}}],"v":1,
//! "world":{"actions":{"github.close_issue":[{"returns":{}}]}}}
//! ```
//!
//! Written in its canonical form: its bytes hash to the test's digest, the key of its blob. A
//! field that holds its default is left out (`input` `{}`, a signal sent `"waiting"`, the order
//! `"any"`), so one test has one document. An action's single rule may be written without its
//! array, and is written back in one. Times are milliseconds, the seed lowercase hex. The
//! world's cassette is a cassette document without its version.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    ActionFailure, ActionFailureKind, ActionOutcome, ActionRule, CallOrder, Expectations,
    ExpectedCall, ExpectedError, RunId, RunStatus, Seed, SignalWhen, Test, TestFromRun, TestSignal,
    Timestamp, World,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use super::cassette::CassetteBodyV1;
use super::events::{ActionFailureKindV1, CanonicalJson, digest, hex, unhex};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestV1 {
    name: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    input: Option<CanonicalJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    world: Option<WorldV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    signals: Vec<SignalV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect: Option<ExpectV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<FromRunV1>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorldV1 {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    actions: BTreeMap<String, RulesV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handlers: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cassette: Option<CassetteBodyV1>,
}

/// An action's rules: an array, or one rule on its own when read.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RulesV1 {
    Many(Vec<RuleV1>),
    One(Box<RuleV1>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    args: Option<CanonicalJson>,
    /// Present even when the result is `null`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    returns: Option<CanonicalJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RuleErrorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    times: Option<u32>,
}

/// A failure, whose `kind` is `error` when left out.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleErrorV1 {
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<ActionFailureKindV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<CanonicalJson>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalV1 {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<CanonicalJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<WhenV1>,
}

/// `"waiting"`, or milliseconds of virtual time after the start.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WhenV1 {
    After(u64),
    Waiting(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<StatusV1>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    output: Option<CanonicalJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ExpectedErrorV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    calls: Vec<ExpectedCallV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<OrderV1>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StatusV1 {
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Aborted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedErrorV1 {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedCallV1 {
    action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    args: Option<CanonicalJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    times: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum OrderV1 {
    Any,
    Listed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FromRunV1 {
    run: String,
    version: String,
    namespace: String,
}

/// A field given, `null` included: `Some`, where a missing field is `None` through `default`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<CanonicalJson>, D::Error> {
    Value::deserialize(deserializer).map(|value| Some(CanonicalJson(value)))
}

/// The test document v1, as a JSON value, `"v": 1` included.
pub(crate) fn test_document(test: &Test) -> Value {
    let dto = TestV1 {
        name: test.name.clone(),
        input: (test.input != json!({})).then(|| CanonicalJson(test.input.clone())),
        start: test.start.map(|start| start.0),
        seed: test.seed.map(|seed| hex(seed.as_bytes())),
        world: (!test.world.is_empty()).then(|| world_dto(&test.world)),
        signals: test.signals.iter().map(signal_dto).collect(),
        expect: (!test.expect.is_empty()).then(|| expect_dto(&test.expect)),
        from: test.from.as_ref().map(|from| FromRunV1 {
            run: from.run.to_string(),
            version: from.version.to_hex(),
            namespace: from.namespace.clone(),
        }),
    };
    let mut document = serde_json::to_value(dto).expect("test DTOs always serialize");
    if let Value::Object(fields) = &mut document {
        fields.insert("v".into(), Value::from(crate::VERSION));
    }
    document
}

/// Decodes the fields of a v1 test document, its version already taken out, and checks the
/// test's invariants ([`Test::check`]).
pub(crate) fn decode_test(document: Value) -> Result<Test, String> {
    let dto: TestV1 = serde_json::from_value(document).map_err(|err| err.to_string())?;
    let world = match dto.world {
        Some(world) => world_from_dto(world)?,
        None => World::default(),
    };
    let test = Test {
        name: dto.name,
        input: dto
            .input
            .map_or_else(|| json!({}), |CanonicalJson(input)| input),
        start: dto.start.map(Timestamp),
        seed: dto
            .seed
            .map(|seed| {
                unhex(&seed).map(Seed::from_bytes).ok_or_else(|| {
                    format!("invalid seed {seed:?}: expected 64 lowercase hex digits")
                })
            })
            .transpose()?,
        world,
        signals: dto
            .signals
            .into_iter()
            .map(signal_from_dto)
            .collect::<Result<_, _>>()?,
        expect: dto.expect.map(expect_from_dto).unwrap_or_default(),
        from: dto
            .from
            .map(|from| {
                Ok::<_, String>(TestFromRun {
                    run: from
                        .run
                        .parse::<RunId>()
                        .map_err(|err| format!("\"from.run\": {err}"))?,
                    version: digest(&from.version)?,
                    namespace: from.namespace,
                })
            })
            .transpose()?,
    };
    test.check().map_err(|err| err.to_string())?;
    Ok(test)
}

fn world_dto(world: &World) -> WorldV1 {
    WorldV1 {
        actions: world
            .actions
            .iter()
            .map(|(action, rules)| {
                (
                    action.clone(),
                    RulesV1::Many(rules.iter().map(rule_dto).collect()),
                )
            })
            .collect(),
        handlers: world.handlers.clone(),
        cassette: world.cassette.as_ref().map(CassetteBodyV1::from),
    }
}

fn world_from_dto(world: WorldV1) -> Result<World, String> {
    let mut actions = BTreeMap::new();
    for (action, rules) in world.actions {
        let rules = match rules {
            RulesV1::Many(rules) => rules,
            RulesV1::One(rule) => vec![*rule],
        };
        let rules = rules
            .into_iter()
            .enumerate()
            .map(|(position, rule)| {
                rule_from_dto(rule)
                    .map_err(|err| format!("rule {} of {action}: {err}", position + 1))
            })
            .collect::<Result<_, _>>()?;
        actions.insert(action, rules);
    }
    Ok(World {
        actions,
        handlers: world.handlers,
        cassette: world.cassette.map(Into::into),
    })
}

fn rule_dto(rule: &ActionRule) -> RuleV1 {
    let (returns, error) = match &rule.answer {
        ActionOutcome::Result(result) => (Some(CanonicalJson(result.clone())), None),
        ActionOutcome::Failure(failure) => (
            None,
            Some(RuleErrorV1 {
                message: failure.message.clone(),
                kind: (failure.kind != ActionFailureKind::Error).then(|| failure.kind.into()),
                data: failure.data.clone().map(CanonicalJson),
            }),
        ),
    };
    RuleV1 {
        args: rule.args.clone().map(CanonicalJson),
        returns,
        error,
        times: rule.times,
    }
}

fn rule_from_dto(rule: RuleV1) -> Result<ActionRule, String> {
    let answer = match (rule.returns, rule.error) {
        (Some(CanonicalJson(result)), None) => ActionOutcome::Result(result),
        (None, Some(error)) => ActionOutcome::Failure(ActionFailure {
            kind: error.kind.map_or(ActionFailureKind::Error, Into::into),
            message: error.message,
            data: error.data.map(|CanonicalJson(data)| data),
        }),
        (Some(_), Some(_)) => return Err("give \"returns\" or \"error\", not both".into()),
        (None, None) => return Err("give \"returns\" or \"error\"".into()),
    };
    Ok(ActionRule {
        args: rule.args.map(|CanonicalJson(args)| args),
        answer,
        times: rule.times,
    })
}

fn signal_dto(signal: &TestSignal) -> SignalV1 {
    SignalV1 {
        name: signal.name.clone(),
        payload: (!signal.payload.is_null()).then(|| CanonicalJson(signal.payload.clone())),
        when: match signal.when {
            SignalWhen::Waiting => None,
            SignalWhen::After(after) => Some(WhenV1::After(
                u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
            )),
        },
    }
}

fn signal_from_dto(signal: SignalV1) -> Result<TestSignal, String> {
    let when = match signal.when {
        None => SignalWhen::Waiting,
        Some(WhenV1::Waiting(when)) if when == "waiting" => SignalWhen::Waiting,
        Some(WhenV1::Waiting(when)) => {
            return Err(format!(
                "signal {}: \"when\" is \"waiting\" or a number of milliseconds, not {when:?}",
                signal.name
            ));
        }
        Some(WhenV1::After(ms)) => SignalWhen::After(Duration::from_millis(ms)),
    };
    Ok(TestSignal {
        name: signal.name,
        payload: signal
            .payload
            .map_or(Value::Null, |CanonicalJson(payload)| payload),
        when,
    })
}

fn expect_dto(expect: &Expectations) -> ExpectV1 {
    ExpectV1 {
        status: expect.status.map(status_dto),
        output: expect.output.clone().map(CanonicalJson),
        error: expect.error.as_ref().map(|error| ExpectedErrorV1 {
            name: error.name.clone(),
            message: error.message.clone(),
        }),
        calls: expect
            .calls
            .iter()
            .map(|call| ExpectedCallV1 {
                action: call.action.clone(),
                args: call.args.clone().map(CanonicalJson),
                times: call.times,
            })
            .collect(),
        order: match expect.order {
            CallOrder::Any => None,
            CallOrder::Listed => Some(OrderV1::Listed),
        },
    }
}

fn expect_from_dto(expect: ExpectV1) -> Expectations {
    Expectations {
        status: expect.status.map(status_from_dto),
        output: expect.output.map(|CanonicalJson(output)| output),
        error: expect.error.map(|error| ExpectedError {
            name: error.name,
            message: error.message,
        }),
        calls: expect
            .calls
            .into_iter()
            .map(|call| ExpectedCall {
                action: call.action,
                args: call.args.map(|CanonicalJson(args)| args),
                times: call.times,
            })
            .collect(),
        order: match expect.order {
            None | Some(OrderV1::Any) => CallOrder::Any,
            Some(OrderV1::Listed) => CallOrder::Listed,
        },
    }
}

fn status_dto(status: RunStatus) -> StatusV1 {
    match status {
        RunStatus::Queued => StatusV1::Queued,
        RunStatus::Running => StatusV1::Running,
        RunStatus::Waiting => StatusV1::Waiting,
        RunStatus::Completed => StatusV1::Completed,
        RunStatus::Failed => StatusV1::Failed,
        RunStatus::Cancelled => StatusV1::Cancelled,
        RunStatus::Aborted => StatusV1::Aborted,
    }
}

fn status_from_dto(status: StatusV1) -> RunStatus {
    match status {
        StatusV1::Queued => RunStatus::Queued,
        StatusV1::Running => RunStatus::Running,
        StatusV1::Waiting => RunStatus::Waiting,
        StatusV1::Completed => RunStatus::Completed,
        StatusV1::Failed => RunStatus::Failed,
        StatusV1::Cancelled => RunStatus::Cancelled,
        StatusV1::Aborted => RunStatus::Aborted,
    }
}
