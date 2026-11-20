//! `rewind namespaces list|show|rename|quotas|delete`: the end users of the tenant, their
//! display names, their limits and their use (`archi/06-storage-and-scale.md`, "Namespaces", "Quotas"). These commands span
//! namespaces: they are bound to one only when `--namespace` names it.

use rewind_api::namespaces::{
    Namespace, NamespaceDefaults, NamespaceDetail, NamespaceList, NamespaceListQuery,
    NamespaceName, NamespacePatch, NamespaceQuery, PutNamespace, PutNamespaceDefaults, QuotaUsage,
    Quotas,
};
use serde_json::json;

use super::Server;
use crate::args::{NamespacesCommand, NamespacesListArgs, QuotasArgs};
use crate::error::CliError;
use crate::exit::Exit;
use crate::output::Console;
use crate::render;

pub(super) async fn namespaces(
    server: &Server<'_>,
    command: NamespacesCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    match command {
        NamespacesCommand::List(args) => list(server, args, console).await,
        NamespacesCommand::Show { name, month } => {
            let name = parse(&name)?;
            let client = server.unbound_client().await?;
            let detail = client
                .get_namespace(&name, &NamespaceQuery { month })
                .await?;
            if console.json() {
                console.print_json(&detail)?;
            } else {
                console.text(&show(&detail));
            }
            Ok(Exit::Success)
        }
        NamespacesCommand::Rename { name, display_name } => {
            let name = parse(&name)?;
            let client = server.unbound_client().await?;
            let detail = client
                .patch_namespace(&name, &NamespacePatch::display_name(display_name))
                .await?;
            if console.json() {
                console.print_json(&detail)?;
            } else {
                console.line(match &detail.namespace.display_name {
                    Some(display) => format!("namespace {name} is shown as {display:?}"),
                    None => format!("namespace {name} has no display name"),
                });
            }
            Ok(Exit::Success)
        }
        NamespacesCommand::Quotas(args) => quotas(server, args, console).await,
        NamespacesCommand::Delete { name } => {
            let name = parse(&name)?;
            let client = server.unbound_client().await?;
            client.delete_namespace(&name).await?;
            if console.json() {
                console.print_json(&json!({ "namespace": name, "erasing": true }))?;
            } else {
                console.line(format!(
                    "namespace {name} is being erased: its tokens are revoked and its active runs \
                     cancelled; its runs, secrets and use are erased in the background"
                ));
            }
            Ok(Exit::Success)
        }
    }
}

fn parse(name: &str) -> Result<NamespaceName, CliError> {
    NamespaceName::new(name).map_err(|error| CliError::Usage(error.to_string()))
}

async fn list(
    server: &Server<'_>,
    args: NamespacesListArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let query = NamespaceListQuery {
        prefix: args.prefix,
        above: args.above,
        order: args.order.map(Into::into),
        month: args.month,
        after: args.after,
        limit: args.limit,
    };
    let client = server.unbound_client().await?;
    let page = client.list_namespaces(&query).await?;
    if console.json() {
        console.print_json(&page)?;
    } else {
        console.text(&listing(&page));
        if let Some(cursor) = &page.next {
            console.note(format!("more namespaces: --after {cursor}"));
        }
    }
    Ok(Exit::Success)
}

async fn quotas(
    server: &Server<'_>,
    args: QuotasArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.unbound_client().await?;
    let month = NamespaceQuery::default();
    let Some(name) = &args.name else {
        let mut defaults = client.namespace_defaults(&month).await?;
        if args.changes() {
            let mut limits = defaults.defaults.clone();
            args.apply(&mut limits);
            defaults = client
                .put_namespace_defaults(&PutNamespaceDefaults { defaults: limits })
                .await?;
        }
        if console.json() {
            console.print_json(&defaults)?;
        } else {
            console.text(&show_defaults(&defaults));
        }
        return Ok(Exit::Success);
    };
    let name = parse(name)?;
    let detail = if args.changes() {
        // The exceptions given change; the others stay. A namespace not created yet has none.
        let mut exceptions = match client.get_namespace(&name, &month).await {
            Ok(detail) => detail.namespace.exceptions,
            Err(error)
                if error
                    .problem()
                    .is_some_and(|problem| problem.is(rewind_api::ProblemType::NotFound)) =>
            {
                Quotas::default()
            }
            Err(error) => return Err(error.into()),
        };
        args.apply(&mut exceptions);
        client
            .put_namespace(&name, &PutNamespace { exceptions })
            .await?
    } else {
        client.get_namespace(&name, &month).await?
    };
    if console.json() {
        console.print_json(&detail)?;
    } else {
        console.text(&show(&detail));
    }
    Ok(Exit::Success)
}

// -------------------------------------------------------------------------------------------
// Text

fn or_dash<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// The use of a counted quota against its limit: `3/10`, or `3` without a limit.
fn against<T: ToString>(used: impl ToString, limit: Option<T>) -> String {
    match limit {
        Some(limit) => format!("{}/{}", used.to_string(), limit.to_string()),
        None => used.to_string(),
    }
}

/// Its name, then its display name quoted: `user_2fQ81 "Acme Corp"`.
fn named(namespace: &Namespace) -> String {
    match &namespace.display_name {
        Some(display) => format!("{} {display:?}", namespace.name),
        None => namespace.name.to_string(),
    }
}

fn listing(page: &NamespaceList) -> String {
    if page.namespaces.is_empty() {
        return format!("no namespace for {}\n", page.month);
    }
    let mut rows = vec![[
        "NAME".to_owned(),
        "CPU".to_owned(),
        "ACTIVE".to_owned(),
        "RUNS".to_owned(),
        "SCRIPTS".to_owned(),
        "LAST ACTIVE".to_owned(),
    ]];
    for namespace in &page.namespaces {
        let limits = &namespace.limits;
        let usage = &namespace.usage;
        let mut name = named(namespace);
        if namespace.deleting_since.is_some() {
            name.push_str(" (erasing)");
        }
        rows.push([
            name,
            against(usage.cpu, limits.cpu_per_month),
            against(usage.active_runs, limits.active_runs),
            against(usage.runs_started, limits.runs_per_month),
            against(usage.scripts, limits.scripts),
            namespace.last_active_at.to_string(),
        ]);
    }
    let counted = match page.total {
        1 => "1 namespace".to_owned(),
        total => format!("{total} namespaces"),
    };
    format!("{} · {counted}\n{}", page.month, render::table(&rows))
}

/// The six quotas: their limit, a second column, and the use of the counted ones.
fn quota_rows(
    header: [&str; 4],
    first: &Quotas,
    second: &Quotas,
    usage: &QuotaUsage,
) -> Vec<[String; 4]> {
    vec![
        header.map(str::to_owned),
        [
            "cpu_per_month".into(),
            or_dash(first.cpu_per_month),
            or_dash(second.cpu_per_month),
            usage.cpu.to_string(),
        ],
        [
            "active_runs".into(),
            or_dash(first.active_runs),
            or_dash(second.active_runs),
            usage.active_runs.to_string(),
        ],
        [
            "runs_per_month".into(),
            or_dash(first.runs_per_month),
            or_dash(second.runs_per_month),
            usage.runs_started.to_string(),
        ],
        [
            "scripts".into(),
            or_dash(first.scripts),
            or_dash(second.scripts),
            usage.scripts.to_string(),
        ],
        [
            "max_ttl".into(),
            or_dash(first.max_ttl),
            or_dash(second.max_ttl),
            "-".into(),
        ],
        [
            "max_script_lines".into(),
            or_dash(first.max_script_lines),
            or_dash(second.max_script_lines),
            "-".into(),
        ],
    ]
}

fn show(detail: &NamespaceDetail) -> String {
    let namespace = &detail.namespace;
    let mut text = format!(
        "namespace {}: created {}, last active {}\n",
        named(namespace),
        namespace.created_at,
        namespace.last_active_at
    );
    if let Some(since) = namespace.deleting_since {
        text.push_str(&format!("being erased since {since}\n"));
    }
    text.push_str(&format!("\nlimits and use in {}\n", detail.month));
    text.push_str(&render::table(&quota_rows(
        ["QUOTA", "LIMIT", "EXCEPTION", "USED"],
        &namespace.limits,
        &namespace.exceptions,
        &namespace.usage,
    )));
    text.push_str(&format!("\ntokens in use: {}\n", detail.tokens));
    if let Some(before) = detail.tokens_revoked_before {
        text.push_str(&format!(
            "signed tokens issued before {before} are refused\n"
        ));
    }
    if !detail.days.is_empty() {
        let mut rows = vec![["DAY".to_owned(), "CPU".to_owned(), "RUNS".to_owned()]];
        for day in &detail.days {
            rows.push([
                day.day.to_string(),
                day.cpu.to_string(),
                day.runs_started.to_string(),
            ]);
        }
        text.push('\n');
        text.push_str(&render::table(&rows));
    }
    if !detail.alerts.is_empty() {
        let mut rows = vec![[
            "ALERT".to_owned(),
            "AT".to_owned(),
            "USED".to_owned(),
            "RAISED".to_owned(),
        ]];
        for alert in &detail.alerts {
            rows.push([
                alert.quota.to_string(),
                format!("{} %", alert.threshold),
                format!("{}/{}", alert.used, alert.limit),
                alert.raised_at.to_string(),
            ]);
        }
        text.push('\n');
        text.push_str(&render::table(&rows));
    }
    text
}

fn show_defaults(defaults: &NamespaceDefaults) -> String {
    let mut text = format!(
        "the limits of every namespace without an exception, and the organization's ceiling, \
         with its use in {}\n",
        defaults.month
    );
    text.push_str(&render::table(&quota_rows(
        ["QUOTA", "DEFAULT", "CEILING", "USED"],
        &defaults.defaults,
        &defaults.ceiling,
        &defaults.usage,
    )));
    text
}

#[cfg(test)]
mod tests {
    use rewind_api::common::Timestamp;

    use super::*;

    fn at() -> Timestamp {
        "2026-11-14T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn a_listing_shows_the_use_against_the_limits() {
        let page = NamespaceList {
            month: "2026-11".into(),
            namespaces: vec![Namespace {
                name: "alice".parse().unwrap(),
                display_name: Some("Acme Corp".into()),
                limits: Quotas {
                    active_runs: Some(10),
                    cpu_per_month: Some("2h".parse().unwrap()),
                    ..Quotas::default()
                },
                exceptions: Quotas::default(),
                usage: QuotaUsage {
                    cpu: "30m".parse().unwrap(),
                    active_runs: 3,
                    runs_started: 12,
                    scripts: 2,
                },
                created_at: at(),
                last_active_at: at(),
                deleting_since: Some(at()),
            }],
            total: 12,
            next: None,
        };
        let text = listing(&page);
        assert!(text.starts_with("2026-11 · 12 namespaces\nNAME"), "{text}");
        assert!(text.contains("alice \"Acme Corp\" (erasing)"), "{text}");
        assert!(text.contains("30m/2h"), "{text}");
        assert!(text.contains("3/10"), "{text}");
        assert!(text.contains(" 12 "), "{text}");
    }
}
