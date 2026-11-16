//! Mock mode (`archi/05-testing-and-observability.md`, "Testing and mocks").
//!
//! Mock mode is not a separate system: it is the same engine wired with in-memory adapters, a
//! virtual clock, and a dispatcher that answers from mocks. A call is answered by the first
//! source that has an answer:
//!
//! 1. the **rules** a test writes per tool ([`RulesDispatcher`]), matched by args;
//! 2. **mock handlers** ([`Mocks`]): any [`ToolDispatcher`], for the tools it handles. The
//!    TypeScript mock handlers run in a separate isolate, behind that port (`rewind-runtime`);
//! 3. a **cassette** recorded from a real run ([`CassetteDispatcher`]): `sequential` hands the
//!    recorded calls out in order, `by_args` by tool and canonical args;
//! 4. **static responses** per tool, as `rewind.tools.json` holds them;
//! 5. otherwise the call fails with "no recorded response for `tool({…})`", and is listed in
//!    the dispatcher's [`UnansweredCalls`]: a test fails on it.
//!
//! Running a test with these sources is [`crate::testing`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use rewind_core::{
    Cassette, JsonValue, Matching, Payload, RecordedCall, RunId, Seq, ToolFailure, ToolFailureKind,
    ToolOutcome, ToolRule,
};

use crate::adapters::{StaticDispatcher, StaticResponsesError};
use crate::ports::{Credentials, Dispatched, ToolCall, ToolDispatcher};

/// Characters of the args shown when no source answers a call.
const ARGS_CHARS: usize = 80;

/// Answers calls from the rules of a test: for each tool, the first rule whose args the call
/// contains and that has answered fewer than its `times` calls.
#[derive(Debug)]
pub struct RulesDispatcher {
    rules: BTreeMap<String, Vec<ToolRule>>,
    state: Mutex<RulesState>,
}

#[derive(Debug, Default)]
struct RulesState {
    /// Calls answered by each rule, by tool and position.
    used: BTreeMap<(String, usize), u32>,
    /// A call answered once gets the same answer again (an attempt after a lost claim).
    answered: BTreeMap<(RunId, Seq), ToolOutcome>,
}

impl RulesDispatcher {
    pub fn new(rules: BTreeMap<String, Vec<ToolRule>>) -> Self {
        Self {
            rules,
            state: Mutex::new(RulesState::default()),
        }
    }

    /// The answer of the first rule that takes the call, if one does.
    pub fn answer(&self, call: &ToolCall) -> Option<ToolOutcome> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (call.run.id, call.seq);
        if let Some(outcome) = state.answered.get(&key) {
            return Some(outcome.clone());
        }
        let rules = self.rules.get(&call.tool.name)?;
        for (position, rule) in rules.iter().enumerate() {
            if !rule.matches(&call.args) {
                continue;
            }
            let used = state
                .used
                .entry((call.tool.name.clone(), position))
                .or_default();
            if rule.times.is_some_and(|times| *used >= times) {
                continue;
            }
            *used += 1;
            state.answered.insert(key, rule.answer.clone());
            return Some(rule.answer.clone());
        }
        None
    }
}

/// The calls no source answered, shared between a [`MockDispatcher`] and whoever reads them
/// once the run is over: each once, in the order they came, as `tool({…})`.
#[derive(Debug, Clone, Default)]
pub struct UnansweredCalls(Arc<Mutex<Vec<UnansweredCall>>>);

/// A call no source answered: the run and `seq` it was, and how it reads.
type UnansweredCall = ((RunId, Seq), String);

impl UnansweredCalls {
    fn push(&self, call: &ToolCall) {
        let mut calls = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (call.run.id, call.seq);
        if !calls.iter().any(|(seen, _)| *seen == key) {
            calls.push((key, describe(call)));
        }
    }

    /// Each call no source answered, as `tool({…})`.
    pub fn list(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, call)| call.clone())
            .collect()
    }
}

/// `github.close_issue({"number":4})`, the args cut to [`ARGS_CHARS`].
fn describe(call: &ToolCall) -> String {
    format!(
        "{}({})",
        call.tool.name,
        Payload::Value(call.args.clone()).preview(ARGS_CHARS)
    )
}

/// Answers calls from a cassette recorded from a real run.
#[derive(Debug)]
pub struct CassetteDispatcher {
    cassette: Cassette,
    state: Mutex<CassetteState>,
}

#[derive(Debug, Default)]
struct CassetteState {
    /// Recorded calls already handed out.
    used: BTreeSet<usize>,
    /// `sequential`: the next recorded call.
    next: usize,
    /// A call answered once gets the same answer again (an attempt after a lost claim).
    answered: BTreeMap<(RunId, Seq), usize>,
}

impl CassetteDispatcher {
    pub fn new(cassette: Cassette) -> Self {
        Self {
            cassette,
            state: Mutex::new(CassetteState::default()),
        }
    }

    pub fn cassette(&self) -> &Cassette {
        &self.cassette
    }

    /// The recorded outcome for this call, if the cassette has one: under `sequential`, the
    /// next recorded call if it is to the same tool with the same args; under `by_args`, the
    /// first unused recorded call to the same tool with the same canonical args.
    pub fn answer(&self, call: &ToolCall) -> Option<ToolOutcome> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (call.run.id, call.seq);
        if let Some(position) = state.answered.get(&key) {
            return Some(self.cassette.calls[*position].outcome.clone());
        }
        let same = |recorded: &RecordedCall| {
            recorded.tool == call.tool.name
                && Payload::Value(recorded.args.clone()).same_as(&Payload::Value(call.args.clone()))
        };
        let position = match self.cassette.matching {
            Matching::Sequential => {
                let position = state.next;
                let recorded = self.cassette.calls.get(position)?;
                if !same(recorded) {
                    return None;
                }
                state.next += 1;
                position
            }
            Matching::ByArgs => {
                self.cassette
                    .calls
                    .iter()
                    .enumerate()
                    .find(|(position, recorded)| !state.used.contains(position) && same(recorded))?
                    .0
            }
        };
        state.used.insert(position);
        state.answered.insert(key, position);
        Some(self.cassette.calls[position].outcome.clone())
    }
}

impl ToolDispatcher for CassetteDispatcher {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        match self.answer(&call) {
            Some(outcome) => dispatched(outcome),
            None => Dispatched::Failed(no_response(&call)),
        }
    }
}

fn dispatched(outcome: ToolOutcome) -> Dispatched {
    match outcome {
        ToolOutcome::Result(result) => Dispatched::Result(result),
        ToolOutcome::Failure(failure) => Dispatched::Failed(failure),
    }
}

fn no_response(call: &ToolCall) -> ToolFailure {
    ToolFailure {
        kind: ToolFailureKind::Error,
        message: format!("no recorded response for `{}`", describe(call)),
        data: None,
    }
}

/// Mock handlers: any dispatcher, for the tools it handles. The TypeScript mock handlers of a
/// test (`rewind-runtime`, in a separate isolate) are one.
#[derive(Debug)]
pub struct Mocks<M> {
    pub handlers: M,
    /// The tools the handlers answer; the others go to the next source.
    pub tools: BTreeSet<String>,
}

/// The dispatcher of mock mode: rules, then mock handlers, then a cassette, then static
/// responses.
#[derive(Debug)]
pub struct MockDispatcher<M> {
    rules: Option<RulesDispatcher>,
    mocks: Option<Mocks<M>>,
    cassette: Option<CassetteDispatcher>,
    statics: StaticDispatcher,
    static_tools: BTreeSet<String>,
    unanswered: UnansweredCalls,
}

/// No mock handlers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMocks;

impl ToolDispatcher for NoMocks {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        Dispatched::Failed(no_response(&call))
    }
}

impl MockDispatcher<NoMocks> {
    /// A dispatcher that answers nothing until sources are added.
    pub fn new() -> Self {
        Self::without_mocks()
    }
}

impl Default for MockDispatcher<NoMocks> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> MockDispatcher<M> {
    /// A dispatcher whose mock handlers would be of type `M`, without any yet.
    pub fn without_mocks() -> Self {
        Self {
            rules: None,
            mocks: None,
            cassette: None,
            statics: StaticDispatcher::new(),
            static_tools: BTreeSet::new(),
            unanswered: UnansweredCalls::default(),
        }
    }

    pub fn with_rules(mut self, rules: BTreeMap<String, Vec<ToolRule>>) -> Self {
        if !rules.is_empty() {
            self.rules = Some(RulesDispatcher::new(rules));
        }
        self
    }

    pub fn with_cassette(mut self, cassette: Cassette) -> Self {
        self.cassette = Some(CassetteDispatcher::new(cassette));
        self
    }

    pub fn with_mocks<N>(self, mocks: Mocks<N>) -> MockDispatcher<N> {
        MockDispatcher {
            rules: self.rules,
            mocks: Some(mocks),
            cassette: self.cassette,
            statics: self.statics,
            static_tools: self.static_tools,
            unanswered: self.unanswered,
        }
    }

    /// Static responses in the `rewind.tools.json` format: a tool maps to its result, or to
    /// `{"$error": "message"}`.
    pub fn with_static_responses(
        mut self,
        responses: &JsonValue,
    ) -> Result<Self, StaticResponsesError> {
        self.statics = StaticDispatcher::from_json(responses)?;
        self.static_tools = responses
            .as_object()
            .map(|tools| tools.keys().cloned().collect())
            .unwrap_or_default();
        Ok(self)
    }

    /// The signals of the cassette, if any.
    pub fn recorded_signals(&self) -> &[rewind_core::RecordedSignal] {
        self.cassette
            .as_ref()
            .map_or(&[], |cassette| cassette.cassette().signals.as_slice())
    }

    /// The calls no source answers, filled as the run goes.
    pub fn unanswered(&self) -> UnansweredCalls {
        self.unanswered.clone()
    }
}

impl<M: ToolDispatcher> ToolDispatcher for MockDispatcher<M> {
    async fn dispatch(&self, call: ToolCall, credentials: Option<Credentials>) -> Dispatched {
        if let Some(outcome) = self.rules.as_ref().and_then(|rules| rules.answer(&call)) {
            return dispatched(outcome);
        }
        if let Some(mocks) = &self.mocks
            && mocks.tools.contains(&call.tool.name)
        {
            return mocks.handlers.dispatch(call, credentials).await;
        }
        if let Some(outcome) = self
            .cassette
            .as_ref()
            .and_then(|cassette| cassette.answer(&call))
        {
            return dispatched(outcome);
        }
        if self.static_tools.contains(&call.tool.name) {
            return self.statics.dispatch(call, credentials).await;
        }
        // A signal to a mocked run has nowhere to go: it is accepted (`archi/03-execution.md`,
        // "Workflows across runs").
        if matches!(
            rewind_core::WorkflowOp::of_tool(&call.tool.name),
            Some((_, rewind_core::WorkflowOp::Signal))
        ) {
            return Dispatched::Result(serde_json::json!({}));
        }
        self.unanswered.push(&call);
        Dispatched::Failed(no_response(&call))
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{RecordedSignal, Tool, ToolRoute};
    use serde_json::json;

    use super::*;
    use crate::ports::{RunRef, TaskToken, TenantId};

    fn call(seq: u32, tool: &str, args: JsonValue) -> ToolCall {
        ToolCall {
            run: RunRef::new(TenantId::local(), RunId::from_u128(1)),
            seq: Seq(seq),
            tool: Tool::new(tool, ToolRoute::Host { group: None }, json!({})),
            server: None,
            args,
            idempotency_key: format!("r/{seq}"),
            attempt: 1,
            schema_change_approved: false,
            token: TaskToken("t".into()),
            trace_context: None,
        }
    }

    fn cassette(matching: Matching) -> Cassette {
        Cassette {
            calls: vec![
                RecordedCall {
                    tool: "a".into(),
                    args: json!({ "n": 1 }),
                    outcome: ToolOutcome::Result(json!("first")),
                },
                RecordedCall {
                    tool: "a".into(),
                    args: json!({ "n": 2 }),
                    outcome: ToolOutcome::Result(json!("second")),
                },
            ],
            signals: vec![RecordedSignal {
                name: "go".into(),
                payload: json!(true),
                elapsed_ms: 10,
            }],
            matching,
        }
    }

    #[test]
    fn sequential_cassettes_answer_in_order() {
        let cassette = CassetteDispatcher::new(cassette(Matching::Sequential));
        // Out of order: the next recorded call is { n: 1 }.
        assert_eq!(cassette.answer(&call(1, "a", json!({ "n": 2 }))), None);
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        // The same call again gets the same answer.
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        assert_eq!(
            cassette.answer(&call(2, "a", json!({ "n": 2 }))),
            Some(ToolOutcome::Result(json!("second")))
        );
        assert_eq!(cassette.answer(&call(3, "a", json!({ "n": 1 }))), None);
    }

    #[test]
    fn by_args_cassettes_answer_in_any_order() {
        let cassette = CassetteDispatcher::new(cassette(Matching::ByArgs));
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 2 }))),
            Some(ToolOutcome::Result(json!("second")))
        );
        assert_eq!(
            cassette.answer(&call(2, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        // Each recorded call is handed out once.
        assert_eq!(cassette.answer(&call(3, "a", json!({ "n": 1 }))), None);
        assert_eq!(cassette.answer(&call(4, "b", json!({ "n": 1 }))), None);
    }

    #[tokio::test]
    async fn sources_are_tried_in_order() {
        let mocks = Mocks {
            handlers: StaticDispatcher::from_json(&json!({ "b": "from the mocks" })).unwrap(),
            tools: BTreeSet::from(["b".to_owned()]),
        };
        let dispatcher = MockDispatcher::new()
            .with_cassette(cassette(Matching::ByArgs))
            .with_static_responses(
                &json!({ "a": "static", "b": "static", "c": { "$error": "nope" } }),
            )
            .unwrap()
            .with_mocks(mocks);
        let answer = |call| dispatcher.dispatch(call, None);
        assert_eq!(
            answer(call(1, "a", json!({ "n": 1 }))).await,
            Dispatched::Result(json!("first"))
        );
        // Not in the cassette: the static response.
        assert_eq!(
            answer(call(2, "a", json!({ "n": 9 }))).await,
            Dispatched::Result(json!("static"))
        );
        // The mocks come before the static responses.
        assert_eq!(
            answer(call(3, "b", json!({}))).await,
            Dispatched::Result(json!("from the mocks"))
        );
        assert!(matches!(
            answer(call(4, "c", json!({}))).await,
            Dispatched::Failed(ToolFailure { message, .. }) if message == "nope"
        ));
        match answer(call(5, "d", json!({ "x": 1 }))).await {
            Dispatched::Failed(failure) => {
                assert_eq!(failure.message, "no recorded response for `d({\"x\":1})`");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(dispatcher.recorded_signals().len(), 1);
        assert_eq!(dispatcher.unanswered().list(), vec!["d({\"x\":1})"]);
    }

    #[tokio::test]
    async fn rules_come_first_and_answer_their_times() {
        let rules = BTreeMap::from([(
            "a".to_owned(),
            vec![
                ToolRule {
                    args: Some(json!({ "n": 1 })),
                    answer: ToolOutcome::Result(json!("rule, once")),
                    times: Some(1),
                },
                ToolRule::always(ToolOutcome::Result(json!("rule, always"))),
            ],
        )]);
        let dispatcher = MockDispatcher::new()
            .with_cassette(cassette(Matching::ByArgs))
            .with_rules(rules);
        let answer = |call| dispatcher.dispatch(call, None);
        // The cassette has { n: 1 } too: the rule wins.
        assert_eq!(
            answer(call(1, "a", json!({ "n": 1, "m": 2 }))).await,
            Dispatched::Result(json!("rule, once"))
        );
        // The same call again (a lost claim) gets the same answer.
        assert_eq!(
            answer(call(1, "a", json!({ "n": 1, "m": 2 }))).await,
            Dispatched::Result(json!("rule, once"))
        );
        // The first rule has answered its one call.
        assert_eq!(
            answer(call(2, "a", json!({ "n": 1 }))).await,
            Dispatched::Result(json!("rule, always"))
        );
        assert!(dispatcher.unanswered().list().is_empty());
        // A signal to a mocked run is accepted, and not listed as unanswered.
        assert_eq!(
            answer(call(3, "workflows.triage.signal", json!({}))).await,
            Dispatched::Result(json!({}))
        );
        assert!(dispatcher.unanswered().list().is_empty());
    }
}
