//! The command line (`archi/07-code-architecture.md`, "Embedded server and CLI").

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rewind_api::common::{Duration, RunStatus};

/// The `rewind` command line, but `rewind server`, which the binary hands to `rewind-server`.
#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Rewind: durable TypeScript scripts that call tools, in a deterministic sandbox.",
    long_about = "Rewind: durable TypeScript scripts that call tools, in a deterministic sandbox.\n\n\
        Commands about runs go to a server: the remote one of --url (REWIND_URL) with its API key \
        (REWIND_API_KEY), or else the embedded server of the state directory (.rewind/), started on \
        demand. `rewind check`, `rewind test` and the replay of an exported journal run in this \
        process.\n\nExit codes: 0 completed or success, 1 failed, 2 cancelled, 3 aborted, 4 \
        still waiting, 5 quarantined, 6 refused, 64 usage error, 65 script refused, 70 internal \
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
    /// Type-check a script against the sandbox and a toolset's declarations.
    Check(CheckArgs),
    /// Run tests in virtual time: in this process, or on the server for a workflow's suite.
    Test(TestArgs),
    /// Send a signal to a run.
    Signal(SignalArgs),
    /// Cancel a run.
    Cancel(CancelArgs),
    /// Show a run's state, waiting up to --wait for it to end.
    Status(StatusArgs),
    /// Replay a run, or an exported journal, in this process, and check determinism.
    Replay(ReplayArgs),
    /// Start a fork of a run with a corrected script.
    Fork(ForkArgs),
    /// The LLM-readable trace of a run, or one entry in full.
    Trace(TraceArgs),
    /// List runs, most recently started first.
    Runs(RunsArgs),
    /// Take a run out of quarantine, or give it up.
    Release(ReleaseArgs),
    /// Export a run's journal document, or a cassette of its calls and signals.
    Export(ExportArgs),
    /// Register toolsets and read their declarations.
    #[command(subcommand)]
    Toolsets(ToolsetsCommand),
    /// The namespaces (the end users) and their use; one namespace; their limits; erase one.
    #[command(subcommand)]
    Namespaces(NamespacesCommand),
    /// Record tests from a workflow's past runs.
    #[command(subcommand)]
    Workflows(WorkflowsCommand),
    /// The organization's secrets, or a namespace's with --namespace: never a value.
    #[command(subcommand)]
    Secrets(SecretsCommand),
    /// Serve the agent tools over MCP on stdin and stdout, to debug them.
    #[command(
        long_about = "Serves the agent tools over MCP on stdin and stdout, against the embedded \
            server of the state directory (.rewind/, or --dir), started on demand, as its tenant \
            `local`: to debug them with an MCP client on this machine, such as the MCP Inspector \
            (npx @modelcontextprotocol/inspector rewind mcp) or Claude Code (claude mcp add \
            rewind -- rewind mcp). Never a remote server: Claude reaches a deployed Rewind at its \
            /mcp URL, with OAuth. --namespace narrows the reads that name no namespace."
    )]
    Mcp,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkflowsCommand {
    /// Record tests from the workflow's past runs, one per path they took; print their digests,
    /// which a save of the workflow takes in `tests`.
    Record {
        /// The workflow's name.
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
    /// Deletes a secret: the next call of a run that attaches it is parked.
    Delete { name: String },
    /// What reads a secret: the active runs that attach it, those waiting on a call it serves,
    /// the workflows whose latest version attaches it.
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
    /// Do not type-check the script before starting it.
    #[arg(long)]
    pub(crate) no_check: bool,
    /// A secret the run reads, for an MCP server or an API whose calls need one:
    /// `github=ns/github` (the end user's), `zendesk=org/zendesk` (the organization's). Repeat
    /// for several. A fork reads its parent's without it.
    #[arg(long = "secret", value_name = "SERVER=SCOPE/NAME")]
    pub(crate) secrets: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// The TypeScript script.
    pub(crate) script: PathBuf,
    /// The input of main: JSON, or @file.json.
    #[arg(long, value_name = "JSON")]
    pub(crate) input: Option<String>,
    /// The toolset: the id of a registered toolset, or a toolset file to register.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) toolset: Option<String>,
    #[command(flatten)]
    pub(crate) start: StartOptions,
}

#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// The TypeScript script.
    pub(crate) script: PathBuf,
    /// The toolset: a toolset file, pinned in this process, or the id of a toolset registered
    /// on the server, which then checks the script.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) toolset: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TestArgs {
    /// The script under test. With --workflow, the version's own when absent.
    pub(crate) script: Option<PathBuf>,
    /// The tests: a JSON array of tests. Default: script.tests.json next to the script.
    #[arg(long, value_name = "FILE")]
    pub(crate) tests: Option<PathBuf>,
    /// A workflow, `triage` or `triage@<version>`: its suite runs too, on the server, which
    /// checks every test against its interface.
    #[arg(long, value_name = "NAME")]
    pub(crate) workflow: Option<String>,
    /// The toolset: a toolset file, or the id of a toolset registered on the server, where the
    /// tests then run. Default: the workflow's, or else the tools the tests answer.
    #[arg(long, value_name = "ID|FILE")]
    pub(crate) toolset: Option<String>,
    /// Static responses per tool, tried last, in this process. Default: rewind.tools.json next
    /// to the tests.
    #[arg(long, value_name = "FILE")]
    pub(crate) tools: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct SignalArgs {
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
    pub(crate) run: String,
    /// The message of the CancelledError the script sees.
    #[arg(long)]
    pub(crate) reason: Option<String>,
    /// Wait up to this long for the run to end, and exit with its status.
    #[arg(long, value_name = "DURATION")]
    pub(crate) wait: Option<Duration>,
}

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    pub(crate) run: String,
    /// Wait up to this long for the run to end.
    #[arg(long, value_name = "DURATION")]
    pub(crate) wait: Option<Duration>,
}

#[derive(Debug, Args)]
pub(crate) struct ReplayArgs {
    /// A run id, or the path of an exported journal document.
    pub(crate) run: String,
    /// Check whether this corrected script is compatible with the journal instead, and what a
    /// fork with it would reuse.
    #[arg(long, value_name = "FILE")]
    pub(crate) script: Option<PathBuf>,
    /// With --script: a fork would go live from this entry.
    #[arg(long, value_name = "INDEX", requires = "script")]
    pub(crate) at: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct ForkArgs {
    pub(crate) run: String,
    /// The corrected script.
    #[arg(long, value_name = "FILE")]
    pub(crate) script: PathBuf,
    /// Go live from this journal entry even if the commands still match.
    #[arg(long, value_name = "INDEX")]
    pub(crate) at: Option<u64>,
    /// The fork's input: JSON, or @file.json. The parent's by default.
    #[arg(long, value_name = "JSON")]
    pub(crate) input: Option<String>,
    #[command(flatten)]
    pub(crate) start: StartOptions,
}

#[derive(Debug, Args)]
pub(crate) struct TraceArgs {
    pub(crate) run: String,
    /// One journal entry in full, its payload included.
    #[arg(long, value_name = "INDEX")]
    pub(crate) entry: Option<u64>,
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
    /// runs that have not ended last), or the most tool calls first.
    #[arg(long, value_enum)]
    pub(crate) sort: Option<SortArg>,
    #[arg(long, value_enum)]
    pub(crate) status: Option<StatusFilter>,
    /// Only runs carrying this label, key:value or key=value. Repeat to require several.
    #[arg(long = "label", value_name = "KEY:VALUE")]
    pub(crate) labels: Vec<String>,
    /// Only runs that called this tool.
    #[arg(long)]
    pub(crate) tool: Option<String>,
    /// Only runs of this script (digest).
    #[arg(long)]
    pub(crate) script: Option<String>,
    /// Only runs whose calls failed with this kind, or that failed with this error name.
    #[arg(long)]
    pub(crate) error: Option<String>,
    /// Only quarantined runs.
    #[arg(long)]
    pub(crate) quarantined: bool,
    /// Only forks of this run.
    #[arg(long)]
    pub(crate) parent: Option<String>,
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
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
    Aborted,
}

impl From<StatusFilter> for RunStatus {
    fn from(status: StatusFilter) -> Self {
        match status {
            StatusFilter::Pending => RunStatus::Pending,
            StatusFilter::Active => RunStatus::Active,
            StatusFilter::Completed => RunStatus::Completed,
            StatusFilter::Failed => RunStatus::Failed,
            StatusFilter::Cancelled => RunStatus::Cancelled,
            StatusFilter::Aborted => RunStatus::Aborted,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ReleaseArgs {
    pub(crate) run: String,
    /// Give the run up instead: it is aborted.
    #[arg(long)]
    pub(crate) give_up: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    pub(crate) run: String,
    /// What to export.
    #[arg(long = "as", value_enum, default_value = "journal")]
    pub(crate) export_as: ExportFormat,
    /// Write it to this file instead of stdout.
    #[arg(long, short, value_name = "FILE")]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ExportFormat {
    /// The journal document: replayable with `rewind replay <file>`.
    Journal,
    /// A cassette of the run's calls and signals, for `rewind test`.
    Cassette,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ToolsetsCommand {
    /// Register and pin a toolset file; print its id.
    Add {
        /// MCP servers, host tools and the http tool, as `POST /v1/toolsets` takes them.
        file: PathBuf,
    },
    /// The declarations of a registered toolset.
    Declarations {
        id: String,
        #[arg(long, value_enum, default_value = "dts")]
        format: DeclarationsFormat,
    },
    /// Search a registered toolset's tools.
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
    /// The order: the most CPU (the default), runs or scripts of the month first, or by name.
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
    Scripts,
    Name,
}

impl From<NamespaceOrderArg> for rewind_api::namespaces::NamespaceOrder {
    fn from(order: NamespaceOrderArg) -> Self {
        match order {
            NamespaceOrderArg::Cpu => Self::Cpu,
            NamespaceOrderArg::Runs => Self::Runs,
            NamespaceOrderArg::Scripts => Self::Scripts,
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
    /// CPU time of the scripts in a calendar month, such as 2h: alerts at 80 % and 100 %.
    #[arg(long, value_name = "DURATION|none")]
    pub(crate) cpu_per_month: Option<Limit<Duration>>,
    /// Runs active at once.
    #[arg(long, value_name = "N|none")]
    pub(crate) active_runs: Option<Limit<u32>>,
    /// Runs created in a calendar month, forks included.
    #[arg(long, value_name = "N|none")]
    pub(crate) runs_per_month: Option<Limit<u64>>,
    /// Distinct scripts with a run kept.
    #[arg(long, value_name = "N|none")]
    pub(crate) scripts: Option<Limit<u64>>,
    /// The longest TTL a run may ask for.
    #[arg(long, value_name = "DURATION|none")]
    pub(crate) max_ttl: Option<Limit<Duration>>,
    /// The most lines a script may have.
    #[arg(long, value_name = "N|none")]
    pub(crate) max_script_lines: Option<Limit<u32>>,
}

impl QuotasArgs {
    /// Whether a limit is to be set.
    pub(crate) fn changes(&self) -> bool {
        self.cpu_per_month.is_some()
            || self.active_runs.is_some()
            || self.runs_per_month.is_some()
            || self.scripts.is_some()
            || self.max_ttl.is_some()
            || self.max_script_lines.is_some()
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
        if let Some(limit) = self.scripts {
            limit.apply(&mut quotas.scripts);
        }
        if let Some(limit) = self.max_ttl {
            limit.apply(&mut quotas.max_ttl);
        }
        if let Some(limit) = self.max_script_lines {
            limit.apply(&mut quotas.max_script_lines);
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum DeclarationsFormat {
    /// rewind-tools.d.ts, for the type-checker.
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
            scripts: Some(9),
            ..Default::default()
        };
        quotas.apply(&mut limits);
        assert_eq!(limits.active_runs, Some(5));
        assert_eq!(limits.max_ttl, None);
        assert_eq!(limits.scripts, Some(9), "a limit not given is kept");
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
                "--scripts",
                "x"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["rewind", "namespaces", "quotas", "--defaults"]).is_ok());
    }
}
