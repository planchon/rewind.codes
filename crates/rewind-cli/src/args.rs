//! The command line (`archi/07-code-architecture.md`, "Embedded server and CLI").

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rewind_api::common::{Duration, RunStatus};
use rewind_api::query::WaitKind;

/// The `rewind` command line, but `rewind server`, which the binary hands to `rewind-server`.
#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Rewind: durable TypeScript workflows that call actions, in a deterministic sandbox.",
    long_about = "Rewind: durable TypeScript workflows that call actions, in a deterministic sandbox.\n\n\
        Commands about runs go to a server: the remote one of --url (REWIND_URL) with its API key \
        (REWIND_API_KEY), or else the embedded server of the state directory (.rewind/), started on \
        demand. `rewind check`, `rewind test` and the replay of a run export run in this \
        process.\n\nExit codes: 0 completed or success, 1 failed, 2 cancelled, 3 aborted, 4 \
        still waiting, 5 quarantined, 6 refused, 64 usage error, 65 code refused, 70 internal \
        error, 130 interrupted.",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: Global,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Options every command takes.
#[derive(Debug, Clone, Args)]
pub(crate) struct Global {
    /// Print the result as JSON on stdout, and errors as problem+json on stderr.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    /// The state directory of the embedded server.
    #[arg(
        long,
        global = true,
        env = "REWIND_DIR",
        value_name = "DIR",
        default_value = ".rewind"
    )]
    pub(crate) dir: PathBuf,
    /// A remote server, such as https://rewind.example.com, instead of the embedded one.
    #[arg(long, global = true, env = "REWIND_URL", value_name = "URL")]
    pub(crate) url: Option<String>,
    /// The API key of the remote server (rewind_sk_…).
    #[arg(
        long,
        global = true,
        env = "REWIND_API_KEY",
        value_name = "KEY",
        hide_env_values = true
    )]
    pub(crate) api_key: Option<String>,

    /// The namespace (the end user) the commands act for. Against the embedded server, `local`
    /// when not given.
    #[arg(long, global = true, env = "REWIND_NAMESPACE", value_name = "NAME")]
    pub(crate) namespace: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Start a run and wait for it, up to --wait.
    Run(RunArgs),
    /// Type-check a code against the sandbox and an action set's declarations.
    Check(CheckArgs),
    /// Run tests in virtual time: in this process, or on the server for a workflow's suite.
    Test(TestArgs),
    /// Send a signal to a run.
    Signal(SignalArgs),
    /// Cancel a run.
    Cancel(CancelArgs),
    /// Show a run's state, waiting up to --wait for it to end.
    Status(StatusArgs),
    /// Replay a run, or a run export, in this process, and check determinism.
    Replay(ReplayArgs),
    /// Start a fork of a run with a corrected code.
    Fork(ForkArgs),
    /// The run's transcript, for a model, or one event in full.
    Transcript(TranscriptArgs),
    /// List runs, most recently started first.
    Runs(RunsArgs),
    /// Take a run out of quarantine, or give it up.
    Release(ReleaseArgs),
    /// Export a run's run export, or a cassette of its calls and signals.
    Export(ExportArgs),
    /// Register action sets and read their declarations.
    #[command(subcommand)]
    ActionSets(ActionSetsCommand),
    /// The namespaces (the end users) and their use; one namespace; their limits; erase one.
    #[command(subcommand)]
    Namespaces(NamespacesCommand),
    /// The workflows: list, show, create, update, promote, fork and pull them; record tests
    /// from their past runs.
    #[command(subcommand)]
    Workflows(WorkflowsCommand),
    /// The organization's secrets, or a namespace's with --namespace: never a value.
    #[command(subcommand)]
    Secrets(SecretsCommand),
    /// Serve the agent tools over MCP on stdin and stdout, to debug them.
    #[command(
        long_about = "Serves the agent tools over MCP on stdin and stdout, against the embedded \
            server of the state directory (.rewind/, or --dir), started on demand, as its \
            organization `local`: to debug them with an MCP client on this machine, such as the \
            MCP Inspector (npx @modelcontextprotocol/inspector rewind mcp) or Claude Code (claude \
            mcp add rewind -- rewind mcp). Never a remote server: Claude reaches a deployed Rewind \
            at its /mcp URL, with OAuth. --namespace narrows the reads that name no namespace."
    )]
    Mcp,
}

/// A workflow, as the commands name it: its id (`wf_…`) or its name, which resolves in the
/// namespace first, then among the organization's workflows.
const WORKFLOW_HELP: &str = "The workflow: its id (wf_…) or its name";

#[derive(Debug, Subcommand)]
pub(crate) enum WorkflowsCommand {
    /// The workflows of the namespace and of the organization, by id, each with its latest
    /// version and what its runs did in the last 30 days.
    List {
        /// Only the namespace's, or only the organization's.
        #[arg(long, value_enum)]
        scope: Option<WorkflowScopeArg>,
        /// Only the workflows one of whose versions has this code: its digest.
        #[arg(long, value_name = "DIGEST")]
        code: Option<String>,
        /// At most this many workflows: 50 by default, 500 at most.
        #[arg(long)]
        limit: Option<u32>,
        /// The next page: the id the previous page printed.
        #[arg(long, value_name = "ID")]
        after: Option<String>,
    },
    /// One workflow: its scope, its versions newest first and its suite; `@v12` or `@<digest>`
    /// after it shows that version.
    Show {
        #[arg(help = WORKFLOW_HELP)]
        workflow: String,
        /// Print the code of the version shown.
        #[arg(long)]
        code: bool,
    },
    /// Creates a workflow from a code, with its version 1, through its gate: in the namespace
    /// --namespace names, else the organization's.
    Create {
        /// The TypeScript code.
        code: PathBuf,
        /// Its name, unique in its scope: an identifier (letters, digits, _ and $).
        #[arg(long)]
        name: Option<String>,
        /// What it does, for a model and a person: display only.
        #[arg(long)]
        description: Option<String>,
        /// The action set: the id of a registered action set, or an action set file to
        /// register. Without it, an empty one.
        #[arg(long, value_name = "ID|FILE")]
        action_set: Option<String>,
        /// Its interface: a JSON file of { "input", "output", "signals" } schemas. Without it,
        /// any input, output and signal.
        #[arg(long, value_name = "FILE")]
        interface: Option<PathBuf>,
        /// Its first suite: a tests file, as `rewind test` reads it.
        #[arg(long, value_name = "FILE")]
        tests: Option<PathBuf>,
        /// A secret its runs use: `github=ns/github`, `zendesk=org/zendesk`. Repeat for several.
        #[arg(long = "secret", value_name = "SERVER=SCOPE/NAME")]
        secrets: Vec<String>,
    },
    /// Names or describes a workflow; makes its next version through its gate (--code,
    /// --interface, --action-set, --secret), with the tests of --tests; or, with --tests alone,
    /// adds them to its suite.
    Update {
        #[arg(help = WORKFLOW_HELP)]
        workflow: String,
        /// Its new name; an empty one removes it.
        #[arg(long)]
        name: Option<String>,
        /// Its new description; an empty one removes it.
        #[arg(long)]
        description: Option<String>,
        /// The code of its next version.
        #[arg(long, value_name = "FILE")]
        code: Option<PathBuf>,
        /// The interface of its next version: a JSON file of { "input", "output", "signals" }.
        #[arg(long, value_name = "FILE")]
        interface: Option<PathBuf>,
        /// The action set of its next version: an id, or a file to register.
        #[arg(long, value_name = "ID|FILE")]
        action_set: Option<String>,
        /// Tests for its suite: a tests file.
        #[arg(long, value_name = "FILE")]
        tests: Option<PathBuf>,
        /// The secrets of its next version, in place of its latest version's.
        #[arg(long = "secret", value_name = "SERVER=SCOPE/NAME")]
        secrets: Vec<String>,
    },
    /// Moves a namespace's workflow to the organization, with its id, versions and suite.
    Promote {
        #[arg(help = WORKFLOW_HELP)]
        workflow: String,
    },
    /// Forks an organization's workflow into the namespace --namespace names: its latest
    /// version, or the one `@v12` names.
    Fork {
        #[arg(help = WORKFLOW_HELP)]
        workflow: String,
    },
    /// A fork takes its source's latest version once its own suite passes.
    Pull {
        #[arg(help = WORKFLOW_HELP)]
        workflow: String,
    },
    /// Record tests from the workflow's past runs, one per path they took; print their digests,
    /// which `rewind workflows update --tests` takes.
    Record {
        #[arg(help = WORKFLOW_HELP)]
        name: String,
        /// Which runs, in the query language of `rewind runs`: its completed and failed runs,
        /// newest first, by default.
        #[arg(long)]
        query: Option<String>,
        /// These runs rather than a query; repeat it for several.
        #[arg(long = "run", value_name = "ID")]
        runs: Vec<String>,
        /// At most this many tests: 20 by default, 100 at most.
        #[arg(long)]
        limit: Option<u32>,
    },
}

/// The scope `rewind workflows list` lists.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WorkflowScopeArg {
    Namespace,
    Organization,
}

impl From<WorkflowScopeArg> for rewind_api::workflows::WorkflowScope {
    fn from(scope: WorkflowScopeArg) -> Self {
        match scope {
            WorkflowScopeArg::Namespace => Self::Namespace,
            WorkflowScopeArg::Organization => Self::Organization,
        }
    }
}

/// `rewind secrets`: the organization's secrets, or those of the namespace `--namespace` names
/// (`archi/04-tools.md`, "Secrets"). A value is read from standard input, never from an
/// argument, and never printed.
#[derive(Debug, Subcommand)]
pub(crate) enum SecretsCommand {
    /// The secrets, each with its labels, who wrote it and when, and its reads.
    List,
    /// Creates or replaces a secret, its value read from standard input (one trailing newline
    /// dropped): `rewind secrets put github < token.txt`.
    Put {
        /// 1 to 64 lowercase letters, digits, - and _, starting with a letter.
        name: String,
        /// A label of an organization's secret, key=value. Repeat for several.
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
    },
    /// Deletes a secret: the next call of a run that uses it is parked.
    Delete { name: String },
    /// What reads a secret: the active runs that use it, those waiting on a call it serves,
    /// the workflows whose latest version uses it.
    Usage { name: String },
}

/// What starts a run: `rewind run` and `rewind fork` share them.
#[derive(Debug, Clone, Args)]
pub(crate) struct StartOptions {
    /// Wait up to this long for the run to end, such as 30s or 2m; 0 does not wait.
    #[arg(long, value_name = "DURATION", default_value = "30s")]
    pub(crate) wait: Duration,
    /// The run's TTL: it is cancelled at its deadline. 30 days by default.
    #[arg(long, value_name = "DURATION")]
    pub(crate) ttl: Option<Duration>,
    /// A label of the run, key=value. Repeat for several.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub(crate) labels: Vec<String>,
    /// An idempotency key: a request repeated with it returns the same run.
    #[arg(long, value_name = "KEY")]
    pub(crate) key: Option<String>,
    /// Do not type-check the code before starting it.
    #[arg(long)]
    pub(crate) no_check: bool,
    /// A secret the run reads, for an MCP server or an API whose calls need one:
    /// `github=ns/github` (the end user's), `zendesk=org/zendesk` (the organization's). Repeat
    /// for several. A fork reads its source's without it.
    #[arg(long = "secret", value_name = "SERVER=SCOPE/NAME")]
    pub(crate) secrets: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// The TypeScript code.
    pub(crate) code: PathBuf,
    /// The input of main: JSON, or @file.json.
    #[arg(long, value_name = "JSON")]
    pub(crate) input: Option<String>,
    /// The action set: the id of a registered action set, or an action set file to register.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) action_set: Option<String>,
    #[command(flatten)]
    pub(crate) start: StartOptions,
}

#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// The TypeScript code.
    pub(crate) code: PathBuf,
    /// The action set: an action set file, pinned in this process, or the id of an action set
    /// registered on the server, which then checks the code.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) action_set: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TestArgs {
    /// The code under test. With --workflow, the version's own when absent.
    pub(crate) code: Option<PathBuf>,
    /// The tests: a JSON array of tests. Default: <name>.tests.json next to the code, for <name>.ts.
    #[arg(long, value_name = "FILE")]
    pub(crate) tests: Option<PathBuf>,
    /// A workflow, `triage`, `triage@<version>` or `triage@v12`: its suite runs too, on the
    /// server, which
    /// checks every test against its interface.
    #[arg(long, value_name = "NAME")]
    pub(crate) workflow: Option<String>,
    /// The action set: an action set file, or the id of an action set registered on the server,
    /// where the tests then run. Default: the workflow's, or else the actions the tests answer.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) action_set: Option<String>,
    /// Static responses per action, tried last, in this process. Default: rewind.actions.json next
    /// to the tests.
    #[arg(long, value_name = "FILE")]
    pub(crate) actions: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct SignalArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// The signal's name.
    pub(crate) name: String,
    /// The payload: JSON, or @file.json. null by default.
    pub(crate) payload: Option<String>,
    /// An idempotency key: a signal sent again with it is not delivered twice.
    #[arg(long, value_name = "KEY")]
    pub(crate) key: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct CancelArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// The message of the CancelledError the code sees.
    #[arg(long)]
    pub(crate) reason: Option<String>,
    /// Wait up to this long for the run to end, and exit with its status.
    #[arg(long, value_name = "DURATION")]
    pub(crate) wait: Option<Duration>,
}

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// Wait up to this long for the run to end.
    #[arg(long, value_name = "DURATION")]
    pub(crate) wait: Option<Duration>,
}

#[derive(Debug, Args)]
pub(crate) struct ReplayArgs {
    /// A run, by its handle or its id, or the path of an exported run export.
    pub(crate) run: String,
    /// Check whether this corrected code is compatible with the history instead, and what a
    /// fork with it would reuse.
    #[arg(long, value_name = "FILE")]
    pub(crate) code: Option<PathBuf>,
    /// With --code: a fork would go live from this event.
    #[arg(long, value_name = "INDEX", requires = "code")]
    pub(crate) at: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct ForkArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// The corrected code.
    #[arg(long, value_name = "FILE")]
    pub(crate) code: PathBuf,
    /// Go live from this event even if the commands still match.
    #[arg(long, value_name = "INDEX")]
    pub(crate) at: Option<u64>,
    /// The fork's input: JSON, or @file.json. The source's by default.
    #[arg(long, value_name = "JSON")]
    pub(crate) input: Option<String>,
    #[command(flatten)]
    pub(crate) start: StartOptions,
}

#[derive(Debug, Args)]
pub(crate) struct TranscriptArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// One event in full, its payload included.
    #[arg(long, value_name = "INDEX")]
    pub(crate) event: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct RunsArgs {
    /// A query: terms separated by spaces, all of which must hold, as
    /// '@agent:triage status:failed,aborted -@env:test "rate limit"'. Joined to the options by
    /// "and".
    #[arg(value_name = "QUERY")]
    pub(crate) query: Option<String>,
    /// The order: the most recently started first (the default), the first started first, the
    /// most recent activity first, the soonest to wake by itself first, the longest first (the
    /// runs that have not ended last), or the most calls first.
    #[arg(long, value_enum)]
    pub(crate) sort: Option<SortArg>,
    /// Only runs with this status; `active`: queued, running or waiting.
    #[arg(long, value_enum)]
    pub(crate) status: Option<StatusFilter>,
    /// Only runs carrying this label, key:value or key=value. Repeat to require several.
    #[arg(long = "label", value_name = "KEY:VALUE")]
    pub(crate) labels: Vec<String>,
    /// Only runs that called this action.
    #[arg(long)]
    pub(crate) action: Option<String>,
    /// Only runs of this workflow: its id or its name.
    #[arg(long)]
    pub(crate) workflow: Option<String>,
    /// Only runs whose calls failed with this kind, or that failed with this error name.
    #[arg(long)]
    pub(crate) error: Option<String>,
    /// Only runs waiting for this: a signal, a timer, a harness action, an approval, a deferred
    /// answer, a parked call, or a release from quarantine.
    #[arg(long, value_enum, value_name = "KIND")]
    pub(crate) waiting: Option<WaitingFilter>,
    /// Only forks of this run: its handle or its id.
    #[arg(long)]
    pub(crate) source: Option<String>,
    /// At most this many runs.
    #[arg(long)]
    pub(crate) limit: Option<u32>,
    /// The next page: the cursor the previous page printed.
    #[arg(long)]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SortArg {
    Started,
    Oldest,
    Activity,
    Wakes,
    Duration,
    Calls,
}

impl From<SortArg> for rewind_api::runs::RunSort {
    fn from(sort: SortArg) -> Self {
        match sort {
            SortArg::Started => Self::Started,
            SortArg::Oldest => Self::Oldest,
            SortArg::Activity => Self::Activity,
            SortArg::Wakes => Self::Wakes,
            SortArg::Duration => Self::Duration,
            SortArg::Calls => Self::Calls,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum StatusFilter {
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Aborted,
    /// Queued, running or waiting: not ended.
    Active,
}

impl StatusFilter {
    /// The one status it names; `None` for `active`, three statuses, which only the query
    /// language names (`status:active`).
    pub(crate) fn status(self) -> Option<RunStatus> {
        Some(match self {
            StatusFilter::Active => return None,
            StatusFilter::Queued => RunStatus::Queued,
            StatusFilter::Running => RunStatus::Running,
            StatusFilter::Waiting => RunStatus::Waiting,
            StatusFilter::Completed => RunStatus::Completed,
            StatusFilter::Failed => RunStatus::Failed,
            StatusFilter::Cancelled => RunStatus::Cancelled,
            StatusFilter::Aborted => RunStatus::Aborted,
        })
    }
}

/// What a waiting run waits for, as `rewind runs --waiting` takes it.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WaitingFilter {
    Signal,
    Timer,
    Harness,
    Approval,
    Deferred,
    Parked,
    Quarantined,
}

impl WaitingFilter {
    /// The term of the query language that asks for it.
    pub(crate) fn term(self) -> String {
        let kind = match self {
            WaitingFilter::Signal => WaitKind::Signal,
            WaitingFilter::Timer => WaitKind::Timer,
            WaitingFilter::Harness => WaitKind::Harness,
            WaitingFilter::Approval => WaitKind::Approval,
            WaitingFilter::Deferred => WaitKind::Deferred,
            WaitingFilter::Parked => WaitKind::Parked,
            WaitingFilter::Quarantined => WaitKind::Quarantined,
        };
        format!("waiting:{}", kind.as_str())
    }
}

#[derive(Debug, Args)]
pub(crate) struct ReleaseArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// Give the run up instead: it is aborted.
    #[arg(long)]
    pub(crate) give_up: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// The run: its handle, such as dana/triage/06bqx3k9f2mt, or its id.
    pub(crate) run: String,
    /// What to export.
    #[arg(long = "as", value_enum, default_value = "run")]
    pub(crate) export_as: ExportFormat,
    /// Write it to this file instead of stdout.
    #[arg(long, short, value_name = "FILE")]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ExportFormat {
    /// The run export: replayable with `rewind replay <file>`.
    Run,
    /// A cassette of the run's calls and signals, for `rewind test`.
    Cassette,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ActionSetsCommand {
    /// Register and pin an action set file; print its id.
    Add {
        /// MCP servers, harness actions and the http action, as `POST /v1/action-sets` takes them.
        file: PathBuf,
    },
    /// The declarations of a registered action set.
    Declarations {
        id: String,
        #[arg(long, value_enum, default_value = "dts")]
        format: DeclarationsFormat,
    },
    /// Search a registered action set's actions.
    Search {
        id: String,
        query: String,
        #[arg(long)]
        limit: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum NamespacesCommand {
    /// The namespaces and their use this month, the most CPU first.
    List(NamespacesListArgs),
    /// One namespace: its limits, its use day by day, its alerts and its tokens in use.
    Show {
        name: String,
        /// The month, such as 2026-11; the current one by default.
        #[arg(long, value_name = "YYYY-MM")]
        month: Option<String>,
    },
    /// Gives a namespace a display name, shown beside its name, which stays its key; without
    /// one, removes it.
    Rename {
        name: String,
        /// The display name, such as "Acme Corp": at most 128 characters.
        display_name: Option<String>,
    },
    /// Sets a namespace's exceptions to the default limits, or the defaults with --defaults,
    /// and prints the limits. Without a limit to set, prints them.
    Quotas(QuotasArgs),
    /// Erases a namespace in the background: the end user's tokens, runs, secrets and use.
    Delete { name: String },
}

#[derive(Debug, Args)]
pub(crate) struct NamespacesListArgs {
    /// Only the namespaces whose name starts with this, or whose display name does, case
    /// aside.
    #[arg(long)]
    pub(crate) prefix: Option<String>,
    /// Only those that used at least this share of one of their limits: 0.8 for 80 %.
    #[arg(long, value_name = "SHARE")]
    pub(crate) above: Option<f64>,
    /// The order: the most CPU (the default), runs or workflows of the month first, or by name.
    #[arg(long, value_enum)]
    pub(crate) order: Option<NamespaceOrderArg>,
    /// The month whose use is listed, such as 2026-11; the current one by default.
    #[arg(long, value_name = "YYYY-MM")]
    pub(crate) month: Option<String>,
    /// At most this many namespaces.
    #[arg(long)]
    pub(crate) limit: Option<u32>,
    /// The next page: the cursor the previous page printed.
    #[arg(long)]
    pub(crate) after: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum NamespaceOrderArg {
    Cpu,
    Runs,
    Workflows,
    Name,
}

impl From<NamespaceOrderArg> for rewind_api::namespaces::NamespaceOrder {
    fn from(order: NamespaceOrderArg) -> Self {
        match order {
            NamespaceOrderArg::Cpu => Self::Cpu,
            NamespaceOrderArg::Runs => Self::Runs,
            NamespaceOrderArg::Workflows => Self::Workflows,
            NamespaceOrderArg::Name => Self::Name,
        }
    }
}

/// A limit to set, or `none` to remove it: a namespace's exception then follows the default,
/// and a default no longer limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Limit<T> {
    Set(T),
    None,
}

impl<T: std::str::FromStr> std::str::FromStr for Limit<T>
where
    T::Err: std::fmt::Display,
{
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        if text == "none" {
            return Ok(Limit::None);
        }
        text.parse()
            .map(Limit::Set)
            .map_err(|error| format!("{error}; or none to remove the limit"))
    }
}

impl<T> Limit<T> {
    /// The limit to keep, given the one in place.
    pub(crate) fn apply(self, slot: &mut Option<T>) {
        *slot = match self {
            Limit::Set(value) => Some(value),
            Limit::None => None,
        };
    }
}

#[derive(Debug, Args)]
pub(crate) struct QuotasArgs {
    /// The namespace whose exceptions to set.
    #[arg(required_unless_present = "defaults", conflicts_with = "defaults")]
    pub(crate) name: Option<String>,
    /// Set the default limits of every namespace without an exception instead.
    #[arg(long)]
    pub(crate) defaults: bool,
    /// CPU time of the workflows in a calendar month, such as 2h: alerts at 80 % and 100 %.
    #[arg(long, value_name = "DURATION|none")]
    pub(crate) cpu_per_month: Option<Limit<Duration>>,
    /// Runs active at once.
    #[arg(long, value_name = "N|none")]
    pub(crate) active_runs: Option<Limit<u32>>,
    /// Runs created in a calendar month, forks included.
    #[arg(long, value_name = "N|none")]
    pub(crate) runs_per_month: Option<Limit<u64>>,
    /// Distinct workflows with a run kept.
    #[arg(long, value_name = "N|none")]
    pub(crate) workflows: Option<Limit<u64>>,
    /// The longest TTL a run may ask for.
    #[arg(long, value_name = "DURATION|none")]
    pub(crate) max_ttl: Option<Limit<Duration>>,
    /// The most lines a code may have.
    #[arg(long, value_name = "N|none")]
    pub(crate) max_code_lines: Option<Limit<u32>>,
}

impl QuotasArgs {
    /// Whether a limit is to be set.
    pub(crate) fn changes(&self) -> bool {
        self.cpu_per_month.is_some()
            || self.active_runs.is_some()
            || self.runs_per_month.is_some()
            || self.workflows.is_some()
            || self.max_ttl.is_some()
            || self.max_code_lines.is_some()
    }

    /// `quotas` with the limits given set or removed, the others kept.
    pub(crate) fn apply(&self, quotas: &mut rewind_api::namespaces::Quotas) {
        if let Some(limit) = self.cpu_per_month {
            limit.apply(&mut quotas.cpu_per_month);
        }
        if let Some(limit) = self.active_runs {
            limit.apply(&mut quotas.active_runs);
        }
        if let Some(limit) = self.runs_per_month {
            limit.apply(&mut quotas.runs_per_month);
        }
        if let Some(limit) = self.workflows {
            limit.apply(&mut quotas.workflows);
        }
        if let Some(limit) = self.max_ttl {
            limit.apply(&mut quotas.max_ttl);
        }
        if let Some(limit) = self.max_code_lines {
            limit.apply(&mut quotas.max_code_lines);
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum DeclarationsFormat {
    /// rewind-actions.d.ts, for the type-checker.
    Dts,
    /// The compact rendering for a model's prompt.
    Prompt,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_options_go_anywhere() {
        let cli = Cli::try_parse_from(["rewind", "run", "a.ts", "--json", "--wait", "2m"]).unwrap();
        assert!(cli.global.json);
        let Command::Run(run) = cli.command else {
            panic!("not a run")
        };
        assert_eq!(run.start.wait.as_millis(), 120_000);
        assert!(Cli::try_parse_from(["rewind", "run", "a.ts", "--wait", "soon"]).is_err());
        assert!(Cli::try_parse_from(["rewind", "replay", "r1", "--at", "3"]).is_err());
    }

    #[test]
    fn quotas_name_a_namespace_or_the_defaults() {
        let cli = Cli::try_parse_from([
            "rewind",
            "namespaces",
            "quotas",
            "alice",
            "--active-runs",
            "5",
            "--max-ttl",
            "none",
            "--cpu-per-month",
            "2h",
        ])
        .unwrap();
        let Command::Namespaces(NamespacesCommand::Quotas(quotas)) = cli.command else {
            panic!("not quotas")
        };
        assert_eq!(quotas.name.as_deref(), Some("alice"));
        assert_eq!(quotas.active_runs, Some(Limit::Set(5)));
        assert_eq!(quotas.max_ttl, Some(Limit::None));
        let mut limits = rewind_api::namespaces::Quotas {
            max_ttl: Some("1h".parse().unwrap()),
            workflows: Some(9),
            ..Default::default()
        };
        quotas.apply(&mut limits);
        assert_eq!(limits.active_runs, Some(5));
        assert_eq!(limits.max_ttl, None);
        assert_eq!(limits.workflows, Some(9), "a limit not given is kept");
        assert_eq!(limits.cpu_per_month, Some("2h".parse().unwrap()));

        assert!(Cli::try_parse_from(["rewind", "namespaces", "quotas"]).is_err());
        assert!(
            Cli::try_parse_from(["rewind", "namespaces", "quotas", "alice", "--defaults"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "rewind",
                "namespaces",
                "quotas",
                "--defaults",
                "--workflows",
                "x"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["rewind", "namespaces", "quotas", "--defaults"]).is_ok());
    }
}
