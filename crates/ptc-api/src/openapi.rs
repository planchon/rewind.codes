//! The OpenAPI 3.1 document of the API, generated from the DTOs with `utoipa`.
//!
//! It is committed at `api/openapi.json`; a test fails when the committed file is stale, and
//! `PTC_OPENAPI_WRITE=1 cargo test -p ptc-api --test openapi` writes it again. The TypeScript
//! SDK and the dashboard generate their types from that file (`api/README.md`).
//!
//! Each operation below is a declaration: its function has no body, and serves only to carry
//! the operation's description to `utoipa`. The server implements the operations; its routes
//! are the paths of [`crate::routes`].

use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::response::{ResponseBuilder, ResponsesBuilder};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{
    ContentBuilder, OpenApi as Document, PathItem, PathsBuilder, Ref, RefOr, Required,
};
use utoipa::{Modify, OpenApi};

use crate::events::StreamEvent;
use crate::problem::{self, Problem};
use crate::webhooks::{
    WEBHOOK_ID, WEBHOOK_SIGNATURE, WEBHOOK_TIMESTAMP, WebhookEvent, WebhookEventType,
};

/// The API's document.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "ai-ptc API",
        description = "The HTTP API /v1 of ai-ptc: toolsets, scripts, runs, host-tool tasks, \
            webhooks, reading and administration. Every request carries an API key as a Bearer \
            token (`ptc_sk_…`), except over the embedded server's unix socket and to the inbound \
            webhook URL. Errors are `application/problem+json`, with a `detail` written for a \
            model. Fields are camelCase, enumerated values snake_case, times RFC 3339, and \
            durations a number and a unit (`\"30s\"`, `\"1h30m\"`) or a number of milliseconds."
    ),
    servers((url = "/", description = "The server that serves this document.")),
    paths(
        operations::create_toolset,
        operations::get_toolset_declarations,
        operations::search_tools,
        operations::set_toolset_credentials,
        operations::check_script,
        operations::test_script,
        operations::list_scripts,
        operations::get_script,
        operations::start_run,
        operations::list_runs,
        operations::get_run,
        operations::delete_run,
        operations::get_run_trace,
        operations::get_run_entry,
        operations::stream_run_events,
        operations::signal_run,
        operations::cancel_run,
        operations::fork_run,
        operations::export_run_journal,
        operations::release_run,
        operations::give_up_run,
        operations::set_run_credentials,
        operations::claim_tasks,
        operations::heartbeat_task,
        operations::defer_task,
        operations::complete_task,
        operations::fail_task,
        operations::approve_task,
        operations::reject_task,
        operations::post_hook,
        operations::create_subscription,
        operations::list_subscriptions,
        operations::delete_subscription,
        operations::get_tool_stats,
        operations::get_payload,
        operations::create_api_key,
        operations::revoke_api_key,
        operations::get_server,
        operations::stop_server,
    ),
    components(schemas(Problem, StreamEvent, WebhookEvent, WebhookEventType)),
    modifiers(&Conventions),
    security(("apiKey" = [])),
    tags(
        (name = "toolsets", description = "Register and pin toolsets; their declarations and credentials."),
        (name = "scripts", description = "Type-check scripts, and run them as scenarios in mock mode."),
        (name = "runs", description = "Start, follow, steer, fork and erase runs."),
        (name = "tasks", description = "Host-tool tasks: long-polled by the harness's SDK worker, approved or rejected by the harness."),
        (name = "webhooks", description = "Inbound webhooks that become signals, and subscriptions to signed lifecycle events."),
        (name = "reading", description = "The scripts executed on the platform, tool statistics and payloads, for the dashboard."),
        (name = "administration", description = "API keys, and the server itself."),
    )
)]
pub struct ApiDoc;

/// The document.
pub fn document() -> Document {
    ApiDoc::openapi()
}

/// The document as committed at `api/openapi.json`: pretty JSON, keys in a stable order, a
/// final newline.
pub fn to_json() -> String {
    let value = sorted(serde_json::to_value(document()).unwrap_or_default());
    let mut json = serde_json::to_string_pretty(&value).unwrap_or_default();
    json.push('\n');
    json
}

/// Object keys in sorted order, whatever the features of `serde_json` in the build: a
/// dependency that enables `preserve_order` must not change the committed file.
fn sorted(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let mut fields: Vec<_> = fields.into_iter().collect();
            fields.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|(key, field)| (key, sorted(field)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
}

/// What every operation shares, added once rather than on each: the security scheme, the
/// error responses every operation can answer, and the outbound webhooks.
struct Conventions;

/// Error responses every operation may answer, by status: those of a request that does not
/// even reach the operation's own logic.
const COMMON_ERRORS: [(&str, &str); 5] = [
    ("400", "The request is malformed (`invalid-request`)."),
    (
        "401",
        "No API key, or an unknown or revoked one (`unauthorized`).",
    ),
    (
        "403",
        "The key's role or allowed labels do not cover this request (`forbidden`).",
    ),
    ("500", "A fault of the platform (`internal`)."),
    (
        "503",
        "The server cannot serve the request now (`unavailable`).",
    ),
];

impl Modify for Conventions {
    fn modify(&self, openapi: &mut Document) {
        // utoipa fills the license from Cargo.toml, which has none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "apiKey",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("ptc_sk_…")
                    .description(Some(
                        "An API key of the tenant. The embedded server's unix socket takes none.",
                    ))
                    .build(),
            ),
        );

        for item in openapi.paths.paths.values_mut() {
            for operation in operations_mut(item) {
                let unauthenticated = operation
                    .security
                    .as_ref()
                    .is_some_and(|requirements| requirements.iter().all(is_empty_requirement));
                for (status, description) in COMMON_ERRORS {
                    if unauthenticated && matches!(status, "401" | "403") {
                        continue;
                    }
                    operation
                        .responses
                        .responses
                        .entry(status.to_owned())
                        .or_insert_with(|| problem_response(description));
                }
            }
        }

        openapi.webhooks = Some(webhooks_paths().into());
    }
}

fn is_empty_requirement(requirement: &utoipa::openapi::security::SecurityRequirement) -> bool {
    serde_json::to_value(requirement)
        .ok()
        .and_then(|value| value.as_object().map(serde_json::Map::is_empty))
        .unwrap_or(false)
}

fn operations_mut(
    item: &mut PathItem,
) -> impl Iterator<Item = &mut utoipa::openapi::path::Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.patch.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// A problem+json response.
pub(crate) fn problem_response(description: &str) -> RefOr<utoipa::openapi::response::Response> {
    ResponseBuilder::new()
        .description(description)
        .content(
            problem::CONTENT_TYPE,
            ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("Problem")))
                .build(),
        )
        .build()
        .into()
}

/// The outbound webhooks: one entry per event type, each a POST to the subscription's URL.
fn webhooks_paths() -> PathsBuilder {
    let mut paths = PathsBuilder::new();
    for kind in WebhookEventType::ALL {
        let header = |name: &str, description: &str| {
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Header)
                .required(Required::True)
                .description(Some(description))
                .schema(Some(
                    utoipa::openapi::ObjectBuilder::new()
                        .schema_type(utoipa::openapi::schema::Type::String),
                ))
                .build()
        };
        let operation = OperationBuilder::new()
            .operation_id(Some(format!("webhook_{}", kind.as_str().replace('.', "_"))))
            .summary(Some(format!("The lifecycle event `{kind}`")))
            .description(Some(format!(
                "Sent to every subscription to `{kind}` whose labels the run carries. Signed \
                 following Standard Webhooks: verify `{WEBHOOK_SIGNATURE}` with the \
                 subscription's secret before trusting the body, and deduplicate on \
                 `{WEBHOOK_ID}`. A delivery that does not get a 2xx answer is retried."
            )))
            .tag("webhooks")
            .parameter(header(
                WEBHOOK_ID,
                "The delivery's id; a retried delivery keeps it.",
            ))
            .parameter(header(
                WEBHOOK_TIMESTAMP,
                "When the delivery was signed, in seconds since the Unix epoch.",
            ))
            .parameter(header(
                WEBHOOK_SIGNATURE,
                "`v1,<base64 HMAC-SHA256 of {id}.{timestamp}.{body}>`, space-separated while a secret is rotated.",
            ))
            .request_body(Some(
                RequestBodyBuilder::new()
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .schema(Some(Ref::from_schema_name("WebhookEvent")))
                            .build(),
                    )
                    .required(Some(Required::True))
                    .build(),
            ))
            .responses(
                ResponsesBuilder::new()
                    .response("2XX", ResponseBuilder::new().description("Received."))
                    .build(),
            )
            .build();
        paths = paths.path(kind.as_str(), PathItem::new(HttpMethod::Post, operation));
    }
    paths
}

/// The operations, declared for `utoipa`. Their bodies are empty: the server implements them.
#[allow(dead_code)]
mod operations {
    use crate::admin::{ApiKey, CreateApiKey, ServerInfo};
    use crate::events::{Entry, EventsQuery, StreamEvent};
    use crate::problem::Problem;
    use crate::reading::{ToolStatsFilter, ToolStatsPage};
    use crate::runs::{
        CancelRun, ForkRun, GetRunQuery, JournalQuery, Run, RunFilter, RunPage, SignalAccepted,
        StartRun, Trace, TraceQuery,
    };
    use crate::scripts::{
        CheckRequest, CheckResult, Script, ScriptFilter, ScriptPage, TestRequest, TestResult,
    };
    use crate::tasks::{
        CompleteTask, DeferTask, FailTask, HeartbeatAck, RejectTask, TaskBatch, TaskQuery,
    };
    use crate::toolsets::{
        CreateToolset, Credentials, DeclarationsQuery, DeclarationsText, SearchQuery,
        ToolSearchResults, Toolset,
    };
    use crate::webhooks::{CreateSubscription, Subscription, SubscriptionList};

    // ---------------------------------------------------------------------------------------
    // Toolsets

    /// Register and pin a toolset
    ///
    /// Connects to the MCP servers, lists their tools, pins their schemas and policies, and
    /// returns the toolset with its declarations, for the model's prompt. Content-addressed:
    /// registering the same toolset again returns it (200).
    #[utoipa::path(
        post, path = "/v1/toolsets", tag = "toolsets", operation_id = "createToolset",
        request_body = CreateToolset,
        responses(
            (status = 201, description = "Registered.", body = Toolset),
            (status = 200, description = "Already registered.", body = Toolset),
            (status = 422, description = "Refused: an invalid schema, a duplicate name, a transport this server does not allow (`invalid-toolset`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "An MCP server cannot be reached (`toolset-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_toolset() {}

    /// The toolset's declarations
    #[utoipa::path(
        get, path = "/v1/toolsets/{id}/declarations", tag = "toolsets", operation_id = "getToolsetDeclarations",
        params(("id" = String, Path, description = "The toolset's id."), DeclarationsQuery),
        responses(
            (status = 200, description = "The declarations in the requested format.", body = DeclarationsText),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_toolset_declarations() {}

    /// Search the toolset's tools (`search_tools`)
    ///
    /// For toolsets too large for the prompt: the declarations of the tools matching a query.
    #[utoipa::path(
        get, path = "/v1/toolsets/{id}/search", tag = "toolsets", operation_id = "searchTools",
        params(("id" = String, Path, description = "The toolset's id."), SearchQuery),
        responses(
            (status = 200, description = "The matching tools, best first.", body = ToolSearchResults),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn search_tools() {}

    /// Set or renew the toolset's credentials
    #[utoipa::path(
        put, path = "/v1/toolsets/{id}/credentials", tag = "toolsets", operation_id = "setToolsetCredentials",
        params(("id" = String, Path, description = "The toolset's id.")),
        request_body = Credentials,
        responses(
            (status = 204, description = "Stored. Calls parked by `credentials.expired` are ready again."),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_toolset_credentials() {}

    // ---------------------------------------------------------------------------------------
    // Scripts

    /// Type-check a script (`check_script`)
    ///
    /// Against the sandbox lib and the toolset's declarations, in strict mode, with the checks
    /// at start and the checker's own rules. Nothing runs.
    #[utoipa::path(
        post, path = "/v1/check", tag = "scripts", operation_id = "checkScript",
        request_body = CheckRequest,
        responses(
            (status = 200, description = "The diagnostics; `ok` when none is an error.", body = CheckResult),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn check_script() {}

    /// Run a scenario in mock mode (`test_script`)
    ///
    /// In virtual time, against a cassette, mock handlers and static responses. No tool is
    /// really called.
    #[utoipa::path(
        post, path = "/v1/test", tag = "scripts", operation_id = "testScript",
        request_body = TestRequest,
        responses(
            (status = 200, description = "The outcome and the expectations that do not hold.", body = TestResult),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The script was refused at start (`invalid-script`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn test_script() {}

    /// List the distinct scripts executed
    #[utoipa::path(
        get, path = "/v1/scripts", tag = "reading", operation_id = "listScripts",
        params(ScriptFilter),
        responses((status = 200, description = "A page of scripts, most recently run first.", body = ScriptPage))
    )]
    pub(super) fn list_scripts() {}

    /// One script's source
    #[utoipa::path(
        get, path = "/v1/scripts/{digest}", tag = "reading", operation_id = "getScript",
        params(("digest" = String, Path, description = "SHA-256 of the source, lowercase hex.")),
        responses(
            (status = 200, description = "The script.", body = Script),
            (status = 404, description = "No such script.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_script() {}

    // ---------------------------------------------------------------------------------------
    // Runs

    /// Start a run (`run_script`)
    ///
    /// Compiles, checks and type-checks the script, then starts the run. With `wait`, answers
    /// when the run ends or `wait` passes, whichever comes first.
    #[utoipa::path(
        post, path = "/v1/runs", tag = "runs", operation_id = "startRun",
        params(("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key returns the run the first one started (200).")),
        request_body = StartRun,
        responses(
            (status = 201, description = "Started.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The input is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The script was refused before any run exists (`invalid-script`), with `diagnostics`.", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "The tenant already has its maximum of active runs (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn start_run() {}

    /// List runs (`list_runs`)
    #[utoipa::path(
        get, path = "/v1/runs", tag = "runs", operation_id = "listRuns",
        params(RunFilter),
        responses((status = 200, description = "A page of runs, most recently started first.", body = RunPage))
    )]
    pub(super) fn list_runs() {}

    /// A run's state (`get_run`)
    #[utoipa::path(
        get, path = "/v1/runs/{id}", tag = "runs", operation_id = "getRun",
        params(("id" = String, Path, description = "The run id."), GetRunQuery),
        responses(
            (status = 200, description = "The run.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run() {}

    /// Erase a run
    ///
    /// Its journal, its archive, its read model rows and its payload references. An active run
    /// is cancelled first.
    #[utoipa::path(
        delete, path = "/v1/runs/{id}", tag = "runs", operation_id = "deleteRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 204, description = "Erased."),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_run() {}

    /// The run's trace, for a model
    #[utoipa::path(
        get, path = "/v1/runs/{id}/trace", tag = "runs", operation_id = "getRunTrace",
        params(("id" = String, Path, description = "The run id."), TraceQuery),
        responses(
            (status = 200, description = "The trace.", body = Trace),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_trace() {}

    /// One journal entry in full
    ///
    /// Every payload of the entry has its value, whatever its size.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/entries/{idx}", tag = "runs", operation_id = "getRunEntry",
        params(("id" = String, Path, description = "The run id."), ("idx" = u64, Path, description = "The entry's index, from 0.")),
        responses(
            (status = 200, description = "The entry.", body = Entry),
            (status = 404, description = "No such run or entry.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_entry() {}

    /// Stream the run's journal (server-sent events)
    ///
    /// Each event's `data` is a `StreamEvent`: the committed entries from `from`, then, with
    /// `follow=1`, each new entry as it is committed, and `end` when the run has ended.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/events", tag = "runs", operation_id = "streamRunEvents",
        params(
            ("id" = String, Path, description = "The run id."),
            EventsQuery,
            ("Last-Event-ID" = Option<String>, Header, description = "Resume after this entry: the same as `from` set to it plus one."),
        ),
        responses(
            (status = 200, description = "A stream of server-sent events; each `data` is a `StreamEvent`.", body = StreamEvent, content_type = "text/event-stream"),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn stream_run_events() {}

    /// Send a signal (`signal_run`)
    ///
    /// The body, any JSON value, is the signal's payload (`null` when empty). It resolves the
    /// script's `signal(name)`, now or the next time the script waits for that name.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/signals/{name}", tag = "runs", operation_id = "signalRun",
        params(
            ("id" = String, Path, description = "The run id."),
            ("name" = String, Path, description = "The signal's name."),
            ("Idempotency-Key" = Option<String>, Header, description = "The same key sent twice to a run delivers one signal."),
        ),
        request_body(content = Option<Value>, description = "The payload: any JSON value. `null` when the body is empty.", content_type = "application/json"),
        responses(
            (status = 202, description = "In the run's mailbox.", body = SignalAccepted),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The payload is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn signal_run() {}

    /// Cancel a run (`cancel_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/cancel", tag = "runs", operation_id = "cancelRun",
        params(("id" = String, Path, description = "The run id.")),
        request_body(content = Option<CancelRun>, description = "Optional."),
        responses(
            (status = 202, description = "Cancellation requested; the run's state, after `wait` if given.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn cancel_run() {}

    /// Fork a run with a corrected script (`retry_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/fork", tag = "runs", operation_id = "forkRun",
        params(
            ("id" = String, Path, description = "The parent run's id."),
            ("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key returns the fork the first one started (200)."),
        ),
        request_body = ForkRun,
        responses(
            (status = 201, description = "The fork, a new run.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The corrected script was refused (`invalid-script`).", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "Too many active runs (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fork_run() {}

    /// Export the journal, or a cassette
    ///
    /// A document of the stored format v1 (`archi/02-journal.md`).
    #[utoipa::path(
        get, path = "/v1/runs/{id}/journal", tag = "runs", operation_id = "exportRunJournal",
        params(("id" = String, Path, description = "The run id."), JournalQuery),
        responses(
            (status = 200, description = "The journal document or the cassette, stored format v1.", body = Object),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn export_run_journal() {}

    /// Take a run out of quarantine
    ///
    /// The run is activated again, typically after a fix of the platform.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/release", tag = "runs", operation_id = "releaseRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 200, description = "Released.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run is not quarantined (`not-quarantined`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn release_run() {}

    /// Give up a quarantined run
    ///
    /// The host ends it: `run_aborted` with `non_determinism`, or `internal` after repeated
    /// failures.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/give-up", tag = "runs", operation_id = "giveUpRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 200, description = "Given up; the run is aborted.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run is not quarantined (`not-quarantined`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn give_up_run() {}

    /// Set or renew the run's credentials
    #[utoipa::path(
        put, path = "/v1/runs/{id}/credentials", tag = "runs", operation_id = "setRunCredentials",
        params(("id" = String, Path, description = "The run id.")),
        request_body = Credentials,
        responses(
            (status = 204, description = "Stored. The run's calls parked by `credentials.expired` are ready again."),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_run_credentials() {}

    // ---------------------------------------------------------------------------------------
    // Host-tool tasks

    /// Claim host-tool tasks (long-poll)
    #[utoipa::path(
        get, path = "/v1/tasks", tag = "tasks", operation_id = "claimTasks",
        params(TaskQuery),
        responses((status = 200, description = "The tasks claimed; none when `wait` passed.", body = TaskBatch))
    )]
    pub(super) fn claim_tasks() {}

    /// Keep a claim alive
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/heartbeat", tag = "tasks", operation_id = "heartbeatTask",
        params(("token" = String, Path, description = "The claim's token.")),
        responses(
            (status = 200, description = "Extended.", body = HeartbeatAck),
            (status = 410, description = "The task no longer exists: stop working on it (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn heartbeat_task() {}

    /// Answer later
    ///
    /// The task waits for `/complete` or `/fail` with the same token, without heartbeats, up to
    /// its timeout.
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/defer", tag = "tasks", operation_id = "deferTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body(content = Option<DeferTask>, description = "Optional."),
        responses(
            (status = 204, description = "Deferred."),
            (status = 410, description = "The task no longer exists (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn defer_task() {}

    /// Complete a task with its result
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/complete", tag = "tasks", operation_id = "completeTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = CompleteTask,
        responses(
            (status = 204, description = "Delivered to the run."),
            (status = 410, description = "The task no longer exists; the result is dropped (`task-gone`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The result is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn complete_task() {}

    /// Fail a task
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/fail", tag = "tasks", operation_id = "failTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = FailTask,
        responses(
            (status = 204, description = "Recorded: delivered as a `ToolError`, or retried."),
            (status = 410, description = "The task no longer exists (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fail_task() {}

    /// Approve a task that waits for the harness
    ///
    /// A task under the `approval` policy is sent; a task parked by a schema change continues
    /// against the new schema.
    #[utoipa::path(
        post, path = "/v1/tasks/{id}/approve", tag = "tasks", operation_id = "approveTask",
        params(("id" = String, Path, description = "The task's id, from the run's state or the webhook.")),
        responses(
            (status = 204, description = "The task is ready."),
            (status = 404, description = "No such task.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The task does not wait for the harness (`task-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn approve_task() {}

    /// Reject a task that waits for the harness
    ///
    /// The call fails with a `ToolError`.
    #[utoipa::path(
        post, path = "/v1/tasks/{id}/reject", tag = "tasks", operation_id = "rejectTask",
        params(("id" = String, Path, description = "The task's id.")),
        request_body(content = Option<RejectTask>, description = "Optional."),
        responses(
            (status = 204, description = "The call failed."),
            (status = 404, description = "No such task.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The task does not wait for the harness (`task-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn reject_task() {}

    // ---------------------------------------------------------------------------------------
    // Webhooks

    /// Inbound webhook: becomes a signal
    ///
    /// At the URL a script got from `webhook(name)`. A JSON body is the signal's payload; any
    /// other body becomes the payload as a string; an empty body is `null`. The first of
    /// `Idempotency-Key`, `webhook-id` and `X-GitHub-Delivery` present is the signal's
    /// idempotency key.
    #[utoipa::path(
        post, path = "/hooks/{token}", tag = "webhooks", operation_id = "postHook",
        params(("token" = String, Path, description = "The unguessable token of the URL.")),
        request_body(content = Option<Value>, description = "Anything: the signal's payload.", content_type = "application/json"),
        security(()),
        responses(
            (status = 202, description = "Delivered to the run as a signal."),
            (status = 404, description = "No such webhook.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The body is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn post_hook() {}

    /// Subscribe to lifecycle events
    ///
    /// The answer holds the signing secret, once.
    #[utoipa::path(
        post, path = "/v1/subscriptions", tag = "webhooks", operation_id = "createSubscription",
        request_body = CreateSubscription,
        responses((status = 201, description = "Subscribed.", body = Subscription))
    )]
    pub(super) fn create_subscription() {}

    /// List subscriptions
    #[utoipa::path(
        get, path = "/v1/subscriptions", tag = "webhooks", operation_id = "listSubscriptions",
        responses((status = 200, description = "Every subscription of the tenant, without secrets.", body = SubscriptionList))
    )]
    pub(super) fn list_subscriptions() {}

    /// Delete a subscription
    #[utoipa::path(
        delete, path = "/v1/subscriptions/{id}", tag = "webhooks", operation_id = "deleteSubscription",
        params(("id" = String, Path, description = "The subscription's id.")),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such subscription.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_subscription() {}

    // ---------------------------------------------------------------------------------------
    // Reading

    /// Latency, errors and retries per tool
    #[utoipa::path(
        get, path = "/v1/tools/stats", tag = "reading", operation_id = "getToolStats",
        params(ToolStatsFilter),
        responses((status = 200, description = "Statistics per tool over the window.", body = ToolStatsPage))
    )]
    pub(super) fn get_tool_stats() {}

    /// A payload's value
    #[utoipa::path(
        get, path = "/v1/payloads/{digest}", tag = "reading", operation_id = "getPayload",
        params(("digest" = String, Path, description = "SHA-256 of the canonical JSON, lowercase hex.")),
        responses(
            (status = 200, description = "The JSON value itself, in canonical form.", body = Value),
            (status = 404, description = "No such payload for this tenant.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_payload() {}

    // ---------------------------------------------------------------------------------------
    // Administration

    /// Create an API key
    #[utoipa::path(
        post, path = "/v1/api-keys", tag = "administration", operation_id = "createApiKey",
        request_body = CreateApiKey,
        responses((status = 201, description = "Created; the answer holds the key, once.", body = ApiKey))
    )]
    pub(super) fn create_api_key() {}

    /// Revoke an API key
    #[utoipa::path(
        delete, path = "/v1/api-keys/{id}", tag = "administration", operation_id = "revokeApiKey",
        params(("id" = String, Path, description = "The key's id.")),
        responses(
            (status = 204, description = "Revoked."),
            (status = 404, description = "No such key.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_api_key() {}

    /// What this server is
    #[utoipa::path(
        get, path = "/v1/server", tag = "administration", operation_id = "getServer",
        responses((status = 200, description = "The server's version, epochs and roles.", body = ServerInfo))
    )]
    pub(super) fn get_server() {}

    /// Stop the embedded server
    ///
    /// Only the embedded server stops this way, once its calls in flight are committed: the
    /// CLI uses it to replace a server of another version. Other servers answer 403.
    #[utoipa::path(
        post, path = "/v1/server/stop", tag = "administration", operation_id = "stopServer",
        responses((status = 202, description = "Stopping."))
    )]
    pub(super) fn stop_server() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::OPERATIONS;

    /// The document and the routes name the same operations.
    #[test]
    fn every_route_is_documented_and_nothing_else() {
        let document = serde_json::to_value(document()).unwrap();
        let mut documented: Vec<(String, String)> = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                if ["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                    documented.push((method.to_uppercase(), path.clone()));
                }
            }
        }
        documented.sort();
        let mut routes: Vec<(String, String)> = OPERATIONS
            .iter()
            .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
            .collect();
        routes.sort();
        assert_eq!(documented, routes);
    }

    #[test]
    fn every_error_answer_is_problem_json_and_every_operation_has_an_id() {
        let document = serde_json::to_value(document()).unwrap();
        assert_eq!(document["openapi"], "3.1.0");
        let mut ids = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let id = operation["operationId"].as_str().unwrap();
                ids.push(id.to_owned());
                for (status, response) in operation["responses"].as_object().unwrap() {
                    if status.starts_with('4') || status.starts_with('5') {
                        let content = response["content"].as_object();
                        assert!(
                            content.is_some_and(
                                |content| content.contains_key("application/problem+json")
                            ),
                            "{method} {path} {status}"
                        );
                    }
                }
            }
        }
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "operation ids are unique");
        let webhooks = document["webhooks"].as_object().unwrap();
        assert_eq!(webhooks.len(), WebhookEventType::ALL.len());
    }

    /// Generators refuse a document with a dangling reference.
    #[test]
    fn every_reference_resolves() {
        let document = serde_json::to_value(document()).unwrap();
        let schemas = document["components"]["schemas"].as_object().unwrap();
        let mut references = Vec::new();
        collect_references(&document, &mut references);
        assert!(!references.is_empty());
        for reference in references {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected reference {reference}"));
            assert!(schemas.contains_key(name), "{reference} does not resolve");
        }
    }

    fn collect_references(value: &serde_json::Value, references: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, field) in fields {
                    match (key.as_str(), field) {
                        ("$ref", serde_json::Value::String(reference)) => {
                            references.push(reference.clone());
                        }
                        _ => collect_references(field, references),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_references(item, references);
                }
            }
            _ => {}
        }
    }
}
