//! Reports: per model and per prompt rendering, the first-try check pass rate, task success,
//! iterations and tokens, whether the model wrote tests and how often the gate refused its
//! fixes (`archi/05-testing-and-observability.md`, "Evals"), as JSON and as a readable summary.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::episode::EpisodeRecord;
use crate::model::Usage;
use crate::prompt::Rendering;

/// The numbers of one model with one rendering.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub model: String,
    pub rendering: Rendering,
    /// Episodes run.
    pub episodes: usize,
    /// Episodes that could not be measured (an API or harness error): left out of the rates.
    pub errors: usize,
    /// Of the measured episodes where the model submitted a code, the share whose first
    /// code passed the checks.
    pub first_check_pass_rate: Option<f64>,
    /// Of the measured episodes, the share whose task was done.
    pub success_rate: Option<f64>,
    /// Model turns per measured episode, on average.
    pub mean_iterations: Option<f64>,
    /// Workflows submitted per measured episode, on average.
    pub mean_scripts: Option<f64>,
    /// Of the measured episodes, the share where the model wrote at least one test.
    pub tests_written_rate: Option<f64>,
    /// Fixes the gate refused (a test of a workflow's suite did not pass) per measured
    /// episode, on average.
    pub mean_gate_refusals: Option<f64>,
    /// Tokens of the measured episodes.
    pub usage: Usage,
    /// Input and output tokens per measured episode, on average.
    pub mean_tokens: Option<f64>,
}

/// The JSON report of an eval run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// When the run started, RFC 3339.
    pub started_at: String,
    pub models: Vec<String>,
    pub renderings: Vec<Rendering>,
    pub tasks: Vec<String>,
    pub repeat: u32,
    pub summaries: Vec<Summary>,
    pub episodes: Vec<EpisodeRecordJson>,
}

/// An episode as the report keeps it: its record, as JSON.
pub type EpisodeRecordJson = serde_json::Value;

fn rate(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn mean(total: usize, count: usize) -> Option<f64> {
    (count > 0).then(|| total as f64 / count as f64)
}

/// The summaries of a set of episodes, by model then rendering.
pub fn summarize(episodes: &[EpisodeRecord]) -> Vec<Summary> {
    let mut groups: BTreeMap<(String, Rendering), Vec<&EpisodeRecord>> = BTreeMap::new();
    for episode in episodes {
        groups
            .entry((episode.model.clone(), episode.rendering))
            .or_default()
            .push(episode);
    }
    groups
        .into_iter()
        .map(|((model, rendering), episodes)| {
            let measured: Vec<&&EpisodeRecord> =
                episodes.iter().filter(|e| e.error.is_none()).collect();
            let checked: Vec<bool> = measured.iter().filter_map(|e| e.first_check).collect();
            let mut usage = Usage::default();
            for episode in &measured {
                usage.add(episode.usage);
            }
            let tokens = usage.total_input() + usage.output;
            Summary {
                model,
                rendering,
                episodes: episodes.len(),
                errors: episodes.len() - measured.len(),
                first_check_pass_rate: rate(
                    checked.iter().filter(|passed| **passed).count(),
                    checked.len(),
                ),
                success_rate: rate(
                    measured.iter().filter(|e| e.success).count(),
                    measured.len(),
                ),
                mean_iterations: mean(measured.iter().map(|e| e.iterations).sum(), measured.len()),
                mean_scripts: mean(measured.iter().map(|e| e.scripts).sum(), measured.len()),
                tests_written_rate: rate(
                    measured.iter().filter(|e| e.tests_written > 0).count(),
                    measured.len(),
                ),
                mean_gate_refusals: mean(
                    measured.iter().map(|e| e.gate_refusals).sum(),
                    measured.len(),
                ),
                usage,
                mean_tokens: mean(
                    usize::try_from(tokens).unwrap_or(usize::MAX),
                    measured.len(),
                ),
            }
        })
        .collect()
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "–".to_owned(), |value| format!("{:.0}%", value * 100.0))
}

fn decimal(value: Option<f64>) -> String {
    value.map_or_else(|| "–".to_owned(), |value| format!("{value:.1}"))
}

fn thousands(value: u64) -> String {
    if value >= 10_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

/// The readable summary: one table per model and rendering, then each task's outcomes.
pub fn markdown(report: &Report, episodes: &[EpisodeRecord]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Rewind evals, {}\n", report.started_at);
    let _ = writeln!(
        out,
        "{} tasks × {} models × {} renderings × {} repetitions.\n",
        report.tasks.len(),
        report.models.len(),
        report.renderings.len(),
        report.repeat
    );
    let _ = writeln!(
        out,
        "| Model | Rendering | First-try check | Success | Iterations | Workflows | Wrote tests | Gate refusals | Tokens in | Tokens out | Errors |"
    );
    let _ = writeln!(
        out,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    for summary in &report.summaries {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            summary.model,
            summary.rendering,
            percent(summary.first_check_pass_rate),
            percent(summary.success_rate),
            decimal(summary.mean_iterations),
            decimal(summary.mean_scripts),
            percent(summary.tests_written_rate),
            decimal(summary.mean_gate_refusals),
            thousands(summary.usage.total_input()),
            thousands(summary.usage.output),
            summary.errors,
        );
    }

    let columns: Vec<(String, Rendering)> = report
        .summaries
        .iter()
        .map(|summary| (summary.model.clone(), summary.rendering))
        .collect();
    let _ = writeln!(out, "\n## Tasks\n");
    let _ = write!(out, "| Task | Difficulty |");
    for (model, rendering) in &columns {
        let _ = write!(out, " {model} ({rendering}) |");
    }
    let _ = write!(out, "\n| --- | --- |");
    for _ in &columns {
        let _ = write!(out, " --- |");
    }
    let _ = writeln!(out);
    for task in &report.tasks {
        let difficulty = episodes
            .iter()
            .find(|e| &e.task == task)
            .map(|e| format!("{:?}", e.difficulty).to_lowercase())
            .unwrap_or_default();
        let _ = write!(out, "| {task} | {difficulty} |");
        for (model, rendering) in &columns {
            let cell: Vec<&EpisodeRecord> = episodes
                .iter()
                .filter(|e| &e.task == task && &e.model == model && e.rendering == *rendering)
                .collect();
            let passed = cell.iter().filter(|e| e.success).count();
            let errors = cell.iter().filter(|e| e.error.is_some()).count();
            let errors = if errors > 0 {
                format!(" ({errors} errors)")
            } else {
                String::new()
            };
            let _ = write!(out, " {passed}/{}{errors} |", cell.len());
        }
        let _ = writeln!(out);
    }

    let failures: Vec<&EpisodeRecord> = episodes.iter().filter(|e| !e.success).collect();
    if !failures.is_empty() {
        let _ = writeln!(out, "\n## Failures\n");
        for episode in failures {
            let why = episode
                .error
                .as_deref()
                .map(|error| format!("error: {error}"))
                .or_else(|| episode.failure.clone())
                .unwrap_or_default();
            let limit = if episode.turn_limit {
                " (turn limit)"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "- {} · {} ({}) #{}{limit}: {}",
                episode.task, episode.model, episode.rendering, episode.repetition, why
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Difficulty;

    fn episode(task: &str, success: bool, first_check: Option<bool>, error: bool) -> EpisodeRecord {
        EpisodeRecord {
            task: task.into(),
            difficulty: Difficulty::Easy,
            model: "m".into(),
            rendering: Rendering::Compact,
            repetition: 0,
            success,
            failure: (!success).then(|| "wrong".into()),
            error: error.then(|| "429".into()),
            first_check,
            iterations: 4,
            scripts: 2,
            refused: 1,
            tests_written: usize::from(success),
            gate_refusals: 1,
            runs: 1,
            tool_calls: BTreeMap::new(),
            usage: Usage {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
            },
            stop: None,
            turn_limit: false,
            wall_ms: 0,
            virtual_end: String::new(),
            delivered: vec![],
        }
    }

    #[test]
    fn rates_leave_errors_out() {
        let episodes = vec![
            episode("a", true, Some(true), false),
            episode("b", false, Some(false), false),
            episode("c", false, None, false),
            episode("d", false, Some(true), true),
        ];
        let summaries = summarize(&episodes);
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.episodes, 4);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.success_rate, Some(1.0 / 3.0));
        assert_eq!(summary.first_check_pass_rate, Some(0.5));
        assert_eq!(summary.mean_iterations, Some(4.0));
        assert_eq!(summary.tests_written_rate, Some(1.0 / 3.0));
        assert_eq!(summary.mean_gate_refusals, Some(1.0));
        assert_eq!(summary.usage.input, 300);
        let report = Report {
            started_at: "2026-11-12T00:00:00Z".into(),
            models: vec!["m".into()],
            renderings: vec![Rendering::Compact],
            tasks: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            repeat: 1,
            summaries,
            episodes: vec![],
        };
        let text = markdown(&report, &episodes);
        assert!(
            text.contains("| m | compact | 50% | 33% | 4.0 | 2.0 | 33% | 1.0 | 300 | 150 | 1 |"),
            "{text}"
        );
        assert!(text.contains("| a | easy | 1/1 |"));
        assert!(text.contains("- d · m (compact) #0: error: 429"));
    }
}
