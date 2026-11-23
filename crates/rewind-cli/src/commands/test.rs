//! `rewind test [script.ts] [--tests file] [--workflow name] [--toolset id|file]`: runs tests in
//! mock mode, in virtual time (`archi/05-testing-and-observability.md`, "Running tests").
//!
//! A tests file is a JSON array of tests, `script.tests.json` next to the script by default:
//!
//! ```json
//! [{
//!   "name": "closes the stale issues once approved",
//!   "input": { "repo": "acme/api" },
//!   "world": {
//!     "tools": { "github.close_issue": { "returns": {} } },
//!     "mocks": "triage.mocks.ts",
//!     "cassette": "cassettes/triage.json"
//!   },
//!   "signals": [{ "name": "approval", "payload": { "close": true } }],
//!   "expect": { "status": "completed", "calls": [{ "tool": "github.close_issue", "times": 3 }] }
//! }]
//! ```
//!
//! `world.cassette` given as a string, and `world.mocks` as the name of a `.ts` or `.js` file,
//! are read from files relative to the tests file. The tests run in this process, the tools'
//! static responses of `rewind.tools.json` tried last, against the toolset file `--toolset`
//! names, or else the tools the tests answer, as host tools. They run on the server (embedded
//! or remote, `POST /v1/test`) when they need what it holds: a workflow's suite (`--workflow`)
//! or a toolset registered there (`--toolset <id>`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rewind_api::scripts::TestRequest;
use rewind_api::testing::{Test as ApiTest, TestInput, TestResult, TestRun};
use rewind_core::{Script, Test};
use rewind_engine::StartError;
use rewind_engine::testing::{Subject, TestError, TestOptions, run_tests};
use rewind_runtime::{SandboxLimits, TsMockLoader, V8Executor};
use serde_json::Value;

use super::Server;
use crate::args::TestArgs;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ToolsetRef};
use crate::output::Console;
use crate::toolset;

/// The static responses read next to the tests.
pub(crate) const TOOLS_FILE: &str = "rewind.tools.json";

/// The tests file next to a script: `triage.ts` → `triage.tests.json`.
fn tests_file_of(script: &Path) -> PathBuf {
    script.with_extension("tests.json")
}

pub(super) async fn test(
    server: &Server<'_>,
    args: TestArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    if args.script.is_none() && args.workflow.is_none() {
        return Err(CliError::usage(
            "give the script to test, or --workflow to run a workflow's suite",
        ));
    }
    let tests_file = match (&args.tests, &args.script) {
        (Some(path), _) => Some(path.clone()),
        (None, Some(script)) => Some(tests_file_of(script)).filter(|path| path.exists()),
        (None, None) => None,
    };
    let tests = match &tests_file {
        Some(path) => read_tests(path)?,
        None => Vec::new(),
    };
    if tests.is_empty() && args.workflow.is_none() {
        return Err(CliError::usage(format!(
            "no test to run: write them in {}, or give --tests",
            args.script
                .as_deref()
                .map(tests_file_of)
                .unwrap_or_default()
                .display()
        )));
    }
    let source = args.script.as_deref().map(input::read_text).transpose()?;
    let toolset = args.toolset.as_deref().map(ToolsetRef::parse).transpose()?;
    let run = match toolset {
        Some(ToolsetRef::Id(id)) => remote(server, source, Some(id), tests, args.workflow).await?,
        Some(ToolsetRef::File(path)) if args.workflow.is_some() => {
            let client = server.client().await?;
            let registered = client.create_toolset(&input::read_json(&path)?).await?;
            remote(server, source, Some(registered.id), tests, args.workflow).await?
        }
        None if args.workflow.is_some() => {
            remote(server, source, None, tests, args.workflow).await?
        }
        toolset => {
            let base = tests_file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_owned)
                .unwrap_or_default();
            let statics = match &args.tools {
                Some(path) => Some(input::read_json::<Value>(path)?),
                None => {
                    let path = base.join(TOOLS_FILE);
                    path.exists()
                        .then(|| input::read_json::<Value>(&path))
                        .transpose()?
                }
            };
            let file = match toolset {
                Some(ToolsetRef::File(path)) => Some(path),
                _ => None,
            };
            local(source.unwrap_or_default(), file.as_deref(), tests, statics).await?
        }
    };
    if console.json() {
        console.print_json(&run)?;
    } else {
        print(&run, console);
    }
    Ok(if run.passed {
        Exit::Success
    } else {
        Exit::Failed
    })
}

/// The tests of a file, with the cassettes and mock handlers it names read in.
fn read_tests(path: &Path) -> Result<Vec<ApiTest>, CliError> {
    let base = path.parent().map(Path::to_owned).unwrap_or_default();
    let mut tests: Vec<Value> = match input::read_json::<Value>(path)? {
        Value::Array(tests) => tests,
        _ => {
            return Err(CliError::usage(format!(
                "{}: a tests file is a JSON array of tests",
                path.display()
            )));
        }
    };
    for test in &mut tests {
        let Some(world) = test.get_mut("world").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(Value::String(file)) = world.get("cassette") {
            let cassette: Value = input::read_json(&base.join(file))?;
            world.insert("cassette".into(), cassette);
        }
        if let Some(Value::String(mocks)) = world.get("mocks")
            && !mocks.contains('\n')
            && (mocks.ends_with(".ts") || mocks.ends_with(".js"))
        {
            let source = input::read_text(&base.join(mocks))?;
            world.insert("mocks".into(), Value::String(source));
        }
    }
    serde_json::from_value(Value::Array(tests))
        .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))
}

/// On the server: `POST /v1/test`.
async fn remote(
    server: &Server<'_>,
    script: Option<String>,
    toolset: Option<String>,
    tests: Vec<ApiTest>,
    workflow: Option<String>,
) -> Result<TestRun, CliError> {
    let client = server.client().await?;
    Ok(client
        .test(&TestRequest {
            script,
            toolset,
            tests: tests
                .into_iter()
                .map(|test| TestInput::Inline(Box::new(test)))
                .collect(),
            workflow,
        })
        .await?)
}

/// In this process.
async fn local(
    source: String,
    toolset_file: Option<&Path>,
    tests: Vec<ApiTest>,
    statics: Option<Value>,
) -> Result<TestRun, CliError> {
    let tests: Vec<Test> = tests
        .into_iter()
        .map(|test| {
            test.into_core(|cassette| {
                let mut cassette = cassette.clone();
                if let Value::Object(fields) = &mut cassette
                    && !fields.contains_key("v")
                {
                    fields.insert("v".into(), Value::from(rewind_format::VERSION));
                }
                rewind_format::decode_cassette(&cassette.to_string())
                    .map_err(|error| error.to_string())
            })
            .map_err(CliError::usage)
        })
        .collect::<Result<_, _>>()?;
    let toolset = match toolset_file {
        Some(path) => toolset::pin_file(path).await?,
        None => {
            let mut answered: BTreeSet<String> = tests
                .iter()
                .flat_map(|test| {
                    test.world
                        .tools
                        .keys()
                        .cloned()
                        .chain(test.world.cassette.iter().flat_map(|cassette| {
                            cassette.calls.iter().map(|call| call.tool.clone())
                        }))
                        .collect::<Vec<_>>()
                })
                .collect();
            if let Some(Value::Object(statics)) = &statics {
                answered.extend(statics.keys().cloned());
            }
            let loader = TsMockLoader::default();
            for test in &tests {
                if let Some(source) = &test.world.mocks {
                    let mocks = rewind_engine::testing::MockLoader::load(
                        &loader,
                        source,
                        rewind_engine::testing::DEFAULT_START,
                    )
                    .await
                    .map_err(|error| match error {
                        rewind_engine::testing::MockLoadError::Invalid(message) => {
                            CliError::usage(format!("test {:?}: the mocks: {message}", test.name))
                        }
                        rewind_engine::testing::MockLoadError::Internal(message) => {
                            CliError::internal(message)
                        }
                    })?;
                    answered.extend(mocks.tools);
                }
            }
            toolset::of_mocks(&answered)?
        }
    };
    let executor = V8Executor::new(SandboxLimits::default())
        .map_err(|error| CliError::internal(error.to_string()))?;
    let script = Script::new(source);
    let reports = run_tests(
        &executor,
        &TsMockLoader::default(),
        &Subject {
            script: &script,
            toolset: &toolset,
            interface: None,
        },
        &tests,
        &TestOptions { ttl: None, statics },
    )
    .await
    .map_err(|error| match error {
        TestError::Start(refused @ StartError::InvalidScript(_)) => CliError::ScriptRefused {
            message: refused.to_string(),
            diagnostics: Vec::new(),
        },
        TestError::Start(
            refused @ (StartError::InvalidTtl { .. }
            | StartError::InputTooLarge { .. }
            | StartError::NulCharacter(_)),
        ) => CliError::usage(format!("the run cannot start: {refused}")),
        TestError::Start(refused) => CliError::internal(format!("the run cannot start: {refused}")),
        TestError::Unavailable(message) => {
            CliError::internal(format!("the tests could not run: {message}"))
        }
        TestError::Mocks(message) => CliError::internal(message),
    })?;
    let results: Vec<TestResult> = reports
        .iter()
        .map(|report| TestResult {
            name: report.name.clone(),
            passed: report.passed,
            problem: report.problem.map(|problem| match problem {
                rewind_engine::testing::TestProblem::Stale => {
                    rewind_api::testing::TestProblem::Stale
                }
                rewind_engine::testing::TestProblem::Incompatible => {
                    rewind_api::testing::TestProblem::Incompatible
                }
                rewind_engine::testing::TestProblem::Failed => {
                    rewind_api::testing::TestProblem::Failed
                }
            }),
            failures: report.failures.clone(),
            status: report.status.map(Into::into),
            output: report.output.clone(),
            error: report.error.as_ref().map(Into::into),
            abort: report.abort.as_ref().map(Into::into),
            calls: report.calls.clone(),
            trace: report.trace.clone(),
        })
        .collect();
    Ok(TestRun {
        passed: results.iter().all(|result| result.passed),
        tests: results,
    })
}

/// One line per test, its failures under it, and the trace of each test that did not pass.
fn print(run: &TestRun, console: &mut Console) {
    for test in &run.tests {
        let verdict = if test.passed { "PASS" } else { "FAIL" };
        let state = match (&test.problem, &test.status) {
            (Some(problem), None) => format!(" ({}: not run)", problem_name(*problem)),
            (_, Some(status)) => format!(" (the run is {status})"),
            (None, None) => String::new(),
        };
        console.line(format!("{verdict} {}{state}", test.name));
        for failure in &test.failures {
            console.line(format!("  - {failure}"));
        }
    }
    let failed: Vec<&TestResult> = run
        .tests
        .iter()
        .filter(|test| !test.passed && !test.trace.is_empty())
        .collect();
    for test in failed {
        console.line("");
        console.line(format!("{}:", test.name));
        console.text(&test.trace);
    }
    let passed = run.tests.iter().filter(|test| test.passed).count();
    console.line("");
    console.line(format!("{passed} of {} tests passed", run.tests.len()));
}

fn problem_name(problem: rewind_api::testing::TestProblem) -> &'static str {
    match problem {
        rewind_api::testing::TestProblem::Stale => "stale",
        rewind_api::testing::TestProblem::Incompatible => "incompatible",
        rewind_api::testing::TestProblem::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tests_file_of_a_script_sits_next_to_it() {
        assert_eq!(
            tests_file_of(Path::new("flows/triage.ts")),
            PathBuf::from("flows/triage.tests.json")
        );
    }

    #[test]
    fn a_tests_file_reads_its_cassettes_and_mocks_from_files() {
        let dir = std::env::temp_dir().join(format!("rewind-cli-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cassette.json"),
            r#"{"v":1,"matching":"by_args","calls":[],"signals":[]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("mocks.ts"), "export default {};\n").unwrap();
        std::fs::write(
            dir.join("t.tests.json"),
            r#"[{"name":"t","world":{"cassette":"cassette.json","mocks":"mocks.ts"}},
               {"name":"inline","world":{"mocks":"export default { a: () => 1 };"}}]"#,
        )
        .unwrap();
        let tests = read_tests(&dir.join("t.tests.json")).unwrap();
        assert_eq!(
            tests[0].world.mocks.as_deref(),
            Some("export default {};\n")
        );
        assert_eq!(
            tests[0].world.cassette.as_ref().unwrap()["matching"],
            "by_args"
        );
        assert_eq!(
            tests[1].world.mocks.as_deref(),
            Some("export default { a: () => 1 };")
        );
        assert!(
            std::fs::write(dir.join("bad.json"), r#"{"name":"t"}"#).is_ok()
                && read_tests(&dir.join("bad.json")).is_err(),
            "a tests file is an array"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
