//! `rewind secrets list|put|delete|usage`: the organization's secrets, or those of the namespace
//! `--namespace` names (`archi/04-tools.md`, "Secrets"). A value is read from standard input,
//! never from an argument, and no command prints one: the API never answers it.

use rewind_api::Client;
use rewind_api::namespaces::NamespaceName;
use rewind_api::secrets::{PutSecret, Secret, SecretList, SecretUsage};
use serde_json::json;

use super::Server;
use crate::args::SecretsCommand;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input;
use crate::output::Console;
use crate::render;

pub(super) async fn secrets(
    server: &Server<'_>,
    command: SecretsCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let namespace = server
        .namespace()
        .map(|name| {
            NamespaceName::new(name)
                .map_err(|error| CliError::Usage(format!("--namespace: {error}")))
        })
        .transpose()?;
    // The organization's secrets span namespaces: the request is bound to none.
    let client = server.unbound_client().await?;
    let scope = Scope {
        client: &client,
        namespace: namespace.as_ref(),
    };
    match command {
        SecretsCommand::List => {
            let list = scope.list().await?;
            if console.json() {
                console.print_json(&list)?;
            } else {
                console.text(&listed(&list, namespace.as_ref()));
            }
        }
        SecretsCommand::Put { name, labels } => {
            let value = value(console)?;
            let labels = input::labels(&labels)?;
            if namespace.is_some() && !labels.is_empty() {
                return Err(CliError::usage(
                    "--label: a namespace's secret takes no label, its namespace is its scope",
                ));
            }
            let secret = scope.put(&name, &PutSecret { value, labels }).await?;
            if console.json() {
                console.print_json(&secret)?;
            } else {
                console.line(format!("{} written", described(&secret)));
            }
        }
        SecretsCommand::Delete { name } => {
            scope.delete(&name).await?;
            if console.json() {
                console.print_json(&json!({ "secret": name, "deleted": true }))?;
            } else {
                console.line(format!(
                    "secret {name} deleted: the next call of a run that attaches it is parked \
                     until it is put again"
                ));
            }
        }
        SecretsCommand::Usage { name } => {
            let usage = scope.usage(&name).await?;
            if console.json() {
                console.print_json(&usage)?;
            } else {
                console.text(&used(&usage));
            }
        }
    }
    Ok(Exit::Success)
}

/// Whose secrets: the organization's, or a namespace's.
struct Scope<'a> {
    client: &'a Client,
    namespace: Option<&'a NamespaceName>,
}

impl Scope<'_> {
    async fn list(&self) -> Result<SecretList, CliError> {
        Ok(match self.namespace {
            Some(namespace) => self.client.namespace_secrets(namespace).await?,
            None => self.client.secrets().await?,
        })
    }

    async fn put(&self, name: &str, secret: &PutSecret) -> Result<Secret, CliError> {
        Ok(match self.namespace {
            Some(namespace) => {
                self.client
                    .put_namespace_secret(namespace, name, secret)
                    .await?
            }
            None => self.client.put_secret(name, secret).await?,
        })
    }

    async fn delete(&self, name: &str) -> Result<(), CliError> {
        Ok(match self.namespace {
            Some(namespace) => self.client.delete_namespace_secret(namespace, name).await?,
            None => self.client.delete_secret(name).await?,
        })
    }

    async fn usage(&self, name: &str) -> Result<SecretUsage, CliError> {
        Ok(match self.namespace {
            Some(namespace) => self.client.namespace_secret_usage(namespace, name).await?,
            None => self.client.secret_usage(name).await?,
        })
    }
}

/// The value on standard input, one trailing newline dropped: `echo $TOKEN |` adds one.
fn value(console: &mut Console) -> Result<String, CliError> {
    let mut value = console.read_input().map_err(|error| {
        CliError::usage(format!(
            "the value cannot be read from standard input: {error}"
        ))
    })?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.is_empty() {
        return Err(CliError::usage(
            "no value on standard input: give it there, never as an argument, such as \
             `rewind secrets put github < token.txt`",
        ));
    }
    Ok(value)
}

/// `org/zendesk (env=prod)`, `ns/github of the namespace alice`.
fn described(secret: &Secret) -> String {
    let mut text = format!("secret {}", secret.reference);
    if let Some(namespace) = &secret.namespace {
        text.push_str(&format!(" of the namespace {}", namespace.as_str()));
    }
    if !secret.labels.is_empty() {
        let labels: Vec<String> = secret
            .labels
            .0
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        text.push_str(&format!(" ({})", labels.join(", ")));
    }
    text
}

fn listed(list: &SecretList, namespace: Option<&NamespaceName>) -> String {
    if list.secrets.is_empty() {
        return match namespace {
            Some(namespace) => format!("no secret in the namespace {}\n", namespace.as_str()),
            None => "no secret of the organization\n".to_owned(),
        };
    }
    let mut rows = vec![[
        "SECRET".to_owned(),
        "LABELS".to_owned(),
        "UPDATED".to_owned(),
        "BY".to_owned(),
        "READS (30 DAYS)".to_owned(),
        "LAST READ".to_owned(),
    ]];
    for secret in &list.secrets {
        let labels: Vec<String> = secret
            .labels
            .0
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let reads: u64 = secret.reads.iter().map(|day| day.reads).sum();
        rows.push([
            secret.reference.clone(),
            if labels.is_empty() {
                "-".to_owned()
            } else {
                labels.join(",")
            },
            secret.updated_at.to_string(),
            secret.updated_by.clone(),
            reads.to_string(),
            secret.last_read.as_ref().map_or_else(
                || "never".to_owned(),
                |last| {
                    format!(
                        "{} (run {}, {})",
                        last.at,
                        last.run,
                        last.namespace.as_str()
                    )
                },
            ),
        ]);
    }
    render::table(&rows)
}

fn used(usage: &SecretUsage) -> String {
    let mut lines = vec![described(&usage.secret)];
    lines.push(format!(
        "active runs attaching it: {}{}",
        usage.active_total,
        listed_runs(&usage.active_runs)
    ));
    lines.push(format!(
        "waiting on a call it serves: {}{}",
        usage.waiting_total,
        listed_runs(&usage.waiting_runs)
    ));
    if usage.workflows.is_empty() {
        lines.push("no workflow's latest version attaches it".to_owned());
    } else {
        lines.push("workflows whose latest version attaches it:".to_owned());
        for workflow in &usage.workflows {
            lines.push(format!(
                "  {}@{} for {}",
                workflow.name,
                &workflow.version[..12.min(workflow.version.len())],
                workflow.servers.join(", ")
            ));
        }
    }
    lines.join("\n") + "\n"
}

fn listed_runs(runs: &[String]) -> String {
    if runs.is_empty() {
        String::new()
    } else {
        format!(" ({})", runs.join(", "))
    }
}
