//! Reading for the dashboard: a code's source, its title and what the runs did with it, the
//! title a person gives it, a payload on demand (`archi/05-testing-and-observability.md`, "Read
//! model" and "Dashboard"; `archi/04-tools.md`, "Script titles"). The lists and statistics drawn
//! from the counters are in [`super::stats`].

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::common::timestamp;
use rewind_api::reading::PayloadQuery;
use rewind_api::scripts::{RunCounts, Script, SetScriptTitle};
use rewind_core::{Digest, MAX_DESCRIPTION_CHARS, MAX_TITLE_CHARS, canonical_json};
use rewind_engine::ports::{
    CatalogStore, Page, Predicate, RunOrder, Scope, ScriptTitle, SearchCursor, Term, Window,
};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::http::search;
use crate::http::stats::run_counts;

/// `GET /v1/scripts/{digest}`: the source, its title, and what the runs the caller sees did
/// with it. A caller limited to a namespace or to labels reads only the workflows of the runs it
/// sees.
pub(super) async fn code(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
) -> Result<Json<Script>, ApiError> {
    caller.require(Permission::RunsRead)?;
    Ok(Json(seen_script(&app, &caller, &digest).await?))
}

/// `PATCH /v1/scripts/{digest}`: the title and description a person gives the code, for the
/// organization, or their removal. The catalog sends the `code` change: every process, and
/// every dashboard following the organization, reads the title again.
pub(super) async fn set_title(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
    Body(request): Body<SetScriptTitle>,
) -> Result<Json<Script>, ApiError> {
    caller.require(Permission::ScriptsWrite)?;
    let title = match request.title {
        Some(title) => Some(ScriptTitle {
            title: given_title(&title)?,
            description: given_description(request.description.as_deref())?,
            updated_by: caller.writer(),
            updated_at: app.now(),
        }),
        None => None,
    };
    // Only a code the caller can read is given a title.
    let seen = seen_script(&app, &caller, &digest).await?;
    let code = Digest::from_hex(&seen.digest).map_err(ApiError::internal)?;
    if !app
        .catalog()
        .set_script_title(&caller.tenant, code, title.as_ref())
        .await?
    {
        return Err(no_code(&digest));
    }
    Ok(Json(seen_script(&app, &caller, &digest).await?))
}

/// A title as a person gives it: trimmed, 1 to 120 characters.
fn given_title(title: &str) -> Result<String, ApiError> {
    let title = title.trim();
    let length = title.chars().count();
    if length == 0 {
        return Err(ApiError::new(
            ProblemType::InvalidTitle,
            "title is empty: give 1 to 120 characters, or null to remove the title",
        ));
    }
    if length > MAX_TITLE_CHARS {
        return Err(ApiError::new(
            ProblemType::InvalidTitle,
            format!(
                "title has {length} characters, above the limit of {MAX_TITLE_CHARS}: shorten \
                 it, and say more in the description"
            ),
        ));
    }
    Ok(title.to_owned())
}

/// A description as a person gives it: trimmed, at most 2,000 characters; none when empty.
fn given_description(description: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(description) = description.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let length = description.chars().count();
    if length > MAX_DESCRIPTION_CHARS {
        return Err(ApiError::new(
            ProblemType::InvalidTitle,
            format!(
                "description has {length} characters, above the limit of \
                 {MAX_DESCRIPTION_CHARS}: shorten it"
            ),
        ));
    }
    Ok(Some(description.to_owned()))
}

fn no_code(digest: &str) -> ApiError {
    ApiError::not_found(format!(
        "no code {digest} for this tenant: GET /v1/scripts lists the workflows its runs ran"
    ))
}

/// The code `digest` names, as `GET /v1/scripts/{digest}` shows it, if the caller may read
/// it: the source, its title, and what the runs the caller sees did with it.
async fn seen_script(app: &App, caller: &Caller, digest: &str) -> Result<Script, ApiError> {
    let missing = || no_code(digest);
    let parsed = Digest::from_hex(digest).map_err(|_| missing())?;
    let stored = app
        .catalog()
        .get_script(&caller.tenant, parsed)
        .await?
        .ok_or_else(missing)?;
    if caller.scoped() || caller.namespace.is_some() {
        // A run the caller sees ran it: one summary of the index, within the caller's scope.
        let mut query = search::run_query(caller, None, None, app.now())?;
        query.terms.push(Term {
            negated: false,
            predicate: Predicate::Script(vec![parsed.to_hex()]),
        });
        let found = app
            .edge
            .search(
                &mut app.gateway(),
                &search::scope_of(&query),
                &query,
                RunOrder::Started,
                &Page {
                    cursor: None::<SearchCursor>,
                    limit: 1,
                    budget: rewind_engine::ports::SCAN_BUDGET,
                },
            )
            .await?;
        if found.summaries.is_empty() {
            return Err(missing());
        }
    }
    let counted = if caller.labels.is_empty() {
        let scope = match &caller.namespace {
            Some(namespace) => Scope::Namespace {
                tenant: caller.tenant.clone(),
                namespace: namespace.clone(),
            },
            None => Scope::Tenant(caller.tenant.clone()),
        };
        let window = Window::covering(rewind_core::Timestamp(0), app.now());
        let (counters, _) = app
            .edge
            .statistics(&mut app.gateway(), &scope, window)
            .await?;
        counters.scripts.get(&parsed).cloned()
    } else {
        None
    };
    let source = stored.code.source().to_owned();
    let title = app.titles(&caller.tenant, [parsed]).await?.remove(&parsed);
    Ok(Script {
        digest: parsed.to_hex(),
        title_source: title.as_ref().map(|title| title.source),
        description: title.as_ref().and_then(|title| title.description.clone()),
        title: title.map(|title| title.title),
        headline: rewind_api::scripts::headline(&source),
        source,
        runs: counted
            .as_ref()
            .map_or_else(RunCounts::default, |counts| run_counts(&counts.runs)),
        actions: counted
            .as_ref()
            .map(|counts| counts.actions.iter().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default()
            .into_iter()
            .collect(),
        first_run_at: counted
            .as_ref()
            .and_then(|counts| counts.first_started_at)
            .map(timestamp),
        last_run_at: counted
            .as_ref()
            .and_then(|counts| counts.last_started_at)
            .map(timestamp),
    })
}

/// `GET /v1/payloads/{digest}?run=…`: a payload's value, read through the run that holds it.
pub(super) async fn payload(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
    Query(query): Query<PayloadQuery>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsRead)?;
    let parsed = Digest::from_hex(&digest)
        .map_err(|_| ApiError::not_found(format!("no payload {digest} for this tenant")))?;
    let run = &query.run;
    let view = app.run(&caller, run).await?;
    let value = app
        .views()
        .value(&view.head.run, parsed)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("no payload {digest} in the history of run {run}"))
        })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        canonical_json(&value),
    )
        .into_response())
}
