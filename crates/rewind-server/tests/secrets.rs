//! Secrets and signing keys over HTTP (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/06-storage-and-scale.md`, "What the caller may do"; `archi/07-code-architecture.md`,
//! "HTTP API /v1"): written and never read back, the organization's within the caller's labels,
//! a namespace's within its end user's token, the checks of a start answered as
//! `secret-required`, the signing keys listed, replaced and published without authentication.
//!
//! Each scenario runs on the engine's in-memory adapters, and on Turso, whose `SecretStore` the
//! stores workstream builds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use rewind_api::access::{BuiltInRole, Permission};
use rewind_api::common::Labels;
use rewind_api::namespaces::{CreateToken, NamespaceName};
use rewind_api::runs::StartRun;
use rewind_api::secrets::{AttachmentProblemKind, PutSecret, SecretScope};
use rewind_api::toolsets::{CreateToolset, HostTool};
use rewind_api::{Client, Problem, ProblemType};
use serde_json::json;
use support::TestServer;

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn refused<T: std::fmt::Debug>(result: Result<T, rewind_api::Error>) -> (u16, Option<ProblemType>) {
    let problem = problem(result.unwrap_err());
    (problem.status, problem.kind())
}

fn ns(name: &str) -> NamespaceName {
    name.parse().unwrap()
}

fn value(value: &str) -> PutSecret {
    PutSecret {
        value: value.into(),
        labels: Labels::new(),
    }
}

fn labelled(value: &str, labels: &[(&str, &str)]) -> PutSecret {
    PutSecret {
        value: value.into(),
        labels: labels.iter().copied().collect(),
    }
}

fn names(list: &rewind_api::secrets::SecretList) -> Vec<String> {
    list.secrets
        .iter()
        .map(|secret| secret.reference.clone())
        .collect()
}

async fn token(customer: &Client, namespace: &str, permissions: &[Permission]) -> String {
    customer
        .create_token(
            &ns(namespace),
            &CreateToken {
                permissions: Some(permissions.to_vec()),
                ..CreateToken::default()
            },
        )
        .await
        .unwrap()
        .token
        .unwrap()
}

/// A harness key of `acme` limited to `labels`, whose requests name no namespace.
async fn labelled_key(server: &TestServer, labels: &[(&str, &str)]) -> Client {
    let key = server
        .admin
        .create_api_key(&rewind_api::access::CreateApiKey {
            tenant: Some("acme".into()),
            name: "a labelled key of the tests".into(),
            role: BuiltInRole::Harness.as_str().into(),
            allowed_labels: labels.iter().copied().collect(),
        })
        .await
        .unwrap();
    server.bare(key.key.as_deref().unwrap())
}

// -------------------------------------------------------------------------------------------
// Scenarios

/// Written, listed with their metadata, replaced, deleted; a value is never answered.
async fn secrets_are_written_and_never_read_back(server: &TestServer) {
    let customer = server.tenant_key("acme", "harness").await;
    let written = customer
        .put_secret("zendesk", &labelled("zd_secret_value", &[("env", "prod")]))
        .await
        .unwrap();
    assert_eq!(
        (
            written.scope,
            written.reference.as_str(),
            written.namespace.as_ref()
        ),
        (SecretScope::Org, "org/zendesk", None)
    );
    assert!(
        written.created_by.starts_with("key:"),
        "{}",
        written.created_by
    );
    assert_eq!(written.labels.0["env"], "prod");
    assert!(written.reads.is_empty() && written.last_read.is_none());
    let raw = reqwest::Client::new()
        .get(format!("{}/v1/secrets", server.url))
        .bearer_auth(support::ADMIN_KEY)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        raw.contains("org/zendesk") && !raw.contains("zd_secret_value"),
        "{raw}"
    );

    // Put again: replaced, 200.
    let response = reqwest::Client::new()
        .put(format!("{}/v1/secrets/zendesk", server.url))
        .bearer_auth(support::ADMIN_KEY)
        .json(&json!({ "value": "zd_other" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "replaced");
    assert_eq!(names(&customer.secrets().await.unwrap()), ["org/zendesk"]);

    let github = customer
        .put_namespace_secret(&ns("alice"), "github", &value("ghp_alice"))
        .await
        .unwrap();
    assert_eq!(
        (github.scope, github.reference.as_str(), github.namespace),
        (SecretScope::Ns, "ns/github", Some(ns("alice")))
    );
    assert_eq!(
        names(&customer.namespace_secrets(&ns("alice")).await.unwrap()),
        ["ns/github"]
    );
    assert!(
        customer
            .namespace_secrets(&ns("bob"))
            .await
            .unwrap()
            .secrets
            .is_empty()
    );
    assert_eq!(
        names(&customer.secrets().await.unwrap()),
        ["org/zendesk"],
        "a namespace's secret is not the organization's"
    );

    // Refused writes.
    assert_eq!(
        refused(
            customer
                .put_namespace_secret(&ns("alice"), "github", &labelled("x", &[("env", "prod")]))
                .await
        ),
        (400, Some(ProblemType::InvalidRequest)),
        "a namespace's secret takes no label"
    );
    assert_eq!(
        refused(customer.put_secret("GitHub", &value("x")).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    assert_eq!(
        refused(
            customer
                .put_secret("big", &value(&"x".repeat(16 * 1024 + 1)))
                .await
        ),
        (413, Some(ProblemType::PayloadTooLarge))
    );

    // Usage of a secret nothing reads.
    let usage = customer.secret_usage("zendesk").await.unwrap();
    assert_eq!(usage.secret.reference, "org/zendesk");
    assert!(usage.active_runs.is_empty() && usage.workflows.is_empty());
    assert_eq!(
        refused(customer.secret_usage("nothing").await),
        (404, Some(ProblemType::NotFound))
    );
    customer
        .namespace_secret_usage(&ns("alice"), "github")
        .await
        .unwrap();

    customer.delete_secret("zendesk").await.unwrap();
    assert_eq!(
        refused(customer.delete_secret("zendesk").await),
        (404, Some(ProblemType::NotFound))
    );
    customer
        .delete_namespace_secret(&ns("alice"), "github")
        .await
        .unwrap();
    assert!(
        customer
            .namespace_secrets(&ns("alice"))
            .await
            .unwrap()
            .secrets
            .is_empty()
    );

    // A reader reads their names, and writes none.
    let reader = server.tenant_key("acme", "reader").await;
    reader.secrets().await.unwrap();
    assert_eq!(
        refused(reader.put_secret("zendesk", &value("x")).await),
        (403, Some(ProblemType::Forbidden))
    );
    let worker = server.tenant_key("acme", "worker").await;
    assert_eq!(
        refused(worker.secrets().await),
        (403, Some(ProblemType::Forbidden))
    );
}

/// A key limited to labels sees, writes and deletes only the organization's secrets carrying
/// them, and the ones it writes get them; it never writes a namespace's.
async fn labels_bound_the_organization_s_secrets(server: &TestServer) {
    let customer = server.tenant_key("acme", "harness").await;
    customer
        .put_secret("dev", &labelled("d", &[("env", "dev")]))
        .await
        .unwrap();
    let prod = labelled_key(server, &[("env", "prod")]).await;
    let written = prod.put_secret("zendesk", &value("z")).await.unwrap();
    assert_eq!(written.labels.0["env"], "prod", "it gets the key's labels");
    assert_eq!(names(&prod.secrets().await.unwrap()), ["org/zendesk"]);
    assert_eq!(
        names(&customer.secrets().await.unwrap()),
        ["org/dev", "org/zendesk"]
    );
    assert_eq!(
        refused(prod.put_secret("dev", &value("d")).await),
        (403, Some(ProblemType::Forbidden)),
        "a secret without its labels"
    );
    assert_eq!(
        refused(
            prod.put_secret("x", &labelled("x", &[("env", "dev")]))
                .await
        ),
        (403, Some(ProblemType::Forbidden)),
        "a contradicting label"
    );
    assert_eq!(
        refused(prod.delete_secret("dev").await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(prod.secret_usage("dev").await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(
            prod.put_namespace_secret(&ns("alice"), "github", &value("g"))
                .await
        ),
        (403, Some(ProblemType::Forbidden)),
        "a namespace's secret serves the end user's runs whatever their labels"
    );
    customer
        .put_namespace_secret(&ns("alice"), "github", &value("g"))
        .await
        .unwrap();
    assert_eq!(
        names(&prod.namespace_secrets(&ns("alice")).await.unwrap()),
        ["ns/github"],
        "it reads a namespace's secrets as any grant does"
    );
}

/// An end user's token with `secrets:*` reaches its namespace's secrets, never the
/// organization's nor another namespace's.
async fn a_token_reaches_its_namespace_s_secrets_only(server: &TestServer) {
    let customer = server.tenant_key("acme", "harness").await;
    customer.put_secret("zendesk", &value("z")).await.unwrap();
    let alice = server.bare(
        &token(
            &customer,
            "alice",
            &[Permission::SecretsRead, Permission::SecretsWrite],
        )
        .await,
    );
    let written = alice
        .put_namespace_secret(&ns("alice"), "github", &value("ghp_alice"))
        .await
        .unwrap();
    assert_eq!(written.created_by, "token:alice");
    assert_eq!(
        names(&alice.namespace_secrets(&ns("alice")).await.unwrap()),
        ["ns/github"]
    );
    assert_eq!(
        refused(alice.secrets().await),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(alice.put_secret("zendesk", &value("x")).await),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(alice.signing_keys().await),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(
            alice
                .put_namespace_secret(&ns("bob"), "github", &value("x"))
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    let reading = server.bare(&token(&customer, "alice", &[Permission::RunsRead]).await);
    assert_eq!(
        refused(reading.namespace_secrets(&ns("alice")).await),
        (403, Some(ProblemType::Forbidden))
    );
}

/// An attachment for a server that takes no secret refuses the start before any run exists,
/// as `secret-required` naming the attachment.
async fn an_attachment_for_a_server_taking_none_is_secret_required(server: &TestServer) {
    let customer = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = customer
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "ask_user".into(),
                description: "Asks the user.".into(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                policy: None,
            }],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let mut start = StartRun::new(
        "import { ask_user } from \"rewind:tools\";\nexport default async () => ask_user({});",
    );
    start.toolset = Some(toolset.id.clone());
    start.secrets = [("ask_user".to_owned(), "ns/github".to_owned())]
        .into_iter()
        .collect();
    let refusal = problem(customer.start_run(&start, None).await.unwrap_err());
    assert_eq!(
        (refusal.status, refusal.kind()),
        (422, Some(ProblemType::SecretRequired))
    );
    assert_eq!(refusal.attachments.len(), 1);
    let attachment = &refusal.attachments[0];
    assert_eq!(
        (attachment.server.as_str(), attachment.problem),
        ("ask_user", AttachmentProblemKind::TakesNone)
    );
    assert!(attachment.attachable.is_empty());
    assert!(
        refusal.detail.contains("ask_user takes no secret"),
        "{}",
        refusal.detail
    );

    let mut invalid = start.clone();
    invalid.secrets = [("ask_user".to_owned(), "github".to_owned())]
        .into_iter()
        .collect();
    assert_eq!(
        refused(customer.start_run(&invalid, None).await),
        (400, Some(ProblemType::InvalidRequest)),
        "a reference is org/<name> or ns/<name>"
    );

    // A registration's listing takes the organization's secrets only.
    let mut listed = CreateToolset::default();
    listed.secrets = [("github".to_owned(), "ns/github".to_owned())]
        .into_iter()
        .collect();
    assert_eq!(
        refused(customer.create_toolset(&listed).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    listed.secrets = [("github".to_owned(), "org/github".to_owned())]
        .into_iter()
        .collect();
    let refusal = problem(customer.create_toolset(&listed).await.unwrap_err());
    assert_eq!(
        (refusal.kind(), refusal.attachments[0].problem),
        (
            Some(ProblemType::SecretRequired),
            AttachmentProblemKind::TakesNone
        ),
        "no MCP server of the request takes it"
    );
}

/// The signing keys: listed with their issuer, replaced by `organization:write`, published
/// without authentication with `Cache-Control: max-age=300`.
async fn signing_keys_are_listed_replaced_and_published(server: &TestServer) {
    let owner = server.tenant_key("acme", "owner").await;
    let reader = server.tenant_key("acme", "reader").await;
    let listed = reader.signing_keys().await.unwrap();
    assert_eq!(listed.issuer, "https://rewind.test/organizations/acme");
    assert_eq!(
        listed.jwks_url,
        "https://rewind.test/organizations/acme/jwks.json"
    );
    assert!(listed.keys.is_empty(), "made with the first signed call");
    assert_eq!(
        refused(reader.rotate_signing_key().await),
        (403, Some(ProblemType::Forbidden))
    );

    let first = owner.rotate_signing_key().await.unwrap();
    assert_eq!(first.kid.len(), 16);
    let second = owner.rotate_signing_key().await.unwrap();
    let listed = reader.signing_keys().await.unwrap();
    assert_eq!(
        listed
            .keys
            .iter()
            .map(|key| (key.kid.clone(), key.retired_at.is_some()))
            .collect::<Vec<_>>(),
        [(second.kid.clone(), false), (first.kid.clone(), true)],
        "the old key stays published, retired"
    );

    let response = reqwest::get(format!("{}/organizations/acme/jwks.json", server.url))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["cache-control"].to_str().unwrap(),
        "max-age=300"
    );
    let jwks: rewind_api::secrets::Jwks = response.json().await.unwrap();
    assert_eq!(
        jwks.keys
            .iter()
            .map(|key| (key.kid.as_str(), key.kty.as_str(), key.crv.as_str()))
            .collect::<Vec<_>>(),
        [
            (second.kid.as_str(), "OKP", "Ed25519"),
            (first.kid.as_str(), "OKP", "Ed25519")
        ]
    );
    assert_eq!(jwks.keys[0].x.len(), 43, "32 bytes in base64url");
    let other: rewind_api::secrets::Jwks =
        reqwest::get(format!("{}/organizations/elsewhere/jwks.json", server.url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert!(other.keys.is_empty(), "another tenant's keys are its own");
}

async fn without_a_public_url_the_keys_have_no_issuer(server: &TestServer) {
    let owner = server.tenant_key("acme", "owner").await;
    assert_eq!(
        refused(owner.signing_keys().await),
        (503, Some(ProblemType::Unavailable))
    );
}

// -------------------------------------------------------------------------------------------
// In memory

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_written_and_never_read_back_in_memory() {
    let server = TestServer::in_memory(&[]).await;
    secrets_are_written_and_never_read_back(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_bound_the_organization_s_secrets_in_memory() {
    let server = TestServer::in_memory(&[]).await;
    labels_bound_the_organization_s_secrets(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reaches_its_namespace_s_secrets_only_in_memory() {
    let server = TestServer::in_memory(&[]).await;
    a_token_reaches_its_namespace_s_secrets_only(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attachment_for_a_server_taking_none_is_secret_required_in_memory() {
    let server = TestServer::in_memory(&[]).await;
    an_attachment_for_a_server_taking_none_is_secret_required(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signing_keys_are_listed_replaced_and_published_in_memory() {
    let server = TestServer::in_memory(&["--public-url", "https://rewind.test/"]).await;
    signing_keys_are_listed_replaced_and_published(&server).await;
    server.stop().await;
    let server = TestServer::in_memory(&[]).await;
    without_a_public_url_the_keys_have_no_issuer(&server).await;
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// On Turso (the stores workstream's `SecretStore`)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_written_and_never_read_back_on_turso() {
    let server = TestServer::start(&[]).await;
    secrets_are_written_and_never_read_back(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_bound_the_organization_s_secrets_on_turso() {
    let server = TestServer::start(&[]).await;
    labels_bound_the_organization_s_secrets(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reaches_its_namespace_s_secrets_only_on_turso() {
    let server = TestServer::start(&[]).await;
    a_token_reaches_its_namespace_s_secrets_only(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signing_keys_are_listed_replaced_and_published_on_turso() {
    let server = TestServer::start(&["--public-url", "https://rewind.test/"]).await;
    signing_keys_are_listed_replaced_and_published(&server).await;
    server.stop().await;
}
