//! Tests: the world a run meets and what the outside must see at the end
//! (`archi/05-testing-and-observability.md`, "Testing and mocks"). `POST /v1/test` runs them,
//! a workflow version carries them as its suite, and `POST /v1/workflows/{name}/record` records
//! them from past runs.
//!
//! The DTOs follow the test document of the stored format, with the API's conventions: times
//! in RFC 3339, durations `"2h"`. A test's world's cassette is a cassette document, as
//! `GET /v1/runs/{id}/journal?as=cassette` exports it. Converting to and from the domain takes
//! the cassette's codec, which the API does not know.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{ActionFailure, ActionOutcome, Cassette, Digest, Seed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::common::{AbortReason, ActionFailureKind, RunStatus, ScriptError, Timestamp, timestamp};

/// A test: what the run is given (its input, the actions' answers, signals), and what must hold
/// at the end. It never names its code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "name": "closes the stale issues once approved",
    "input": { "repo": "acme/api" },
    "world": { "actions": {
        "github.list_issues": [{ "args": { "state": "open" }, "returns": [{ "number": 4, "updated_at": "2025-10-01T00:00:00Z" }] }],
        "github.close_issue": { "returns": {} }
    } },
    "signals": [{ "name": "approval", "payload": { "close": true } }],
    "expect": {
        "status": "completed",
        "output": { "closed": 1 },
        "calls": [{ "action": "github.close_issue", "times": 1 }]
    }
}))]
pub struct Test {
    /// Unique within a suite.
    pub name: String,
    /// The run's input. `{}` when absent.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// When the run starts, in virtual time; 2026-01-01T00:00:00Z when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Timestamp>,
    /// The run's seed, 64 lowercase hex digits; the same fixed seed for every test when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    #[serde(default, skip_serializing_if = "World::is_empty")]
    pub world: World,
    /// Sent to the run in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<TestSignal>,
    #[serde(default)]
    pub expect: Expectations,
    /// Where a recorded test comes from; set by recording, refused in a test given inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TestOrigin>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// What answers the run's calls: the rules, then the mock handlers, then the cassette. A call
/// no source answers fails, and fails the test.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct World {
    /// By action, one answer for every call, or rules tried in order. A start of a workflow
    /// (`workflows.<name>.start`) is answered with the child's output, or its failure.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ActionRules>,
    /// TypeScript mock handlers, run in a separate isolate: a module default-exporting its
    /// handlers by action name or by namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mocks: Option<String>,
    /// Calls and signals recorded from a run: a cassette document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cassette: Option<Value>,
}

impl World {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.mocks.is_none() && self.cassette.is_none()
    }
}

/// One rule, or several tried in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ActionRules {
    Many(Vec<ActionRule>),
    One(Box<ActionRule>),
}

impl ActionRules {
    fn into_vec(self) -> Vec<ActionRule> {
        match self {
            ActionRules::Many(rules) => rules,
            ActionRules::One(rule) => vec![*rule],
        }
    }
}

/// An answer to the calls whose args contain `args`: `returns` (the result, `null` included) or
/// `error`, for `times` calls at most.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionRule {
    /// The call's args contain these (objects compared key by key); any call when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub args: Option<Value>,
    /// The result.
    #[serde(
        default,
        deserialize_with = "crate::common::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub returns: Option<Value>,
    /// The call fails with an `ActionError`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RuleError>,
    /// How many calls it answers; any number when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

/// A failure a rule answers with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleError {
    pub message: String,
    /// `error` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ActionFailureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A signal sent during a test.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestSignal {
    pub name: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// When it is sent: `"blocked"` (the default) once the run can go no further without the
    /// outside world; or a duration of virtual time after the start, such as `"2h"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// What must hold at the end of a test. Everything not given is not checked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Expectations {
    /// `active` for a run that can go no further in virtual time without the outside world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    /// The output contains these values (objects are compared key by key, recursively).
    #[serde(
        default,
        deserialize_with = "crate::common::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Value>,
    /// The run failed with an error of this name, whose message contains `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ExpectedError>,
    /// Calls that must happen: those to `action` whose args contain `args`, exactly `times` of
    /// them, at least one when `times` is absent; `times: 0` forbids them. Calls the world
    /// answers that are not listed are allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ExpectedCall>,
    /// `listed`: the first call matching each entry comes in the order of the entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<CallOrder>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedError {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedCall {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallOrder {
    Any,
    Listed,
}

/// The run a recorded test comes from. The test holds that run's data, and is read like it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestOrigin {
    pub run: String,
    /// The workflow version the run executed.
    pub version: String,
    pub namespace: String,
}

/// A test given to `POST /v1/test` or to a save: inline, by the digest of a stored test (one
/// recorded from a run), or as a stored test with a new expectation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum TestInput {
    /// The digest of a stored test.
    Digest(String),
    From(TestFrom),
    Inline(Box<Test>),
}

/// A stored test with a new expectation: a model corrects a recorded test without reading its
/// cassette.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestFrom {
    /// The digest of the stored test.
    pub from: String,
    pub expect: Expectations,
}

/// Why a test did not pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestProblem {
    /// It names actions, answers or args the action set no longer has. Not run.
    Stale,
    /// The workflow's input or signal schemas refuse it. Not run.
    Incompatible,
    /// It ran, and an expectation does not hold.
    Failed,
}

/// The outcome of a test.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<TestProblem>,
    /// What does not hold, one sentence each, written for a model.
    pub failures: Vec<String>,
    /// The run's status; absent for a test that was not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    #[serde(
        default,
        deserialize_with = "crate::common::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ScriptError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<AbortReason>,
    /// How many times each action was called.
    #[serde(default)]
    pub calls: BTreeMap<String, u64>,
    /// The LLM trace of the run; empty for a test that was not run.
    #[serde(default)]
    pub trace: String,
}

/// `POST /v1/test`: what the tests ran against, and their outcomes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestRun {
    /// Every test passed.
    pub passed: bool,
    /// In the order given, then the suite's.
    pub tests: Vec<TestResult>,
}

// -------------------------------------------------------------------------------------------
// Conversions

impl Test {
    /// The domain's test. `decode_cassette` reads a cassette document; the problems are
    /// written for a model.
    pub fn into_core(
        self,
        decode_cassette: impl Fn(&Value) -> Result<Cassette, String>,
    ) -> Result<rewind_core::Test, String> {
        let name = self.name;
        let start = self
            .start
            .map(|start| {
                u64::try_from(start.as_millisecond())
                    .map(rewind_core::Timestamp::from_millis)
                    .map_err(|_| format!("test {name:?}: the start is before 1970"))
            })
            .transpose()?;
        let seed = self
            .seed
            .map(|seed| parse_seed(&seed).map_err(|error| format!("test {name:?}: {error}")))
            .transpose()?;
        let mut actions = BTreeMap::new();
        for (action, rules) in self.world.actions {
            let rules = rules
                .into_vec()
                .into_iter()
                .enumerate()
                .map(|(position, rule)| {
                    rule.into_core().map_err(|error| {
                        format!("test {name:?}: rule {} of {action}: {error}", position + 1)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            actions.insert(action, rules);
        }
        let cassette = self
            .world
            .cassette
            .as_ref()
            .map(|cassette| {
                decode_cassette(cassette)
                    .map_err(|error| format!("test {name:?}: the cassette is not valid: {error}"))
            })
            .transpose()?;
        let signals = self
            .signals
            .into_iter()
            .map(|signal| {
                let when = match signal.when.as_deref() {
                    None | Some("blocked") => rewind_core::SignalWhen::Blocked,
                    Some(after) => rewind_core::SignalWhen::After(
                        after
                            .parse::<crate::common::Duration>()
                            .map(Duration::from)
                            .map_err(|_| {
                                format!(
                                    "test {name:?}: signal {}: `when` is \"blocked\" or a \
                                     duration such as \"2h\", not {after:?}",
                                    signal.name
                                )
                            })?,
                    ),
                };
                Ok(rewind_core::TestSignal {
                    name: signal.name,
                    payload: signal.payload,
                    when,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let origin = self
            .origin
            .map(|origin| {
                Ok::<_, String>(rewind_core::TestOrigin {
                    run: origin
                        .run
                        .parse()
                        .map_err(|error| format!("test {name:?}: origin.run: {error}"))?,
                    version: Digest::from_hex(&origin.version)
                        .map_err(|error| format!("test {name:?}: origin.version: {error}"))?,
                    namespace: origin.namespace,
                })
            })
            .transpose()?;
        let test = rewind_core::Test {
            name,
            input: self.input,
            start,
            seed,
            world: rewind_core::World {
                actions,
                mocks: self.world.mocks,
                cassette,
            },
            signals,
            expect: self.expect.into_core(),
            origin,
        };
        test.check().map_err(|error| error.to_string())?;
        Ok(test)
    }

    /// The DTO of a domain test. `encode_cassette` writes a cassette document.
    pub fn from_core(
        test: &rewind_core::Test,
        encode_cassette: impl Fn(&Cassette) -> Value,
    ) -> Self {
        Self {
            name: test.name.clone(),
            input: test.input.clone(),
            start: test.start.map(timestamp),
            seed: test.seed.map(|seed| hex(seed.as_bytes())),
            world: World {
                actions: test
                    .world
                    .actions
                    .iter()
                    .map(|(action, rules)| {
                        (
                            action.clone(),
                            ActionRules::Many(rules.iter().map(ActionRule::from_core).collect()),
                        )
                    })
                    .collect(),
                mocks: test.world.mocks.clone(),
                cassette: test.world.cassette.as_ref().map(encode_cassette),
            },
            signals: test
                .signals
                .iter()
                .map(|signal| TestSignal {
                    name: signal.name.clone(),
                    payload: signal.payload.clone(),
                    when: match signal.when {
                        rewind_core::SignalWhen::Blocked => None,
                        rewind_core::SignalWhen::After(after) => {
                            Some(crate::common::Duration::from(after).to_string())
                        }
                    },
                })
                .collect(),
            expect: Expectations::from_core(&test.expect),
            origin: test.origin.as_ref().map(|origin| TestOrigin {
                run: origin.run.to_string(),
                version: origin.version.to_hex(),
                namespace: origin.namespace.clone(),
            }),
        }
    }
}

impl ActionRule {
    fn into_core(self) -> Result<rewind_core::ActionRule, String> {
        let answer = match (self.returns, self.error) {
            (Some(result), None) => ActionOutcome::Result(result),
            (None, Some(error)) => ActionOutcome::Failure(ActionFailure {
                kind: error
                    .kind
                    .map_or(rewind_core::ActionFailureKind::Error, Into::into),
                message: error.message,
                data: error.data,
            }),
            (Some(_), Some(_)) => return Err("give `returns` or `error`, not both".into()),
            (None, None) => return Err("give `returns` or `error`".into()),
        };
        Ok(rewind_core::ActionRule {
            args: self.args,
            answer,
            times: self.times,
        })
    }

    fn from_core(rule: &rewind_core::ActionRule) -> Self {
        let (returns, error) = match &rule.answer {
            ActionOutcome::Result(result) => (Some(result.clone()), None),
            ActionOutcome::Failure(failure) => (
                None,
                Some(RuleError {
                    message: failure.message.clone(),
                    kind: (failure.kind != rewind_core::ActionFailureKind::Error)
                        .then(|| failure.kind.into()),
                    data: failure.data.clone(),
                }),
            ),
        };
        Self {
            args: rule.args.clone(),
            returns,
            error,
            times: rule.times,
        }
    }
}

impl Expectations {
    pub fn into_core(self) -> rewind_core::Expectations {
        rewind_core::Expectations {
            status: self.status.map(Into::into),
            output: self.output,
            error: self.error.map(|error| rewind_core::ExpectedError {
                name: error.name,
                message: error.message,
            }),
            calls: self
                .calls
                .into_iter()
                .map(|call| rewind_core::ExpectedCall {
                    action: call.action,
                    args: call.args,
                    times: call.times,
                })
                .collect(),
            order: match self.order {
                None | Some(CallOrder::Any) => rewind_core::CallOrder::Any,
                Some(CallOrder::Listed) => rewind_core::CallOrder::Listed,
            },
        }
    }

    pub fn from_core(expect: &rewind_core::Expectations) -> Self {
        Self {
            status: expect.status.map(Into::into),
            output: expect.output.clone(),
            error: expect.error.as_ref().map(|error| ExpectedError {
                name: error.name.clone(),
                message: error.message.clone(),
            }),
            calls: expect
                .calls
                .iter()
                .map(|call| ExpectedCall {
                    action: call.action.clone(),
                    args: call.args.clone(),
                    times: call.times,
                })
                .collect(),
            order: match expect.order {
                rewind_core::CallOrder::Any => None,
                rewind_core::CallOrder::Listed => Some(CallOrder::Listed),
            },
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_seed(text: &str) -> Result<Seed, String> {
    let digest = Digest::from_hex(text)
        .map_err(|_| format!("the seed is 64 lowercase hex digits, not {text:?}"))?;
    if digest.to_hex() != text {
        return Err(format!("the seed is 64 lowercase hex digits, not {text:?}"));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("the seed is 64 lowercase hex digits, not {text:?}"))?;
    }
    Ok(Seed::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_test_reads_inline_by_digest_or_from_a_stored_one() {
        let inputs: Vec<TestInput> = serde_json::from_value(json!([
            "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
            { "from": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
              "expect": { "status": "failed" } },
            { "name": "t", "world": { "actions": { "a.b": { "returns": null } } } },
        ]))
        .unwrap();
        assert!(matches!(inputs[0], TestInput::Digest(_)));
        assert!(matches!(inputs[1], TestInput::From(_)));
        let TestInput::Inline(test) = &inputs[2] else {
            panic!("{:?}", inputs[2]);
        };
        let core = (**test)
            .clone()
            .into_core(|_| Err("no cassette".into()))
            .unwrap();
        assert_eq!(
            core.world.actions["a.b"][0].answer,
            ActionOutcome::Result(Value::Null)
        );
        assert!(
            serde_json::from_value::<Test>(json!({ "name": "t", "x": 1 })).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn a_test_goes_to_the_domain_and_back() {
        let test: Test = serde_json::from_value(json!({
            "name": "approves",
            "input": { "repo": "acme/api" },
            "start": "2026-01-01T00:00:00Z",
            "seed": "07".repeat(32),
            "world": { "actions": { "a.b": [
                { "args": { "n": 1 }, "returns": 1, "times": 2 },
                { "error": { "message": "down", "kind": "unavailable" } }
            ] } },
            "signals": [{ "name": "approval", "payload": true, "when": "2h" }],
            "expect": { "status": "completed", "calls": [{ "action": "a.b", "times": 0 }], "order": "listed" }
        }))
        .unwrap();
        let core = test
            .clone()
            .into_core(|_| Err("no cassette".into()))
            .unwrap();
        assert_eq!(core.seed, Some(Seed::from_bytes([7; 32])));
        assert_eq!(
            core.signals[0].when,
            rewind_core::SignalWhen::After(Duration::from_secs(7_200))
        );
        let back = Test::from_core(&core, |_| Value::Null);
        assert_eq!(back.world, test.world.clone().normalized());
        assert_eq!(back.expect, test.expect);
        assert_eq!(back.seed, test.seed);
        assert!(
            Test {
                seed: Some("ABC".into()),
                ..test
            }
            .into_core(|_| Err("no cassette".into()))
            .is_err()
        );
    }

    impl World {
        /// The same world, each action's rules in an array.
        fn normalized(self) -> Self {
            Self {
                actions: self
                    .actions
                    .into_iter()
                    .map(|(action, rules)| (action, ActionRules::Many(rules.into_vec())))
                    .collect(),
                ..self
            }
        }
    }
}
