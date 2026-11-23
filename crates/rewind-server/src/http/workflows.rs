//! Workflows: saving them as versions, listing them, and finding the version a reference
//! names (`archi/01-product.md`, "Workflows").
//!
//! A reference is a name, for the latest version the caller sees, or `name@<version>`, the
//! version's digest or its first 12 hex digits at least, or `name@v<number>`, the version's
//! number among the versions of its workflow.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::common::{Labels, timestamp};
use rewind_api::toolsets::Declarations;
use rewind_api::workflows::{
    CreateWorkflow, RetiredTest, SuiteTest, UpdatedTest, Workflow as ApiWorkflow, WorkflowPage,
    WorkflowQuery, WorkflowVersions,
};
use rewind_core::{Digest, Script, Toolset, WorkflowSchemas};
use rewind_engine::ports::{
    CatalogStore, StoredWorkflow, TenantId, WorkflowDeclarations, WorkflowFilter,
};
use rewind_engine::suites::{SuiteChanges, TestKit, TestRetirement, TestUpdate, UpdateWith};
use rewind_engine::testing::TestOptions;
use rewind_engine::{RegisterError, RegisterWorkflow};
use rewind_runtime::TsMockLoader;

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::http::{testing, toolsets};

/// Workflows a page of `GET /v1/workflows` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// Hex digits a reference to a version gives at least.
const MIN_VERSION_DIGITS: usize = 12;

/// `POST /v1/workflows`: the checks, then the gate: the version's suite runs, and it is saved
/// only if every test passes.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateWorkflow>,
) -> Result<Response, ApiError> {
    caller.require(Permission::ToolsetsWrite)?;
    let toolset = toolsets::parse_digest(&request.toolset)?;
    let secrets = super::runs::attachments(&request.secrets)?;
    let schemas = WorkflowSchemas {
        input: request.input,
        output: request.output,
        signals: request.signals,
    };
    let generated = rewind_codegen::workflow_declarations(&request.name, &schemas);
    let script = Script::new(request.script);
    let parent = match &request.parent {
        Some(reference) => Some(parent_of(&app, &caller, &request.name, reference).await?),
        None => None,
    };
    let mut tests = Vec::with_capacity(request.tests.len());
    for given in request.tests {
        tests.push(testing::input(given)?);
    }
    let mut update = Vec::with_capacity(request.update.len());
    for given in request.update {
        let with = match (given.test, given.expect) {
            (Some(test), None) => UpdateWith::Test(testing::input(test)?),
            (None, Some(expect)) => UpdateWith::Expect(expect.into_core()),
            _ => {
                return Err(ApiError::new(
                    ProblemType::InvalidTest,
                    format!(
                        "the update of {:?} gives `test` or `expect`: one of them",
                        given.name
                    ),
                ));
            }
        };
        update.push(TestUpdate {
            name: given.name,
            reason: given.reason,
            with,
        });
    }
    let suite = SuiteChanges {
        parent,
        tests,
        update,
        retire: request
            .retire
            .into_iter()
            .map(|retire| TestRetirement {
                names: retire.names,
                reason: retire.reason,
            })
            .collect(),
    };
    let before = app
        .store
        .workflow_versions(&caller.tenant, &request.name, &caller.labels)
        .await?;
    let loader = TsMockLoader::default();
    let registered = app
        .edge
        .register_workflow(
            RegisterWorkflow {
                tenant: caller.tenant.clone(),
                name: request.name,
                description: request.description,
                script,
                toolset,
                schemas,
                labels: caller.labels.clone(),
                secrets,
                attach: super::runs::attach_rights(&caller),
                declarations: WorkflowDeclarations {
                    signals: generated.signals,
                    entry: generated.entry,
                },
                suite,
            },
            &app.checker,
            &TestKit {
                loader: &loader,
                format: testing::format(),
                options: TestOptions::default(),
            },
        )
        .await
        .map_err(register_error)?;
    // Saving a version again returns it with 200.
    let existed = before
        .iter()
        .any(|stored| stored.workflow.version() == registered.workflow.version());
    let status = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(view(&registered))).into_response())
}

/// The parent a save names: `name@<version>`, or a version of the same workflow.
async fn parent_of(
    app: &App,
    caller: &Caller,
    name: &str,
    reference: &str,
) -> Result<rewind_core::Digest, ApiError> {
    let reference = if reference.contains('@') {
        reference.to_owned()
    } else {
        format!("{name}@{reference}")
    };
    Ok(resolve(app, &caller.tenant, &caller.labels, &reference)
        .await?
        .workflow
        .version())
}

/// `GET /v1/workflows`.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<WorkflowQuery>,
) -> Result<Json<WorkflowPage>, ApiError> {
    caller.require(Permission::ToolsetsRead)?;
    if let Some(script) = &query.script {
        let script = Digest::from_hex(script).map_err(|_| {
            ApiError::invalid("script: the digest of a script, 64 lowercase hexadecimal digits")
        })?;
        let found = app
            .store
            .workflows_with_script(&caller.tenant, script, &caller.labels)
            .await?;
        return Ok(Json(WorkflowPage {
            workflows: found.iter().map(view).collect(),
            next: None,
        }));
    }
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let found = app
        .store
        .latest_workflows(&WorkflowFilter {
            tenant: caller.tenant.clone(),
            labels: caller.labels.clone(),
            after: query.after,
            limit: Some(limit),
        })
        .await?;
    let next = (found.len() == limit)
        .then(|| found.last())
        .flatten()
        .map(|last| last.workflow.name().to_owned());
    Ok(Json(WorkflowPage {
        workflows: found.iter().map(view).collect(),
        next,
    }))
}

/// `GET /v1/workflows/{name}`.
pub(super) async fn versions(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<WorkflowVersions>, ApiError> {
    caller.require(Permission::ToolsetsRead)?;
    let versions = app
        .store
        .workflow_versions(&caller.tenant, &name, &caller.labels)
        .await?;
    if versions.is_empty() {
        return Err(not_found(&name));
    }
    Ok(Json(WorkflowVersions {
        name,
        versions: versions.iter().map(view).collect(),
    }))
}

/// The version a reference names, among those the caller sees: `name` for the latest,
/// `name@<version>` or `name@v<number>` for one.
pub(crate) async fn resolve(
    app: &App,
    tenant: &TenantId,
    labels: &BTreeMap<String, String>,
    reference: &str,
) -> Result<StoredWorkflow, ApiError> {
    let (name, version) = match reference.split_once('@') {
        Some((name, version)) => (name, Some(version.to_ascii_lowercase())),
        None => (reference, None),
    };
    // A digest's hex digits hold no `v`: `v12` is a number.
    if let Some(number) = version
        .as_deref()
        .and_then(|version| version.strip_prefix('v'))
    {
        let number: u32 = number.parse().map_err(|_| {
            ApiError::invalid(format!(
                "workflow {reference:?}: name a version by its number, as in triage@v12, or \
                 by its digest or its first {MIN_VERSION_DIGITS} hex digits at least"
            ))
        })?;
        return app
            .store
            .workflow_number(tenant, name, number)
            .await?
            .filter(|stored| stored.carries(labels))
            .ok_or_else(|| not_found(reference));
    }
    if let Some(version) = &version
        && (version.len() < MIN_VERSION_DIGITS
            || version.len() > 64
            || !version.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(ApiError::invalid(format!(
            "workflow {reference:?}: name a version by its digest or its first \
             {MIN_VERSION_DIGITS} hex digits at least, as in triage@9f2c4e1a07b3, or by its \
             number, as in triage@v12"
        )));
    }
    let versions = app.store.workflow_versions(tenant, name, labels).await?;
    let found = match &version {
        None => versions.into_iter().next(),
        Some(prefix) => versions.into_iter().find(|stored| {
            stored
                .workflow
                .version()
                .to_hex()
                .starts_with(prefix.as_str())
        }),
    };
    found.ok_or_else(|| not_found(reference))
}

/// The pinned forms of the workflows a toolset names.
pub(crate) async fn pin(
    app: &App,
    tenant: &TenantId,
    labels: &BTreeMap<String, String>,
    references: &[String],
    toolset: Toolset,
) -> Result<Toolset, ApiError> {
    if references.is_empty() {
        return Ok(toolset);
    }
    let mut pinned: BTreeMap<String, rewind_core::PinnedWorkflow> = toolset
        .workflows()
        .iter()
        .map(|workflow| (workflow.name.clone(), workflow.clone()))
        .collect();
    for reference in references {
        let stored = resolve(app, tenant, labels, reference).await?;
        let workflow = stored.workflow.pinned();
        pinned.insert(workflow.name.clone(), workflow);
    }
    toolset
        .with_workflows(pinned.into_values())
        .map_err(|error| ApiError::new(ProblemType::InvalidToolset, error.to_string()))
}

/// A stored version, as the API shows it.
pub(crate) fn view(stored: &StoredWorkflow) -> ApiWorkflow {
    let workflow = &stored.workflow;
    let pinned = workflow.pinned();
    let declarations = Toolset::empty()
        .with_workflows([pinned])
        .map(|toolset| rewind_codegen::generate(&toolset, &BTreeMap::new()))
        .ok();
    let declaration = declarations.as_ref().and_then(|generated| {
        let name = format!("{}.{}", rewind_core::WORKFLOWS_NAMESPACE, workflow.name());
        generated.tools.iter().find(|tool| tool.name == name)
    });
    ApiWorkflow {
        name: workflow.name().to_owned(),
        version: workflow.version().to_hex(),
        number: stored.number,
        description: workflow.description().map(str::to_owned),
        toolset: workflow.toolset().to_hex(),
        script: workflow.script().to_hex(),
        input: workflow.schemas().input.clone(),
        output: workflow.schemas().output.clone(),
        signals: workflow.schemas().signals.clone(),
        labels: Labels::from(stored.labels.clone()),
        secrets: workflow
            .secrets()
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
            .collect(),
        created_at: timestamp(stored.created_at),
        declarations: Declarations {
            dts: declaration
                .map(|declaration| declaration.typescript.clone())
                .unwrap_or_default(),
            prompt: declaration
                .map(|declaration| declaration.prompt.clone())
                .unwrap_or_default(),
        },
        parent: workflow.suite().parent.map(|parent| parent.to_hex()),
        tests: workflow
            .suite()
            .tests
            .iter()
            .map(|test| SuiteTest {
                name: test.name.clone(),
                digest: test.test.to_hex(),
            })
            .collect(),
        retired: workflow
            .suite()
            .retired
            .iter()
            .map(|retired| RetiredTest {
                name: retired.name.clone(),
                digest: retired.test.to_hex(),
                reason: retired.reason.clone(),
            })
            .collect(),
        updated: workflow
            .suite()
            .updated
            .iter()
            .map(|updated| UpdatedTest {
                name: updated.name.clone(),
                from: updated.from.to_hex(),
                to: updated.to.to_hex(),
                reason: updated.reason.clone(),
            })
            .collect(),
    }
}

fn not_found(reference: &str) -> ApiError {
    ApiError::not_found(format!(
        "no workflow {reference} for this API key: save it with POST /v1/workflows, or list them \
         with GET /v1/workflows"
    ))
}

/// A workflow refused at registration, as a problem.
fn register_error(error: RegisterError) -> ApiError {
    match error {
        RegisterError::Invalid(_) | RegisterError::Schema { .. } => {
            ApiError::new(ProblemType::InvalidWorkflow, error.to_string())
        }
        RegisterError::SecretRequired(ref refused) => {
            ApiError::secret_required(error.to_string(), refused)
        }
        RegisterError::Secret(error) => error.into(),
        RegisterError::ToolsetNotFound(digest) => ApiError::not_found(format!(
            "no toolset {digest} for this tenant: register it with POST /v1/toolsets"
        )),
        RegisterError::InvalidScript(invalid) => {
            rewind_engine::StartError::InvalidScript(invalid).into()
        }
        RegisterError::TypeErrors(diagnostics) => {
            rewind_engine::StartError::TypeErrors(diagnostics).into()
        }
        RegisterError::Checker(error) => error.into(),
        RegisterError::NulCharacter(error) => error.into(),
        RegisterError::Store(error) => error.into(),
        RegisterError::Blob(error) => error.into(),
        RegisterError::Suite(error) => testing::suite_error(error.into()),
        RegisterError::CorruptTest(message) => ApiError::internal(message),
        RegisterError::TestsFailed(reports) => testing::tests_failed("the suite", &reports),
        RegisterError::Tests(error) => testing::test_error(error),
    }
}
