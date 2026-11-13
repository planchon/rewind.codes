//! `cargo run -p evals -- …`: lists the tasks, or runs models on them and writes the reports.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use evals::episode::{self, EpisodeConfig, EpisodeRecord};
use evals::model::{DEFAULT_MODELS, Model};
use evals::prompt::Rendering;
use evals::report::{self, Report};
use evals::tasks;
use rewind_check::TsgoChecker;
use tokio::task::JoinSet;

#[derive(Debug, Parser)]
#[command(
    name = "evals",
    about = "The eval suite of Rewind: models solve tasks end to end."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Lists the tasks.
    List,
    /// Runs models on tasks, and writes `report.json`, `summary.md` and the transcripts.
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// A model: claude-… (ANTHROPIC_API_KEY), gpt-… (OPENAI_API_KEY), or scripted:<file.json>.
    /// Repeat it for several. Claude Opus, Sonnet and Haiku and gpt-6-astra by default.
    #[arg(long = "model")]
    models: Vec<String>,
    /// compact, dts or search; repeat it to compare renderings, or `all`. compact by default.
    #[arg(long = "rendering")]
    renderings: Vec<String>,
    /// A task id (see `list`); repeat it for several. Every task by default.
    #[arg(long = "task")]
    tasks: Vec<String>,
    /// Episodes per task, model and rendering.
    #[arg(long, default_value_t = 1)]
    repeat: u32,
    /// Episodes run at the same time.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Model turns per episode, at most.
    #[arg(long, default_value_t = 30)]
    max_turns: usize,
    /// The models' reasoning effort; `default` leaves each API's default.
    #[arg(long, default_value = "high")]
    effort: String,
    /// Where to write the reports; `target/evals/<time>` by default.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::List => {
            list();
            ExitCode::SUCCESS
        }
        Command::Run(args) => match run(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("evals: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn list() {
    for task in tasks::all() {
        println!(
            "{:<28} {:<7} {}",
            task.id,
            format!("{:?}", task.difficulty).to_lowercase(),
            task.features.join(", ")
        );
    }
}

async fn run(args: RunArgs) -> Result<(), String> {
    if std::env::var("TZ").as_deref() != Ok("UTC") {
        return Err("the sandbox needs TZ=UTC: run through cargo, or set TZ=UTC".into());
    }
    let tasks = tasks::select(&args.tasks)?;
    let renderings = renderings(&args.renderings)?;
    let names: Vec<String> = if args.models.is_empty() {
        DEFAULT_MODELS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else {
        args.models.clone()
    };
    let effort = (args.effort != "default").then_some(args.effort.as_str());
    let mut models = Vec::new();
    for name in &names {
        models.push(Arc::new(
            Model::from_name(name, effort).map_err(|err| format!("{name}: {err}"))?,
        ));
    }
    let checker = Arc::new(TsgoChecker::new(TsgoChecker::default_cache_dir()));
    checker
        .install()
        .await
        .map_err(|err| format!("the type-checker cannot be installed: {err}"))?;

    let now = rewind_engine::ports::Clock::now(&rewind_engine::adapters::SystemClock);
    let started_at = evals::clock::rfc3339(rewind_core::Timestamp(now.0 / 1_000 * 1_000));
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/evals").join(started_at.replace(':', "-")));
    std::fs::create_dir_all(out.join("transcripts"))
        .map_err(|err| format!("{}: {err}", out.display()))?;

    let config = EpisodeConfig {
        max_turns: args.max_turns,
        ..EpisodeConfig::default()
    };
    let mut plan = Vec::new();
    for model in &models {
        for rendering in &renderings {
            for task in &tasks {
                for repetition in 0..args.repeat {
                    plan.push((model.clone(), *rendering, task.clone(), repetition));
                }
            }
        }
    }
    let total = plan.len();
    eprintln!(
        "evals: {total} episodes ({} tasks × {} models × {} renderings × {}), writing to {}",
        tasks.len(),
        models.len(),
        renderings.len(),
        args.repeat,
        out.display()
    );

    let mut pending = plan.into_iter();
    let mut running = JoinSet::new();
    let mut records: Vec<EpisodeRecord> = Vec::new();
    loop {
        while running.len() < args.jobs.max(1) {
            let Some((model, rendering, task, repetition)) = pending.next() else {
                break;
            };
            let checker = checker.clone();
            let config = config.clone();
            running.spawn(async move {
                episode::run(&task, &model, rendering, checker, &config, repetition).await
            });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let episode = joined.map_err(|err| format!("an episode panicked: {err}"))?;
        let record = &episode.record;
        let verdict = match (&record.error, record.success) {
            (Some(error), _) => format!("error: {error}"),
            (None, true) => "success".to_owned(),
            (None, false) => format!("failure: {}", record.failure.as_deref().unwrap_or("")),
        };
        eprintln!(
            "[{}/{total}] {} · {} ({}) #{}: {verdict} ({} turns)",
            records.len() + 1,
            record.task,
            record.model,
            record.rendering,
            record.repetition,
            record.iterations
        );
        let file = out.join("transcripts").join(format!(
            "{}--{}--{}--{}.json",
            sanitize(&record.model),
            record.rendering,
            record.task,
            record.repetition
        ));
        write(&file, &pretty(&episode.transcript()))?;
        records.push(episode.record);
    }

    records.sort_by(|a, b| {
        (&a.model, a.rendering, &a.task, a.repetition).cmp(&(
            &b.model,
            b.rendering,
            &b.task,
            b.repetition,
        ))
    });
    let report = Report {
        started_at,
        models: names,
        renderings,
        tasks: tasks.iter().map(|task| task.id.to_owned()).collect(),
        repeat: args.repeat,
        summaries: report::summarize(&records),
        episodes: records
            .iter()
            .map(|record| serde_json::to_value(record).unwrap_or_default())
            .collect(),
    };
    write(
        &out.join("report.json"),
        &pretty(&serde_json::to_value(&report).unwrap_or_default()),
    )?;
    let summary = report::markdown(&report, &records);
    write(&out.join("summary.md"), &summary)?;
    println!("{summary}");
    Ok(())
}

fn renderings(names: &[String]) -> Result<Vec<Rendering>, String> {
    if names.is_empty() {
        return Ok(vec![Rendering::Compact]);
    }
    if names.iter().any(|name| name == "all") {
        return Ok(Rendering::ALL.to_vec());
    }
    let mut renderings = Vec::new();
    for name in names {
        let rendering: Rendering = name.parse()?;
        if !renderings.contains(&rendering) {
            renderings.push(rendering);
        }
    }
    Ok(renderings)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn write(path: &std::path::Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|err| format!("{}: {err}", path.display()))
}
