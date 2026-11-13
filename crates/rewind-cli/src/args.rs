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
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Start a run and wait for it, up to --wait.
    Run(RunArgs),
    /// Type-check a script against the sandbox and a toolset's declarations.
    Check(CheckArgs),
    /// Run a scenario against mocks and cassettes, in virtual time, in this process.
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
    /// The scenario: its script, input, mocks, signals and expectations.
    pub(crate) scenario: PathBuf,
    /// Static responses per tool. Default: rewind.tools.json next to the scenario, if present.
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
    /// The order: the most recently started first (the default), the most recent activity
    /// first, or the soonest to wake by itself first.
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
    Activity,
    Wakes,
}

impl From<SortArg> for rewind_api::runs::RunSort {
    fn from(sort: SortArg) -> Self {
        match sort {
            SortArg::Started => Self::Started,
            SortArg::Activity => Self::Activity,
            SortArg::Wakes => Self::Wakes,
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
}
