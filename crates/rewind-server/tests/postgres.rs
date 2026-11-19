//! The API end to end on Postgres (Docker): a server with the api role only and two servers with
//! the worker role only share one database and one blob directory. Runs started through the api
//! server are activated by the workers; host-tool tasks are long-polled through the api server;
//! quotas hold across them; members sign in on the api server, which the workers refuse.
//! Skipped, with a message, when Docker is absent, unless
//! `REWIND_REQUIRE_DOCKER` is set (CI): they fail then.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use rewind_api::access::BuiltInRole;
use rewind_api::common::{Labels, RunStatus};
use rewind_api::organization::{CreateInvitation, CreateOrganization};
use rewind_api::runs::{RunFilter, StartRun};
use rewind_api::tasks::{CompleteTask, TaskQuery};
use rewind_api::toolsets::{CreateToolset, HostTool};
use rewind_engine::ports::InvitationId;
use serde_json::json;
use support::{Browser, Postgres, TestServer, token, wait};

const SLEEP: &str = r#"import { sleep, signal } from "rewind";
export default async function main(input: { n: number }) {
  await sleep(100);
  const go = await signal<number>("go");
  return { n: input.n, go };
}
"#;

const ASK: &str = r#"import { ask_user } from "rewind:tools";
export default async function main() {
  return { answer: await ask_user({ question: "ready?" }) };
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_server_and_two_workers_share_one_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let shared = tempfile::tempdir().unwrap();
    let blobs = shared.path().join("blobs");
    let url = postgres.url();
    let worker = |name: &'static str| {
        let (url, blobs) = (url.clone(), blobs.clone());
        async move {
            TestServer::start_on(
                tempfile::tempdir().unwrap(),
                &url,
                &blobs,
                &[
                    "--role",
                    "worker",
                    "--worker-name",
                    name,
                    "--quota-active-runs",
                    "20",
                ],
            )
            .await
        }
    };
    let first = worker("worker-1").await;
    let second = worker("worker-2").await;
    let api = TestServer::start_on(
        tempfile::tempdir().unwrap(),
        &url,
        &blobs,
        &["--role", "api", "--quota-active-runs", "20"],
    )
    .await;
    let info = api.admin.server_info().await.unwrap();
    assert_eq!(info.roles, [rewind_api::admin::ServerRole::Api]);
    let harness = api.key(BuiltInRole::Harness, &[]).await;

    let mut runs = Vec::new();
    for n in 0..6 {
        let mut request = StartRun::new(SLEEP);
        request.input = json!({ "n": n });
        runs.push(harness.start_run(&request, None).await.unwrap().id);
    }
    for (n, run) in runs.iter().enumerate() {
        harness
            .signal_run(run, "go", &json!(n * 10), None)
            .await
            .unwrap();
    }
    for (n, run) in runs.iter().enumerate() {
        let ended = harness.get_run(run, wait("60s")).await.unwrap();
        assert_eq!(ended.status, RunStatus::Completed, "{ended:?}");
        assert_eq!(
            ended.output.unwrap().value,
            Some(json!({ "n": n, "go": n * 10 }))
        );
    }

    // A host tool, long-polled through the api server.
    let toolset = harness
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "ask_user".into(),
                description: "Ask the user.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "question": { "type": "string" } },
                    "required": ["question"]
                }),
                output_schema: None,
                policy: None,
            }],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let mut request = StartRun::new(ASK);
    request.toolset = Some(toolset.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();
    let task = harness
        .claim_tasks(&TaskQuery {
            wait: Some("30s".parse().unwrap()),
            ..TaskQuery::default()
        })
        .await
        .unwrap()
        .tasks
        .remove(0);
    harness
        .complete_task(
            &task.token,
            &CompleteTask {
                result: json!("yes"),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("60s")).await.unwrap();
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "answer": "yes" }))
    );

    api.stop().await;
    first.stop().await;
    second.stop().await;
    drop(postgres);
}

/// Members on Postgres: an organization created, an invitee signed in with the invitation's
/// token and its grant claimed, a member removed and cut at the next request; the worker, on
/// the same database, refuses access tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_sign_in_and_are_granted_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let shared = tempfile::tempdir().unwrap();
    let blobs = shared.path().join("blobs");
    let (identity, _clock) = support::identity();
    identity.set_sign_ups_closed(true);
    identity.add_user("ada@example.net", true);
    let api = TestServer::start_with(
        tempfile::tempdir().unwrap(),
        &postgres.url(),
        &blobs,
        &["--role", "api"],
        Some(identity.clone()),
    )
    .await;
    let worker = TestServer::start_with(
        tempfile::tempdir().unwrap(),
        &postgres.url(),
        &blobs,
        &["--role", "worker", "--worker-name", "worker-1"],
        Some(identity.clone()),
    )
    .await;

    let mut ada = Browser::new(&api.url);
    let state = ada
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let acme = api
        .client(Some(&token(&state).0))
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    let owner = api.client(Some(&token(&ada.switch(&acme.id).await.unwrap()).0));
    assert_eq!(owner.whoami().await.unwrap().grant.unwrap().role, "owner");
    let invitation = owner
        .invite(&CreateInvitation {
            email: "grace@example.net".into(),
            role: "harness".into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap();
    let invitation_token = identity
        .invitation_token(&InvitationId::new(&invitation.id))
        .unwrap();
    let mut grace = Browser::new(&api.url);
    let state = grace
        .sign_in(&identity, "grace@example.net", Some(&invitation_token))
        .await
        .unwrap();
    let grace = api.client(Some(&token(&state).0));
    assert_eq!(grace.whoami().await.unwrap().grant.unwrap().role, "harness");
    grace.list_runs(&RunFilter::default()).await.unwrap();

    let refused = worker.client(Some(&token(&state).0)).whoami().await;
    assert_eq!(
        refused.unwrap_err().problem().unwrap().status,
        401,
        "a worker accepts only API keys"
    );

    let grace_id = owner
        .list_members()
        .await
        .unwrap()
        .members
        .into_iter()
        .find(|member| member.user.email == "grace@example.net")
        .unwrap()
        .user
        .id;
    owner.remove_member(&grace_id).await.unwrap();
    assert_eq!(
        grace
            .list_runs(&RunFilter::default())
            .await
            .unwrap_err()
            .problem()
            .unwrap()
            .status,
        403
    );
    api.stop().await;
    worker.stop().await;
    drop(postgres);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_with_a_nul_character_match_nothing_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let shared = tempfile::tempdir().unwrap();
    let server = TestServer::start_on(
        tempfile::tempdir().unwrap(),
        &postgres.url(),
        &shared.path().join("blobs"),
        &[],
    )
    .await;
    support::nul_lookups_match_nothing(&server).await;
    server.stop().await;
}
