//! `rewind workflows list|show|create|update|promote|fork|pull|record`: the workflows
//! (`archi/01-product.md`, "Workflows"), through the API; `record`, tests recorded from a
//! workflow's past runs (`archi/05-testing-and-observability.md`, "Tests recorded from runs").

use std::collections::BTreeMap;
use std::path::Path;

use rewind_api::Client;
use rewind_api::testing::TestInput;
use rewind_api::workflows::{
    ChangeTests, CreateVersion, CreateWorkflow, PatchWorkflow, RecordTests, Workflow, WorkflowQuery,
};
use serde::Deserialize;
use serde_json::Value;

use super::Server;
use super::test::read_tests;
use crate::args::WorkflowsCommand;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ActionSetRef};
use crate::interrupt;
use crate::output::Console;
use crate::render::{self, short_id};

/// Hex digits a digest shows in a table.
const DIGEST_CHARS: usize = 12;

/// An interface file: the schemas of a workflow's input, output and signals.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Interface {
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    signals: Option<BTreeMap<String, Value>>,
}

pub(super) async fn workflows(
    server: &Server<'_>,
    command: WorkflowsCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    match command {
        WorkflowsCommand::List {
            scope,
            code,
            limit,
            after,
        } => {
            let client = server.client().await?;
            let page = client
                .list_workflows(&WorkflowQuery {
                    scope: scope.map(Into::into),
                    code,
                    after,
                    limit,
                    ..WorkflowQuery::default()
                })
                .await?;
            if console.json() {
                console.print_json(&page)?;
                return Ok(Exit::Success);
            }
            if page.workflows.is_empty() {
                console.line("no workflow");
                return Ok(Exit::Success);
            }
            let mut rows = vec![[
                "ID".to_owned(),
                "NAME".to_owned(),
                "SCOPE".to_owned(),
                "VERSION".to_owned(),
                "RUNS".to_owned(),
                "FAILED".to_owned(),
                "LAST RUN".to_owned(),
            ]];
            for workflow in &page.workflows {
                let runs = &workflow.runs;
                rows.push([
                    workflow.id.clone(),
                    workflow.name.clone().unwrap_or_default(),
                    scope_of(workflow.namespace.as_ref()),
                    format!("v{}", workflow.latest.version),
                    (runs.active + runs.completed + runs.failed + runs.cancelled + runs.aborted)
                        .to_string(),
                    runs.failed.to_string(),
                    workflow
                        .last_run_at
                        .map(|at| at.to_string())
                        .unwrap_or_default(),
                ]);
            }
            console.text(&render::table(&rows));
            if let Some(next) = &page.next {
                console.note(format!("more: rewind workflows list --after {next}"));
            }
        }
        WorkflowsCommand::Show { workflow, code } => {
            let client = server.client().await?;
            let shown = client.get_workflow(&workflow).await?;
            if console.json() {
                console.print_json(&shown)?;
                return Ok(Exit::Success);
            }
            show(&shown, code, console);
        }
        WorkflowsCommand::Create {
            code,
            name,
            description,
            action_set,
            interface,
            tests,
            secrets,
        } => {
            let source = input::read_text(&code)?;
            let interface = interface
                .as_deref()
                .map(read_interface)
                .transpose()?
                .unwrap_or_default();
            let tests = tests_of(tests.as_deref())?;
            let secrets = input::secrets(&secrets)?;
            let client = server.client().await?;
            let action_set = match action_set {
                Some(action_set) => registered(&client, &action_set).await?,
                None => client.create_action_set(&Default::default()).await?.id,
            };
            interrupt::before_change("the creation of the workflow")?;
            let created = client
                .create_workflow(&CreateWorkflow {
                    name,
                    description,
                    action_set,
                    code: source,
                    input: interface.input,
                    output: interface.output,
                    signals: interface.signals,
                    secrets,
                    tests,
                })
                .await?;
            written(&created, "created", console)?;
        }
        WorkflowsCommand::Update {
            workflow,
            name,
            description,
            code,
            interface,
            action_set,
            tests,
            secrets,
        } => {
            let source = code.as_deref().map(input::read_text).transpose()?;
            let interface = interface.as_deref().map(read_interface).transpose()?;
            let tests = tests_of(tests.as_deref())?;
            let secrets = (!secrets.is_empty())
                .then(|| input::secrets(&secrets))
                .transpose()?;
            let named = name.is_some() || description.is_some();
            let versioned = source.is_some()
                || interface.is_some()
                || action_set.is_some()
                || secrets.is_some();
            if !named && !versioned && tests.is_empty() {
                return Err(CliError::usage(
                    "nothing to update: give --name, --description, --code, --interface, \
                     --action-set, --secret or --tests",
                ));
            }
            let client = server.client().await?;
            let action_set = match action_set {
                Some(action_set) => Some(registered(&client, &action_set).await?),
                None => None,
            };
            interrupt::before_change("the update of the workflow")?;
            // A name given first: the version and the tests then name the workflow by its id.
            let mut reference = workflow;
            let mut updated = None;
            if named {
                let patch = PatchWorkflow {
                    name: name.map(|name| Some(name).filter(|name| !name.is_empty())),
                    description: description
                        .map(|description| Some(description).filter(|text| !text.is_empty())),
                };
                let patched = client.update_workflow(&reference, &patch).await?;
                reference.clone_from(&patched.id);
                updated = Some(patched);
            }
            if versioned {
                let interface = interface.unwrap_or_default();
                updated = Some(
                    client
                        .create_version(
                            &reference,
                            &CreateVersion {
                                code: source,
                                action_set,
                                input: interface.input,
                                output: interface.output,
                                signals: interface.signals,
                                secrets,
                                tests,
                                replace: Vec::new(),
                                retire: Vec::new(),
                            },
                        )
                        .await?,
                );
            } else if !tests.is_empty() {
                updated = Some(
                    client
                        .change_tests(
                            &reference,
                            &ChangeTests {
                                tests,
                                replace: Vec::new(),
                                retire: Vec::new(),
                            },
                        )
                        .await?,
                );
            }
            if let Some(updated) = updated {
                written(&updated, "updated", console)?;
            }
        }
        WorkflowsCommand::Promote { workflow } => {
            let client = server.client().await?;
            interrupt::before_change("the promotion of the workflow")?;
            let promoted = client.promote_workflow(&workflow).await?;
            written(&promoted, "promoted", console)?;
        }
        WorkflowsCommand::Fork { workflow } => {
            if server.namespace().is_none() {
                return Err(CliError::usage(
                    "--namespace (REWIND_NAMESPACE) names the namespace the fork goes into",
                ));
            }
            let client = server.client().await?;
            interrupt::before_change("the fork of the workflow")?;
            let forked = client.fork_workflow(&workflow).await?;
            written(&forked, "forked", console)?;
        }
        WorkflowsCommand::Pull { workflow } => {
            let client = server.client().await?;
            interrupt::before_change("the pull of the workflow")?;
            let pulled = client.pull_workflow(&workflow).await?;
            written(&pulled, "pulled", console)?;
        }
        WorkflowsCommand::Record {
            name,
            query,
            runs,
            limit,
        } => {
            let client = server.unbound_client().await?;
            let recorded = client
                .record_tests(&name, &RecordTests { query, runs, limit })
                .await?;
            if console.json() {
                console.print_json(&recorded)?;
                return Ok(Exit::Success);
            }
            if recorded.tests.is_empty() {
                console.line(format!("no run of {name} to record"));
            }
            for test in &recorded.tests {
                console.line(format!("{}  {}", test.digest, test.name));
                console.line(format!("  {}", test.path));
            }
            for skipped in &recorded.skipped {
                console.note(format!("skipped run {}: {}", skipped.run, skipped.reason));
            }
        }
    }
    Ok(Exit::Success)
}

/// The id of an action set given by its id, or registered from its file.
async fn registered(client: &Client, action_set: &str) -> Result<String, CliError> {
    Ok(match ActionSetRef::parse(action_set)? {
        ActionSetRef::Id(id) => id,
        // Content-addressed: registering the same file again gives the same id.
        ActionSetRef::File(path) => {
            client
                .create_action_set(&input::read_json(&path)?)
                .await?
                .id
        }
    })
}

fn read_interface(path: &Path) -> Result<Interface, CliError> {
    input::read_json(path)
}

/// The tests of a tests file, sent inline.
fn tests_of(path: Option<&Path>) -> Result<Vec<TestInput>, CliError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    Ok(read_tests(path)?
        .into_iter()
        .map(|test| TestInput::Inline(Box::new(test)))
        .collect())
}

fn scope_of(namespace: Option<&rewind_api::namespaces::NamespaceName>) -> String {
    namespace.map_or_else(|| "organization".to_owned(), ToString::to_string)
}

fn label(workflow: &Workflow) -> String {
    workflow
        .name
        .clone()
        .unwrap_or_else(|| short_id(&workflow.id))
}

fn written(workflow: &Workflow, what: &str, console: &mut Console) -> Result<(), CliError> {
    if console.json() {
        console.print_json(workflow)?;
        return Ok(());
    }
    console.line(format!(
        "{what} {} ({}) v{} in {}",
        label(workflow),
        workflow.id,
        workflow.version.version,
        scope_of(workflow.namespace.as_ref())
    ));
    Ok(())
}

fn show(workflow: &Workflow, code: bool, console: &mut Console) {
    console.line(format!("workflow {} ({})", label(workflow), workflow.id));
    console.line(format!("scope: {}", scope_of(workflow.namespace.as_ref())));
    if let Some(description) = &workflow.description {
        console.line(format!("description: {description}"));
    }
    if let Some(source) = &workflow.source {
        console.line(format!(
            "fork of {} at v{}",
            source.workflow, source.version
        ));
    }
    let mut rows = vec![[
        "VERSION".to_owned(),
        "DIGEST".to_owned(),
        "CODE".to_owned(),
        "ACTION SET".to_owned(),
        "TESTS".to_owned(),
        "CREATED".to_owned(),
    ]];
    for version in &workflow.versions {
        rows.push([
            format!("v{}", version.version),
            short(&version.digest),
            short(&version.code_digest),
            short(&version.action_set),
            version.tests.len().to_string(),
            version.created_at.to_string(),
        ]);
    }
    console.text(&render::table(&rows));
    if !workflow.suite.is_empty() {
        console.line("suite:");
        for test in &workflow.suite {
            match (&test.retired_at, &test.reason) {
                (None, _) => console.line(format!("  {}  {}", short(&test.digest), test.name)),
                (Some(_), reason) => console.line(format!(
                    "  {}  {} (left: {})",
                    short(&test.digest),
                    test.name,
                    reason.as_deref().unwrap_or("retired")
                )),
            }
        }
    }
    if code {
        console.line(format!("code of v{}:", workflow.version.version));
        console.text(&workflow.code);
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(DIGEST_CHARS).collect()
}
