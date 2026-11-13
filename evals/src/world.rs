//! The world of an episode: the real engine with the V8 sandbox and the MCP dispatcher, the
//! fake MCP servers it calls, and the outside world reacting in virtual time.
//!
//! The engine is wired in process, as `rewind-e2e` wires it (`archi/08-decisions.md`, "Taken
//! while building the evals"): in-memory storage, `McpDispatcher` for MCP tools, the type
//! checker `rewind-check`, codegen `rewind-codegen`, and one virtual clock shared with the fake
//! servers. Time only moves when the world is driven: a run's `wait`, a server's webhook, a
//! signal sent by the world after a delay, a timer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_check::TsgoChecker;
use rewind_core::{
    Builtin, HistoryEntry, McpServer, McpTransport, RunId, RunOutcome, RunState, RunStatus, Script,
    Timestamp, Tool, ToolRoute, Toolset,
};
use rewind_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, RoutingDispatcher, SeededEntropy,
    Unrouted,
};
use rewind_engine::ports::{
    Clock, JournalStore, RunFilter, RunRef, TenantId, ToolDeclarations, VirtualTime, WebhookToken,
};
use rewind_engine::{Adapters, DriveOptions, Engine, EngineConfig, EngineError, StartRun};
use rewind_mcp::{McpConfig, McpDispatcher, McpEgress};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::Value;

use crate::clock::{DAY, SharedClock, T0};
use crate::fake::{self, Running, Service};

/// The engine of an episode.
pub type EvalEngine = Engine<
    MemoryJournal,
    MemoryBlobs,
    V8Executor,
    RoutingDispatcher<McpDispatcher, Unrouted, Unrouted>,
    SharedClock,
    SeededEntropy,
    MemorySecrets,
    MemoryWebhooks,
>;

/// The base of the URLs `webhook(name)` returns.
pub const PUBLIC_URL: &str = "https://rewind.example";

/// How far the world goes on after the conversation, at most: the runs left waiting for
/// timers end within it.
pub const SETTLE_HORIZON: u64 = 60 * DAY;

/// Jumps of virtual time in one drive, at most: a guard against scripts that never stop.
const MAX_STEPS: usize = 20_000;

/// The world answers a signal wait: when a run waits for `signal`, the world sends `payload`
/// `after` that much virtual time, once per run.
#[derive(Debug, Clone, PartialEq)]
pub struct Reaction {
    pub signal: &'static str,
    pub payload: Value,
    pub after: Duration,
}

/// What an episode's world is made of.
#[derive(Debug, Clone, Default)]
pub struct WorldSpec {
    /// Fake servers by name ([`fake::SERVERS`]).
    pub servers: Vec<&'static str>,
    /// The toolset has the built-in `webhook`.
    pub webhook: bool,
    pub reactions: Vec<Reaction>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("unknown fake server {0}")]
    UnknownServer(String),
    #[error("cannot serve a fake server: {0}")]
    Serve(#[from] std::io::Error),
    #[error("cannot list the tools of {server}: {message}")]
    Discovery { server: String, message: String },
    #[error("invalid toolset: {0}")]
    Toolset(String),
    #[error("the sandbox cannot start: {0}")]
    Sandbox(String),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("an activation of run {run} failed: {error}")]
    Activation { run: RunId, error: EngineError },
}

/// How a drive ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drove {
    /// The run the drive waited for has ended.
    Ended,
    /// The time limit was reached.
    TimeUp,
    /// Nothing can happen without the agent.
    Idle,
    /// The drive gave up: the runs kept going without reaching any of the above.
    Interrupted,
}

#[derive(Debug, Clone)]
struct Scheduled {
    run: RunRef,
    signal: String,
    payload: Value,
    at: Timestamp,
}

/// A signal or webhook the world delivered, for the episode's record.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Delivered {
    pub run: String,
    pub signal: String,
    pub at: String,
    /// `reaction` or `webhook`.
    pub source: &'static str,
}

#[derive(Debug, Default)]
struct Reactions {
    scheduled: Vec<Scheduled>,
    fired: BTreeSet<(RunId, String)>,
    delivered: Vec<Delivered>,
}

/// The world of an episode.
pub struct World {
    pub engine: EvalEngine,
    pub clock: SharedClock,
    pub servers: Vec<Running>,
    /// The pinned toolset the agent's scripts import from `rewind:tools`.
    pub toolset: Toolset,
    /// What codegen generated for it.
    pub declarations: rewind_codegen::Declarations,
    pub checker: Arc<TsgoChecker>,
    /// Set on every run of the agent.
    pub labels: BTreeMap<String, String>,
    pub options: DriveOptions,
    mcp: McpDispatcher,
    reactions_spec: Vec<Reaction>,
    reactions: Mutex<Reactions>,
}

impl std::fmt::Debug for World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("servers", &self.servers)
            .field("toolset", &self.toolset.digest())
            .finish_non_exhaustive()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.mcp.close();
        for server in &self.servers {
            server.stop();
        }
    }
}

impl World {
    /// Serves the fake servers, pins their tools as `POST /v1/toolsets` does (discovery through
    /// `McpDispatcher`), generates the declarations, and wires the engine at [`T0`].
    pub async fn new(
        spec: &WorldSpec,
        checker: Arc<TsgoChecker>,
        labels: BTreeMap<String, String>,
    ) -> Result<Self, WorldError> {
        let clock = SharedClock::new(T0);
        let mut servers = Vec::new();
        for name in &spec.servers {
            let service: Arc<dyn Service> =
                fake::by_name(name).ok_or_else(|| WorldError::UnknownServer((*name).to_owned()))?;
            servers.push(fake::serve(service, clock.clone()).await?);
        }

        // The fake servers listen on 127.0.0.1, as a developer's would for the embedded server.
        let mcp = McpDispatcher::new(McpConfig {
            egress: McpEgress::open(),
            ..McpConfig::default()
        });
        let tenant = TenantId::local();
        let mut pinned_servers = Vec::new();
        let mut tools: Vec<Tool> = Vec::new();
        for running in &servers {
            let server = McpServer::new(
                running.service.name(),
                McpTransport::Http {
                    url: running.url.clone(),
                },
            );
            let listed = mcp
                .list_tools(&tenant, &server, None)
                .await
                .map_err(|err| WorldError::Discovery {
                    server: server.name.clone(),
                    message: err.to_string(),
                })?;
            tools.extend(listed);
            pinned_servers.push(server);
        }
        if spec.webhook {
            tools.push(Tool::new(
                "webhook",
                ToolRoute::Builtin(Builtin::Webhook),
                rewind_runtime::builtins::webhook_input_schema(),
            ));
        }
        let toolset = Toolset::new(pinned_servers, tools)
            .map_err(|err| WorldError::Toolset(err.to_string()))?;
        let declarations = rewind_codegen::generate(&toolset, &BTreeMap::new());

        let executor = V8Executor::new(SandboxLimits::default())
            .map_err(|err| WorldError::Sandbox(err.to_string()))?;
        let config = EngineConfig {
            worker: "evals".into(),
            public_url: Some(PUBLIC_URL.into()),
            ..EngineConfig::default()
        };
        let engine = Engine::new(
            Adapters {
                journal: MemoryJournal::new(),
                blobs: MemoryBlobs::new(),
                executor,
                dispatcher: RoutingDispatcher::new(mcp.clone(), Unrouted, Unrouted),
                clock: clock.clone(),
                entropy: SeededEntropy::new(1),
                secrets: MemorySecrets::new(),
                webhooks: MemoryWebhooks::new(),
            },
            config,
        );
        Ok(Self {
            engine,
            clock,
            servers,
            toolset,
            declarations,
            checker,
            labels,
            options: DriveOptions {
                max_passes: 20_000,
                ..DriveOptions::default()
            },
            mcp,
            reactions_spec: spec.reactions.clone(),
            reactions: Mutex::new(Reactions::default()),
        })
    }

    /// The declarations stored with the toolset, as the engine and the checker take them.
    pub fn tool_declarations(&self) -> ToolDeclarations {
        ToolDeclarations {
            typescript: self.declarations.typescript.clone(),
            prompt: self.declarations.prompt.clone(),
        }
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// A run of the tenant `local`.
    pub fn run_ref(&self, id: RunId) -> RunRef {
        RunRef::new(TenantId::local(), id)
    }

    /// Starts a run of the agent: with its labels, live, in virtual time. The caller has
    /// type-checked the script.
    pub fn start_request(&self, script: Script, input: Value) -> StartRun {
        let mut request = StartRun::new(script, self.toolset.clone(), input);
        request.declarations = self.tool_declarations();
        request.metadata.labels = self.labels.clone();
        request
    }

    /// Every run of the world, oldest first.
    pub async fn runs(&self) -> Result<Vec<RunRef>, WorldError> {
        let records = self
            .engine
            .list_runs(&RunFilter::new(TenantId::local()))
            .await?;
        Ok(records.into_iter().map(|record| record.run).collect())
    }

    /// The calls a fake server received.
    pub fn calls(&self, server: &str) -> Vec<fake::CallRecord> {
        self.servers
            .iter()
            .find(|running| running.service.name() == server)
            .map(Running::calls)
            .unwrap_or_default()
    }

    /// The signals the world sent, in order.
    pub fn delivered(&self) -> Vec<Delivered> {
        fake::lock(&self.reactions).delivered.clone()
    }

    /// Drives the world in virtual time: the engine runs whatever is ready, the world reacts
    /// (webhooks of the servers, signals answering waits), and time jumps to the next thing
    /// that happens, until `run` has ended, until `until`, or until nothing can happen without
    /// the agent.
    pub async fn drive(
        &self,
        run: Option<&RunRef>,
        until: Option<Timestamp>,
    ) -> Result<Drove, WorldError> {
        self.drive_until(run, until, true).await
    }

    /// [`drive`](Self::drive); when nothing can happen any more before `until`, the clock
    /// moves to `until` only if `elapse` (a wait elapses; a world left alone does not).
    async fn drive_until(
        &self,
        run: Option<&RunRef>,
        until: Option<Timestamp>,
        elapse: bool,
    ) -> Result<Drove, WorldError> {
        for _ in 0..MAX_STEPS {
            let report = self.engine.run_until_idle(&self.options).await?;
            if let Some((run, error)) = report.failures.into_iter().next() {
                return Err(WorldError::Activation { run: run.id, error });
            }
            if report.interrupted {
                return Ok(Drove::Interrupted);
            }
            if let Some(run) = run
                && self.is_terminal(run).await?
            {
                return Ok(Drove::Ended);
            }
            if self.react().await? {
                continue;
            }
            let now = self.now();
            match (self.next_event().await?, until) {
                (Some(next), Some(until)) if next > until => {
                    self.clock.advance_to(until);
                    return Ok(Drove::TimeUp);
                }
                (Some(next), _) if next > now => self.clock.advance_to(next),
                (Some(_), _) => return Ok(Drove::Interrupted),
                (None, Some(until)) => {
                    if !elapse {
                        return Ok(Drove::Idle);
                    }
                    self.clock.advance_to(until);
                    return Ok(Drove::TimeUp);
                }
                (None, None) => return Ok(Drove::Idle),
            }
        }
        Ok(Drove::Interrupted)
    }

    /// After the conversation: the world goes on, up to [`SETTLE_HORIZON`] of virtual time, so
    /// that a run sleeping for days does what it was written to do.
    pub async fn settle(&self) -> Result<Drove, WorldError> {
        let horizon = Timestamp(self.now().0 + SETTLE_HORIZON);
        self.drive_until(None, Some(horizon), false).await
    }

    async fn is_terminal(&self, run: &RunRef) -> Result<bool, WorldError> {
        Ok(self
            .engine
            .get_run(run)
            .await?
            .is_some_and(|record| record.status.is_terminal()))
    }

    /// The next time something happens by itself: a timer, a retried tool call (or a run's
    /// deadline before them), a webhook of a server, a signal the world will send.
    async fn next_event(&self) -> Result<Option<Timestamp>, WorldError> {
        let journal = self.engine.journal();
        let timer = journal.next_timer().await.map_err(EngineError::from)?;
        let retry = self.engine.next_tool_wake_up().await?;
        let mut next = timer.into_iter().chain(retry).min();
        if let Some(engine_next) = next {
            let deadline = journal.next_deadline().await.map_err(EngineError::from)?;
            next = Some(deadline.map_or(engine_next, |deadline| deadline.min(engine_next)));
        }
        let scheduled = fake::lock(&self.reactions)
            .scheduled
            .iter()
            .map(|scheduled| scheduled.at)
            .min();
        let webhooks = self
            .servers
            .iter()
            .filter_map(|server| server.service.outbox().and_then(|outbox| outbox.next()))
            .min();
        Ok(next.into_iter().chain(scheduled).chain(webhooks).min())
    }

    /// Delivers what is due now: the servers' webhooks, and the signals answering waits.
    /// Returns whether anything was delivered.
    async fn react(&self) -> Result<bool, WorldError> {
        let now = self.now();
        let mut delivered = false;

        for server in &self.servers {
            let Some(outbox) = server.service.outbox() else {
                continue;
            };
            for outbound in outbox.take_due(now) {
                // What the server's POST to `/hooks/{token}` does on the embedded server.
                let token = outbound
                    .url
                    .strip_prefix(PUBLIC_URL)
                    .and_then(|path| path.strip_prefix("/hooks/"));
                let Some(token) = token else {
                    continue;
                };
                let target = self
                    .engine
                    .journal()
                    .resolve_webhook_token(&WebhookToken(token.to_owned()))
                    .await
                    .map_err(EngineError::from)?;
                if let Some(target) = target {
                    self.engine
                        .signal(
                            &target.run,
                            target.name.clone(),
                            outbound.payload,
                            Some(outbound.id),
                        )
                        .await?;
                    self.record(&target.run, &target.name, now, "webhook");
                    delivered = true;
                }
            }
        }

        if !self.reactions_spec.is_empty() {
            for run in self.runs().await? {
                let state = self.engine.run_state(&run).await?;
                if state.status().is_terminal() {
                    continue;
                }
                for awaited in state.awaited_signals() {
                    let Some(reaction) = self
                        .reactions_spec
                        .iter()
                        .find(|reaction| reaction.signal == awaited.name)
                    else {
                        continue;
                    };
                    let mut reactions = fake::lock(&self.reactions);
                    if reactions.fired.insert((run.id, awaited.name.clone())) {
                        let after = u64::try_from(reaction.after.as_millis()).unwrap_or(u64::MAX);
                        reactions.scheduled.push(Scheduled {
                            run: run.clone(),
                            signal: awaited.name.clone(),
                            payload: reaction.payload.clone(),
                            at: Timestamp(now.0.saturating_add(after)),
                        });
                    }
                }
            }
        }

        let due: Vec<Scheduled> = {
            let mut reactions = fake::lock(&self.reactions);
            let (due, later) = reactions
                .scheduled
                .drain(..)
                .partition(|scheduled| scheduled.at <= now);
            reactions.scheduled = later;
            due
        };
        for scheduled in due {
            self.engine
                .signal(
                    &scheduled.run,
                    scheduled.signal.clone(),
                    scheduled.payload,
                    None,
                )
                .await?;
            self.record(&scheduled.run, &scheduled.signal, now, "reaction");
            delivered = true;
        }
        Ok(delivered)
    }

    fn record(&self, run: &RunRef, signal: &str, at: Timestamp, source: &'static str) {
        fake::lock(&self.reactions).delivered.push(Delivered {
            run: run.id.to_string(),
            signal: signal.to_owned(),
            at: crate::clock::rfc3339(at),
            source,
        });
    }

    /// Every run as a checker sees it.
    pub async fn run_reports(&self) -> Result<Vec<RunReport>, WorldError> {
        let mut reports = Vec::new();
        for run in self.runs().await? {
            let record = self.engine.get_run(&run).await?;
            let history = self.engine.export_history(&run).await?;
            let state = RunState::from_history(&history).ok();
            let (output, error) = match state.as_ref().and_then(RunState::outcome) {
                Some(RunOutcome::Completed { output }) => (output.value().cloned(), None),
                Some(RunOutcome::Failed { error }) => {
                    (None, Some(format!("{}: {}", error.name, error.message)))
                }
                Some(RunOutcome::Aborted { reason }) => (None, Some(reason.to_string())),
                None => (None, None),
            };
            reports.push(RunReport {
                id: run.id,
                status: record.as_ref().map_or(RunStatus::Pending, |r| r.status),
                labels: record.map(|r| r.metadata.labels).unwrap_or_default(),
                output,
                error,
                history,
            });
        }
        Ok(reports)
    }
}

/// A run at the end of an episode.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub id: RunId,
    pub status: RunStatus,
    pub labels: BTreeMap<String, String>,
    /// With `completed`.
    pub output: Option<Value>,
    /// With `failed`, `cancelled` or `aborted`: `Name: message`.
    pub error: Option<String>,
    /// Every payload loaded.
    pub history: Vec<HistoryEntry>,
}
