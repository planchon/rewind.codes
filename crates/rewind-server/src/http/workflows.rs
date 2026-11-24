//! Workflows (`archi/01-product.md`, "Workflows"; `archi/07-code-architecture.md`, "HTTP API
//! `/v1`"): created, listed, read, named, given versions and tests, promoted, forked and pulled.
//!
//! A reference is a workflow's id (`wf_…`) or its name, which resolves in the caller's namespace
//! first, then among the organization's workflows; `@v12` (its number) or `@<digest>` (its
//! first 12 hex digits at least) names one version, else the latest the caller sees.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::action_sets::Declarations;
use rewind_api::common::{Labels, timestamp};
use rewind_api::workflows::{
    ChangeTests as ApiChangeTests, CodeComment, CreateVersion, CreateWorkflow as ApiCreateWorkflow,
    PatchWorkflow, ReplacedTest as ApiReplacedTest, RetiredTest as ApiRetiredTest, RunCounts,
    SuiteEntry, SuiteTest as ApiSuiteTest, Workflow as ApiWorkflow, WorkflowPage, WorkflowQuery,
    WorkflowScope as ApiScope, WorkflowSource as ApiSource, WorkflowSummary,
    WorkflowVersion as ApiVersion, interface,
};
use rewind_core::{ActionSet, Code, PinnedWorkflow, WorkflowSchemas, code_doc};
use rewind_engine::edge::{ResolveError, WorkflowKit};
use rewind_engine::ports::{
    CatalogStore, Labels as EngineLabels, ScopeFilter, StoredVersion, StoredWorkflow, TenantId,
    WorkflowDeclarations, WorkflowPatch, WorkflowScope, WorkflowSource,
};
use rewind_engine::suites::{ReplaceWith, SuiteChanges, TestKit, TestReplacement, TestRetirement};
use rewind_engine::testing::TestOptions;
use rewind_engine::{
    ChangeTests, CreateWorkflow, ForkWorkflow, MakeVersion, PullWorkflow, WorkflowError,
    WorkflowMaker, WrittenWorkflow,
};
use rewind_runtime::TsHandlerLoader;

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::http::{action_sets, stats, testing};

/// Workflows a page of `GET /v1/workflows` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// The window the counts of `GET /v1/workflows` cover by default: the last 30 days.
const DEFAULT_WINDOW_MS: u64 = 30 * 24 * 3_600_000;

/// The workflow a reference names, without a version, among those the caller sees.
pub(crate) async fn find(
    app: &App,
    caller: &Caller,
    reference: &str,
) -> Result<StoredWorkflow, ApiError> {
    app.edge
        .find_workflow(
            &caller.tenant,
            caller.namespace.as_ref(),
            &caller.labels,
            reference,
        )
        .await
        .map_err(resolve_error)
}

/// The workflow and the version a reference names, among those the caller sees.
pub(crate) async fn resolve(
    app: &App,
    caller: &Caller,
    reference: &str,
) -> Result<(StoredWorkflow, StoredVersion), ApiError> {
    app.edge
        .resolve_workflow(
            &caller.tenant,
            caller.namespace.as_ref(),
            &caller.labels,
            reference,
        )
        .await
        .map_err(resolve_error)
}

fn resolve_error(error: ResolveError) -> ApiError {
    match error {
        ResolveError::Invalid(message) => ApiError::invalid(message),
        ResolveError::NotFound(message) => ApiError::not_found(message),
        ResolveError::Store(error) => error.into(),
    }
}

/// Who writes: the caller's labels, what it may use, and its principal.
fn maker(caller: &Caller) -> WorkflowMaker {
    WorkflowMaker {
        labels: caller.labels.clone(),
        secret_rights: super::runs::secret_rights(caller),
        principal: caller.writer(),
    }
}

/// The declarations `rewind-codegen` generates from an interface.
fn declare(name: &str, schemas: &WorkflowSchemas) -> WorkflowDeclarations {
    let generated = rewind_codegen::workflow_declarations(name, schemas);
    WorkflowDeclarations {
        signals: generated.signals,
        entry: generated.entry,
    }
}

fn kit<'a>(
    app: &'a App,
    loader: &'a TsHandlerLoader,
) -> WorkflowKit<'a, crate::wiring::checker::Checker, TsHandlerLoader> {
    WorkflowKit {
        checker: &app.checker,
        tests: TestKit {
            loader,
            format: testing::format(),
            options: TestOptions::default(),
        },
        declare,
    }
}

/// The interface a request gives, if it gives one.
fn given_interface(
    input: &Option<serde_json::Value>,
    output: &Option<serde_json::Value>,
    signals: &Option<BTreeMap<String, serde_json::Value>>,
) -> Option<WorkflowSchemas> {
    interface(input, output, signals).map(|(input, output, signals)| WorkflowSchemas {
        input,
        output,
        signals,
    })
}

/// The changes of a suite a request gives, as the engine takes them.
fn suite_changes(
    tests: Vec<rewind_api::testing::TestInput>,
    replace: Vec<rewind_api::workflows::TestReplacement>,
    retire: Vec<rewind_api::workflows::TestRetirement>,
) -> Result<SuiteChanges, ApiError> {
    let mut given = Vec::with_capacity(tests.len());
    for test in tests {
        given.push(testing::input(test)?);
    }
    let mut replacements = Vec::with_capacity(replace.len());
    for replacement in replace {
        let with = match (replacement.new, replacement.expect) {
            (Some(test), None) => ReplaceWith::Test(testing::input(test)?),
            (None, Some(expect)) => ReplaceWith::Expect(expect.into_core()),
            _ => {
                return Err(ApiError::new(
                    ProblemType::InvalidTest,
                    format!(
                        "the replacement of {:?} gives `new` or `expect`: one of them",
                        replacement.name
                    ),
                ));
            }
        };
        replacements.push(TestReplacement {
            name: replacement.name,
            reason: replacement.reason,
            with,
        });
    }
    Ok(SuiteChanges {
        tests: given,
        replace: replacements,
        retire: retire
            .into_iter()
            .map(|retirement| TestRetirement {
                names: retirement.names,
                reason: retirement.reason,
            })
            .collect(),
    })
}

/// 201 for a version made, 200 for one found.
async fn written(
    app: &App,
    caller: &Caller,
    written: WrittenWorkflow,
) -> Result<Response, ApiError> {
    let status = if written.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let view = view(app, caller, &written.workflow, &written.version).await?;
    Ok((status, Json(view)).into_response())
}

/// `POST /v1/workflows`: in the namespace `Rewind-Namespace` names, else the organization's.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<ApiCreateWorkflow>,
) -> Result<Response, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let action_set = action_sets::parse_digest(&request.action_set)?;
    let secrets = super::runs::server_secrets(&request.secrets)?;
    let interface = given_interface(&request.input, &request.output, &request.signals);
    let suite = suite_changes(request.tests, Vec::new(), Vec::new())?;
    let loader = TsHandlerLoader::default();
    let made = app
        .edge
        .create_workflow(
            CreateWorkflow {
                tenant: caller.tenant.clone(),
                scope: WorkflowScope::of(caller.namespace.as_ref()),
                name: request.name,
                description: request.description,
                code: Code::new(request.code),
                action_set,
                interface,
                secrets,
                tests: suite.tests,
                maker: maker(&caller),
            },
            &kit(&app, &loader),
        )
        .await
        .map_err(workflow_error)?;
    written(&app, &caller, made).await
}

/// `GET /v1/workflows`: the workflows the caller sees, by id, each with its latest version and
/// what its runs did in the window.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<WorkflowQuery>,
) -> Result<Json<WorkflowPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let code = query
        .code
        .as_deref()
        .map(|code| {
            rewind_core::Digest::from_hex(code).map_err(|_| {
                ApiError::invalid("code: the digest of a code, 64 lowercase hexadecimal digits")
            })
        })
        .transpose()?;
    let after = query
        .after
        .as_deref()
        .map(|after| {
            after
                .parse()
                .map_err(|_| ApiError::invalid(format!("after {after:?}: a workflow's id")))
        })
        .transpose()?;
    let scope = match (query.scope, &caller.namespace) {
        (Some(ApiScope::Organization), _) => ScopeFilter::Organization,
        (Some(ApiScope::Namespace), Some(namespace)) => ScopeFilter::Namespace(namespace.clone()),
        (None, Some(namespace)) => ScopeFilter::Visible(namespace.clone()),
        (_, None) => ScopeFilter::All,
    };
    let only_namespaces = query.scope == Some(ApiScope::Namespace) && caller.namespace.is_none();
    let found = app
        .catalog()
        .list_workflows(&rewind_engine::ports::WorkflowFilter {
            tenant: caller.tenant.clone(),
            scope,
            code,
            labels: caller.labels.clone(),
            after,
            limit,
        })
        .await?;
    let next = (found.len() == limit)
        .then(|| found.last())
        .flatten()
        .map(|last| last.id.to_string());
    // What their runs did in the window, from the counters; none for a caller limited to
    // labels, which counters do not know.
    let counted = if caller.labels.is_empty() {
        let scope = stats::scope(&caller, query.q.as_deref())?;
        let since = query.since.or_else(|| {
            let now = app.now();
            Some(timestamp(rewind_core::Timestamp(
                now.0.saturating_sub(DEFAULT_WINDOW_MS),
            )))
        });
        let (window, _) = stats::window(since, query.until, None, app.now())?;
        let (counters, _) = app
            .edge
            .statistics(&mut app.gateway(), &scope, window)
            .await?;
        counters.workflows
    } else {
        BTreeMap::new()
    };
    let mut workflows = Vec::with_capacity(found.len());
    for workflow in found {
        if only_namespaces && workflow.scope == WorkflowScope::Organization {
            continue;
        }
        let Some(latest) = app
            .catalog()
            .versions(&caller.tenant, &workflow.id, &caller.labels)
            .await?
            .into_iter()
            .next()
        else {
            continue;
        };
        let comment = comment(&app, &caller.tenant, &latest).await?;
        let counts = counted.get(&workflow.id);
        workflows.push(WorkflowSummary {
            id: workflow.id.to_string(),
            short_id: workflow.id.short(),
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            namespace: workflow.scope.namespace().map(crate::views::namespace),
            source: workflow.source.as_ref().map(source),
            created_at: timestamp(workflow.created_at),
            promoted_at: workflow.promoted_at.map(timestamp),
            latest: version_view(&workflow, &latest, comment, false),
            runs: counts.map_or_else(RunCounts::default, |counts| stats::run_counts(&counts.runs)),
            actions: counts
                .map(|counts| counts.actions.iter().cloned().collect())
                .unwrap_or_default(),
            first_run_at: counts
                .and_then(|counts| counts.first_started_at)
                .map(timestamp),
            last_run_at: counts
                .and_then(|counts| counts.last_started_at)
                .map(timestamp),
        });
    }
    Ok(Json(WorkflowPage { workflows, next }))
}

/// `GET /v1/workflows/{id}`.
pub(super) async fn get(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
) -> Result<Json<ApiWorkflow>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let (workflow, version) = resolve(&app, &caller, &reference).await?;
    Ok(Json(view(&app, &caller, &workflow, &version).await?))
}

/// `PATCH /v1/workflows/{id}`: its name and description, without a version.
pub(super) async fn update(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
    Body(request): Body<PatchWorkflow>,
) -> Result<Json<ApiWorkflow>, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let workflow = find(&app, &caller, &reference).await?;
    let patch = WorkflowPatch {
        name: request.name,
        description: request.description,
    };
    let updated = app
        .edge
        .update_workflow(&caller.tenant, &workflow, &patch)
        .await
        .map_err(workflow_error)?;
    let latest = latest(&app, &caller, &updated).await?;
    Ok(Json(view(&app, &caller, &updated, &latest).await?))
}

/// `POST /v1/workflows/{id}/versions`: its next version, through its gate.
pub(super) async fn version(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
    Body(request): Body<CreateVersion>,
) -> Result<Response, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let workflow = find(&app, &caller, &reference).await?;
    let action_set = request
        .action_set
        .as_deref()
        .map(action_sets::parse_digest)
        .transpose()?;
    let secrets = request
        .secrets
        .as_ref()
        .map(super::runs::server_secrets)
        .transpose()?;
    let interface = given_interface(&request.input, &request.output, &request.signals);
    let suite = suite_changes(request.tests, request.replace, request.retire)?;
    let loader = TsHandlerLoader::default();
    let made = app
        .edge
        .make_version(
            MakeVersion {
                tenant: caller.tenant.clone(),
                workflow,
                code: request.code.map(Code::new),
                action_set,
                interface,
                secrets,
                suite,
                maker: maker(&caller),
            },
            &kit(&app, &loader),
        )
        .await
        .map_err(workflow_error)?;
    written(&app, &caller, made).await
}

/// `POST /v1/workflows/{id}/tests`: tests added to its suite, replaced or retired.
pub(super) async fn tests(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
    Body(request): Body<ApiChangeTests>,
) -> Result<Json<ApiWorkflow>, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let workflow = find(&app, &caller, &reference).await?;
    let suite = suite_changes(request.tests, request.replace, request.retire)?;
    let loader = TsHandlerLoader::default();
    let (workflow, _) = app
        .edge
        .change_tests(
            ChangeTests {
                tenant: caller.tenant.clone(),
                workflow,
                suite,
                maker: maker(&caller),
            },
            &kit(&app, &loader),
        )
        .await
        .map_err(workflow_error)?;
    let latest = latest(&app, &caller, &workflow).await?;
    Ok(Json(view(&app, &caller, &workflow, &latest).await?))
}

/// `POST /v1/workflows/{id}/promote`: a namespace's workflow becomes the organization's.
pub(super) async fn promote(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
) -> Result<Json<ApiWorkflow>, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let workflow = find(&app, &caller, &reference).await?;
    let promoted = app
        .edge
        .promote_workflow(&caller.tenant, &workflow)
        .await
        .map_err(workflow_error)?;
    let latest = latest(&app, &caller, &promoted).await?;
    Ok(Json(view(&app, &caller, &promoted, &latest).await?))
}

/// `POST /v1/workflows/{id}/fork`: an organization's workflow forked into the namespace
/// `Rewind-Namespace` names.
pub(super) async fn fork(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
) -> Result<Response, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let namespace = caller.namespace_required("fork a workflow")?;
    // The organization's workflow: a name resolves among the organization's, never the
    // namespace's own.
    let (source, version) = app
        .edge
        .resolve_workflow(&caller.tenant, None, &caller.labels, &reference)
        .await
        .map_err(resolve_error)?;
    let named_version = reference.contains('@');
    let forked = app
        .edge
        .fork_workflow(ForkWorkflow {
            tenant: caller.tenant.clone(),
            source,
            version: named_version.then_some(version),
            namespace,
            maker: maker(&caller),
        })
        .await
        .map_err(workflow_error)?;
    written(&app, &caller, forked).await
}

/// `POST /v1/workflows/{id}/pull`: a fork takes its source's latest version, through its suite.
pub(super) async fn pull(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(reference): Path<String>,
) -> Result<Response, ApiError> {
    caller.require(Permission::WorkflowsWrite)?;
    let fork = find(&app, &caller, &reference).await?;
    let loader = TsHandlerLoader::default();
    let pulled = app
        .edge
        .pull_workflow(
            PullWorkflow {
                tenant: caller.tenant.clone(),
                fork,
                maker: maker(&caller),
            },
            &kit(&app, &loader),
        )
        .await
        .map_err(workflow_error)?;
    written(&app, &caller, pulled).await
}

/// The latest version of a workflow the caller sees.
async fn latest(
    app: &App,
    caller: &Caller,
    workflow: &StoredWorkflow,
) -> Result<StoredVersion, ApiError> {
    app.catalog()
        .versions(&caller.tenant, &workflow.id, &caller.labels)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "the workflow {} has no version this caller sees",
                workflow.reference()
            ))
        })
}

/// The pinned forms of the workflows an action set names: the organization's, each by its name
/// or its id, `@v12` or `@<digest>` for a version, else its latest. A workflow without a name,
/// or a version without an interface, cannot be pinned: `rewind:workflows` exports it by its
/// name and types it by its interface.
pub(crate) async fn pin(
    app: &App,
    tenant: &TenantId,
    labels: &EngineLabels,
    references: &[String],
    action_set: ActionSet,
) -> Result<ActionSet, ApiError> {
    if references.is_empty() {
        return Ok(action_set);
    }
    let mut pinned: BTreeMap<String, PinnedWorkflow> = action_set
        .workflows()
        .iter()
        .map(|workflow| (workflow.name.clone(), workflow.clone()))
        .collect();
    for reference in references {
        let (workflow, version) = app
            .edge
            .resolve_workflow(tenant, None, labels, reference)
            .await
            .map_err(resolve_error)?;
        let refused = |message: String| ApiError::new(ProblemType::InvalidActionSet, message);
        if let WorkflowScope::Namespace(namespace) = &workflow.scope {
            return Err(refused(format!(
                "the workflow {} is the namespace {namespace}'s: an action set pins the \
                 organization's workflows; promote it first (POST /v1/workflows/{}/promote)",
                workflow.reference(),
                workflow.id
            )));
        }
        let Some(name) = workflow.name.clone() else {
            return Err(refused(format!(
                "the workflow {} has no name: `rewind:workflows` exports a workflow by its \
                 name; name it first",
                workflow.id
            )));
        };
        let Some(schemas) = version.document.interface().cloned() else {
            return Err(refused(format!(
                "{name}@v{} has no interface: an action set pins a version whose input, output \
                 and signals are typed; give it an interface first",
                version.version
            )));
        };
        pinned.insert(
            name.clone(),
            PinnedWorkflow {
                name,
                id: workflow.id.clone(),
                version: version.version,
                digest: version.digest,
                description: workflow.description.clone(),
                schemas,
            },
        );
    }
    action_set
        .with_workflows(pinned.into_values())
        .map_err(|error| ApiError::new(ProblemType::InvalidActionSet, error.to_string()))
}

/// The comment at the top of a version's code, if it has one.
async fn comment(
    app: &App,
    tenant: &TenantId,
    version: &StoredVersion,
) -> Result<Option<CodeComment>, ApiError> {
    let Some(code) = app
        .catalog()
        .get_code(tenant, version.document.code())
        .await?
    else {
        return Ok(None);
    };
    Ok(code_doc(code.code.source()).map(|doc| CodeComment {
        title: doc.title,
        description: doc.description,
    }))
}

fn source(source: &WorkflowSource) -> ApiSource {
    ApiSource {
        workflow: source.workflow.to_string(),
        version: source.version,
    }
}

/// A workflow as the API shows it: `shown`, the version it shows with its code, its versions
/// the caller sees and its suite.
pub(crate) async fn view(
    app: &App,
    caller: &Caller,
    workflow: &StoredWorkflow,
    shown: &StoredVersion,
) -> Result<ApiWorkflow, ApiError> {
    let tenant = &caller.tenant;
    let catalog = app.catalog();
    let code = catalog
        .get_code(tenant, shown.document.code())
        .await?
        .ok_or_else(|| {
            ApiError::internal(format!(
                "the code {} of {} is not stored",
                shown.document.code(),
                workflow.id
            ))
        })?
        .code;
    let doc = code_doc(code.source()).map(|doc| CodeComment {
        title: doc.title,
        description: doc.description,
    });
    let versions = catalog
        .versions(tenant, &workflow.id, &caller.labels)
        .await?
        .iter()
        .map(|version| version_view(workflow, version, None, true))
        .collect();
    let suite = catalog
        .suite(tenant, &workflow.id)
        .await?
        .into_iter()
        .map(|member| SuiteEntry {
            name: member.name,
            digest: member.digest.to_hex(),
            added_at: timestamp(member.added_at),
            retired_at: member.retired_at.map(timestamp),
            reason: member.reason,
        })
        .collect();
    Ok(ApiWorkflow {
        id: workflow.id.to_string(),
        short_id: workflow.id.short(),
        name: workflow.name.clone(),
        description: workflow.description.clone(),
        namespace: workflow.scope.namespace().map(crate::views::namespace),
        source: workflow.source.as_ref().map(source),
        labels: Labels::from(workflow.labels.clone()),
        created_at: timestamp(workflow.created_at),
        promoted_at: workflow.promoted_at.map(timestamp),
        version: version_view(workflow, shown, doc, true),
        code: code.source().to_owned(),
        versions,
        suite,
    })
}

/// A version as the API shows it; with `tests`, the suite it passed.
fn version_view(
    workflow: &StoredWorkflow,
    stored: &StoredVersion,
    comment: Option<CodeComment>,
    tests: bool,
) -> ApiVersion {
    let document = &stored.document;
    let schemas = document.interface();
    let declarations = match (&workflow.name, schemas) {
        (Some(name), Some(schemas)) => {
            let pinned = PinnedWorkflow {
                name: name.clone(),
                id: workflow.id.clone(),
                version: stored.version,
                digest: stored.digest,
                description: workflow.description.clone(),
                schemas: schemas.clone(),
            };
            ActionSet::empty()
                .with_workflows([pinned])
                .ok()
                .map(|action_set| rewind_codegen::generate(&action_set, &BTreeMap::new()))
                .and_then(|generated| {
                    let action = format!("{}.{name}", rewind_core::WORKFLOWS_GROUP);
                    generated
                        .actions
                        .iter()
                        .find(|declaration| declaration.name == action)
                        .map(|declaration| Declarations {
                            dts: declaration.typescript.clone(),
                            prompt: declaration.prompt.clone(),
                        })
                })
        }
        _ => None,
    };
    let suite = document.suite();
    ApiVersion {
        version: stored.version,
        digest: stored.digest.to_hex(),
        code_digest: document.code().to_hex(),
        comment,
        action_set: document.action_set().to_hex(),
        input: schemas.map(|schemas| schemas.input.clone()),
        output: schemas.map(|schemas| schemas.output.clone()),
        signals: schemas.map(|schemas| schemas.signals.clone()),
        labels: Labels::from(stored.labels.clone()),
        secrets: document
            .secrets()
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
            .collect(),
        created_at: timestamp(stored.created_at),
        declarations,
        base: suite.base.map(|base| base.to_hex()),
        tests: if tests {
            suite
                .tests
                .iter()
                .map(|test| ApiSuiteTest {
                    name: test.name.clone(),
                    digest: test.test.to_hex(),
                })
                .collect()
        } else {
            Vec::new()
        },
        retired: suite
            .retired
            .iter()
            .filter(|_| tests)
            .map(|retired| ApiRetiredTest {
                name: retired.name.clone(),
                digest: retired.test.to_hex(),
                reason: retired.reason.clone(),
            })
            .collect(),
        replaced: suite
            .replaced
            .iter()
            .filter(|_| tests)
            .map(|replaced| ApiReplacedTest {
                name: replaced.name.clone(),
                old: replaced.old.to_hex(),
                new: replaced.new.to_hex(),
                reason: replaced.reason.clone(),
            })
            .collect(),
    }
}

/// A workflow's write refused, as a problem.
pub(crate) fn workflow_error(error: WorkflowError) -> ApiError {
    match error {
        WorkflowError::Invalid(_)
        | WorkflowError::Schema { .. }
        | WorkflowError::DescriptionTooLong(_) => {
            ApiError::new(ProblemType::InvalidWorkflow, error.to_string())
        }
        WorkflowError::NameTaken(message) => ApiError::new(ProblemType::NameTaken, message),
        WorkflowError::NotForkable(message) => ApiError::new(ProblemType::NotForkable, message),
        WorkflowError::NotAFork(message) => ApiError::new(ProblemType::NotAFork, message),
        WorkflowError::NotFound(message) => ApiError::not_found(message),
        WorkflowError::SecretRequired(ref refused) => {
            ApiError::secret_required(error.to_string(), refused)
        }
        WorkflowError::Secret(error) => error.into(),
        WorkflowError::ActionSetNotFound(digest) => ApiError::not_found(format!(
            "no action set {digest} for this tenant: register it with POST /v1/action-sets"
        )),
        WorkflowError::InvalidCode(invalid) => {
            rewind_engine::StartError::InvalidCode(invalid).into()
        }
        WorkflowError::TypeErrors(diagnostics) => {
            rewind_engine::StartError::TypeErrors(diagnostics).into()
        }
        WorkflowError::Checker(error) => error.into(),
        WorkflowError::NulCharacter(error) => error.into(),
        WorkflowError::Store(error) => error.into(),
        WorkflowError::Blob(error) => error.into(),
        WorkflowError::Suite(error) => testing::suite_error(error.into()),
        WorkflowError::CorruptTest(message) => ApiError::internal(message),
        WorkflowError::TestsFailed(reports) => testing::tests_failed("the suite", &reports),
        WorkflowError::Tests(error) => testing::test_error(error),
    }
}
