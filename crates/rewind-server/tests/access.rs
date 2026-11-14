//! Keys and permissions end to end (`archi/06-storage-and-scale.md`, "What the caller may do";
//! `archi/07-code-architecture.md`, "HTTP API `/v1`"): every endpoint requires the permission of
//! 07's table, a key is given within what its creator holds, a grant limited to labels does not
//! administer, and the operator reaches the keys of every tenant.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use rewind_api::access::{BuiltInRole, CreateApiKey, OPERATOR, Permission};
use rewind_api::common::Labels;
use rewind_api::organization::Principal as CallerPrincipal;
use rewind_api::reading::ToolStatsFilter;
use rewind_api::runs::{CancelRun, ForkRun, RunFilter};
use rewind_api::scripts::{CheckRequest, ScriptFilter};
use rewind_api::tasks::TaskQuery;
use rewind_api::toolsets::{CreateToolset, Credentials};
use rewind_api::{Client, Problem, ProblemType};
use rewind_core::Timestamp;
use rewind_engine::ports::{AccessStore, Role, TenantId};
use rewind_store_turso::{MasterKey, TursoStore};
use support::TestServer;

/// A run id no run has.
const UNKNOWN_RUN: &str = "01930000-0000-7000-8000-00000000abcd";

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

/// A server whose store holds these roles of the tenant `acme` before it starts.
async fn server_with_roles(
    roles: &[(String, BTreeSet<String>)],
) -> (TestServer, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    {
        let store = TursoStore::open(&path, MasterKey::from_bytes([9; 32]))
            .await
            .unwrap();
        for (name, permissions) in roles {
            let role = Role {
                tenant: TenantId::new("acme").unwrap(),
                name: name.clone(),
                description: format!("The role {name} of the tests."),
                permissions: permissions.clone(),
                created_at: Timestamp(0),
                updated_at: Timestamp(0),
            };
            store.put_role(&role).await.unwrap();
        }
    }
    let store = format!("turso:{}", path.display());
    let server = TestServer::start_in(dir.path(), &store, &dir.path().join("blobs"), &[]).await;
    (server, dir)
}

fn create(tenant: Option<&str>, name: &str, role: &str, labels: &[(&str, &str)]) -> CreateApiKey {
    CreateApiKey {
        tenant: tenant.map(str::to_owned),
        name: name.to_owned(),
        role: role.to_owned(),
        allowed_labels: labels.iter().copied().collect::<Labels>(),
    }
}

/// A representative request of each group of 07's table, each requiring one permission.
#[derive(Clone, Copy, Debug)]
enum Call {
    ListToolsets,
    CreateToolset,
    SetToolsetCredentials,
    Check,
    ListRuns,
    ForkRun,
    CancelRun,
    ReleaseRun,
    GiveUpRun,
    SetRunCredentials,
    DeleteRun,
    Heartbeat,
    ApproveTask,
    ListSubscriptions,
    DeleteSubscription,
    ListScripts,
    ToolStats,
    ListApiKeys,
    RevokeApiKey,
}

const CALLS: [Call; 19] = [
    Call::ListToolsets,
    Call::CreateToolset,
    Call::SetToolsetCredentials,
    Call::Check,
    Call::ListRuns,
    Call::ForkRun,
    Call::CancelRun,
    Call::ReleaseRun,
    Call::GiveUpRun,
    Call::SetRunCredentials,
    Call::DeleteRun,
    Call::Heartbeat,
    Call::ApproveTask,
    Call::ListSubscriptions,
    Call::DeleteSubscription,
    Call::ListScripts,
    Call::ToolStats,
    Call::ListApiKeys,
    Call::RevokeApiKey,
];

/// The status of an answer, and the detail of a problem.
fn outcome<T>(result: Result<T, rewind_api::Error>) -> (u16, String) {
    match result {
        Ok(_) => (200, String::new()),
        Err(error) => {
            let problem = problem(error);
            (problem.status, problem.detail)
        }
    }
}

impl Call {
    /// The permission of 07's table.
    fn permission(self) -> Permission {
        match self {
            Call::ListToolsets => Permission::ToolsetsRead,
            Call::CreateToolset => Permission::ToolsetsWrite,
            Call::SetToolsetCredentials | Call::SetRunCredentials => Permission::CredentialsWrite,
            Call::Check | Call::ForkRun => Permission::RunsStart,
            Call::ListRuns | Call::ListScripts | Call::ToolStats => Permission::RunsRead,
            Call::CancelRun => Permission::RunsControl,
            Call::ReleaseRun | Call::GiveUpRun => Permission::RunsQuarantine,
            Call::DeleteRun => Permission::RunsDelete,
            Call::Heartbeat => Permission::TasksWork,
            Call::ApproveTask => Permission::TasksApprove,
            Call::ListSubscriptions => Permission::SubscriptionsRead,
            Call::DeleteSubscription => Permission::SubscriptionsWrite,
            Call::ListApiKeys => Permission::ApiKeysRead,
            Call::RevokeApiKey => Permission::ApiKeysWrite,
        }
    }

    /// Sends the request. Allowed, each answers without changing anything that matters here:
    /// unknown ids are not found, and the empty toolset is registered once.
    async fn on(self, client: &Client) -> (u16, String) {
        let credentials = Credentials {
            servers: BTreeMap::new(),
        };
        match self {
            Call::ListToolsets => outcome(client.list_toolsets().await),
            Call::CreateToolset => outcome(client.create_toolset(&CreateToolset::default()).await),
            Call::SetToolsetCredentials => outcome(
                client
                    .set_toolset_credentials("unknown", &credentials)
                    .await,
            ),
            Call::Check => outcome(
                client
                    .check(&CheckRequest {
                        script: "export default async function main() {}".into(),
                        toolset: Some("unknown".into()),
                    })
                    .await,
            ),
            Call::ListRuns => outcome(client.list_runs(&RunFilter::default()).await),
            Call::ForkRun => outcome(
                client
                    .fork_run(
                        UNKNOWN_RUN,
                        &ForkRun::new("export default async () => 1"),
                        None,
                    )
                    .await,
            ),
            Call::CancelRun => outcome(client.cancel_run(UNKNOWN_RUN, &CancelRun::default()).await),
            Call::ReleaseRun => outcome(client.release_run(UNKNOWN_RUN).await),
            Call::GiveUpRun => outcome(client.give_up_run(UNKNOWN_RUN).await),
            Call::SetRunCredentials => {
                outcome(client.set_run_credentials(UNKNOWN_RUN, &credentials).await)
            }
            Call::DeleteRun => outcome(client.delete_run(UNKNOWN_RUN).await),
            Call::Heartbeat => outcome(client.heartbeat_task("unknown").await),
            Call::ApproveTask => outcome(client.approve_task("unknown").await),
            Call::ListSubscriptions => outcome(client.list_subscriptions().await),
            Call::DeleteSubscription => outcome(client.delete_subscription("unknown").await),
            Call::ListScripts => outcome(client.list_scripts(&ScriptFilter::default()).await),
            Call::ToolStats => outcome(client.tool_stats(&ToolStatsFilter::default()).await),
            Call::ListApiKeys => outcome(client.list_api_keys().await),
            Call::RevokeApiKey => outcome(client.revoke_api_key("unknown").await),
        }
    }
}

/// The role that gives every permission but `permission`.
fn without(permission: Permission) -> String {
    format!("without-{}", permission.as_str().replace(':', "-"))
}

/// Each request is refused (403, naming the permission) to a key whose role gives every
/// permission but the one of 07's table, and answered to the keys that lack another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_endpoint_requires_its_permission() {
    let guarded: BTreeSet<Permission> = CALLS.iter().map(|call| call.permission()).collect();
    let roles: Vec<(String, BTreeSet<String>)> = guarded
        .iter()
        .map(|&lacking| {
            let permissions = Permission::ALL
                .into_iter()
                .filter(|permission| *permission != lacking)
                .map(|permission| permission.as_str().to_owned())
                .collect();
            (without(lacking), permissions)
        })
        .collect();
    let (server, _dir) = server_with_roles(&roles).await;
    let mut keys = BTreeMap::new();
    for &lacking in &guarded {
        keys.insert(
            lacking,
            server.key_with("acme", &without(lacking), &[]).await,
        );
    }
    for call in CALLS {
        for (&lacking, client) in &keys {
            let (status, detail) = call.on(client).await;
            if lacking == call.permission() {
                assert_eq!(status, 403, "{call:?} without {lacking}: {detail}");
                assert!(
                    detail.contains(&format!("`{lacking}`")),
                    "{call:?}: the answer names the permission: {detail}"
                );
            } else {
                assert_ne!(status, 403, "{call:?} without {lacking}: {detail}");
            }
        }
    }
    server.stop().await;
}

/// A key is given and taken away only within what its creator holds; revoked, it is refused and
/// listed without a grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_are_given_within_what_their_creator_holds() {
    let key_admin: BTreeSet<String> = ["api_keys:read", "api_keys:write", "runs:read"]
        .map(String::from)
        .into();
    let (server, _dir) = server_with_roles(&[("key-admin".to_owned(), key_admin)]).await;
    let owner = server.key(BuiltInRole::Owner, &[]).await;
    let owner_id = match owner.whoami().await.unwrap().principal {
        Some(CallerPrincipal::Key { id }) => id,
        other => panic!("a key's principal is the key: {other:?}"),
    };

    // An owner creates the keys of its tenant, its own role included.
    let harness = owner
        .create_api_key(&create(
            None,
            "triage agent",
            "harness",
            &[("agent", "triage")],
        ))
        .await
        .unwrap();
    assert_eq!(
        (
            harness.tenant.as_str(),
            harness.name.as_str(),
            harness.role.as_str()
        ),
        ("acme", "triage agent", "harness")
    );
    assert_eq!(
        harness.allowed_labels,
        Labels::from_iter([("agent", "triage")])
    );
    assert_eq!(harness.created_by, Some(format!("key:{owner_id}")));
    assert_eq!(harness.revoked_at, None);
    assert!(
        harness
            .key
            .as_deref()
            .is_some_and(|key| key.starts_with("rewind_sk_"))
    );
    let harness_client = server.client(harness.key.as_deref());
    let identity = harness_client.whoami().await.unwrap();
    assert_eq!(identity.tenant.as_deref(), Some("acme"));
    assert_eq!(
        identity.principal,
        Some(CallerPrincipal::Key {
            id: harness.id.clone()
        })
    );
    assert_eq!(identity.permissions, BuiltInRole::Harness.permissions());
    assert!(!identity.operator);
    let grant = identity.grant.unwrap();
    assert_eq!(grant.role, "harness");
    assert_eq!(grant.granted_by, Some(format!("key:{owner_id}")));
    assert!(grant.granted_at.is_some());
    owner
        .create_api_key(&create(None, "second owner", "owner", &[]))
        .await
        .unwrap();

    for (request, kind, said) in [
        (
            create(None, "operator", OPERATOR, &[]),
            ProblemType::Forbidden,
            "only the deployment's operator",
        ),
        (
            create(Some("beta"), "elsewhere", "harness", &[]),
            ProblemType::Forbidden,
            "another tenant",
        ),
        (
            create(None, "nobody", "no-such-role", &[]),
            ProblemType::InvalidRequest,
            "no role \"no-such-role\"",
        ),
        (
            create(None, "  ", "reader", &[]),
            ProblemType::InvalidRequest,
            "a key needs a name",
        ),
    ] {
        let refused = problem(owner.create_api_key(&request).await.unwrap_err());
        assert_eq!(refused.kind(), Some(kind), "{}", refused.detail);
        assert!(refused.detail.contains(said), "{}", refused.detail);
    }

    // Listed oldest first, with their grants and without their secrets: the operator's first.
    let listed = owner.list_api_keys().await.unwrap().keys;
    assert_eq!(
        (listed[0].name.as_str(), listed[0].role.as_str()),
        ("operator key (--admin-key)", "operator")
    );
    assert_eq!(listed[0].created_by, None);
    assert!(listed.iter().all(|key| key.key.is_none()));
    let listed_harness = listed.iter().find(|key| key.id == harness.id).unwrap();
    assert_eq!(
        (
            listed_harness.role.as_str(),
            &listed_harness.allowed_labels,
            &listed_harness.created_by
        ),
        ("harness", &harness.allowed_labels, &harness.created_by)
    );

    // A role of the tenant gives only what it holds, and takes away only that.
    let key_admin = server.key_with("acme", "key-admin", &[]).await;
    for (role, lacking) in [("reader", "`toolsets:read`"), ("worker", "`tasks:work`")] {
        let refused = problem(
            key_admin
                .create_api_key(&create(None, role, role, &[]))
                .await
                .unwrap_err(),
        );
        assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
        assert!(refused.detail.contains(lacking), "{}", refused.detail);
    }
    let peer = key_admin
        .create_api_key(&create(None, "peer", "key-admin", &[]))
        .await
        .unwrap();
    assert_eq!(peer.role, "key-admin");
    let refused = problem(key_admin.revoke_api_key(&harness.id).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
    assert!(
        refused.detail.contains("`runs:start`"),
        "{}",
        refused.detail
    );
    key_admin.revoke_api_key(&peer.id).await.unwrap();

    // Revoked: refused from the next request, listed without a grant, not revoked twice.
    owner.revoke_api_key(&harness.id).await.unwrap();
    assert_eq!(
        problem(harness_client.whoami().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    let revoked = owner
        .list_api_keys()
        .await
        .unwrap()
        .keys
        .into_iter()
        .find(|key| key.id == harness.id)
        .unwrap();
    assert!(revoked.revoked_at.is_some());
    assert_eq!(revoked.role, "", "its grant is gone");
    assert_eq!(
        problem(owner.revoke_api_key(&harness.id).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );

    // Only the operator revokes the operator's key; a harness administers no key.
    let refused = problem(owner.revoke_api_key(&listed[0].id).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
    assert!(refused.detail.contains("operator"), "{}", refused.detail);
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let refused = problem(harness.list_api_keys().await.unwrap_err());
    assert!(
        refused.detail.contains("`api_keys:read`"),
        "{}",
        refused.detail
    );
    let refused = problem(
        harness
            .create_api_key(&create(None, "worker", "worker", &[]))
            .await
            .unwrap_err(),
    );
    assert!(
        refused.detail.contains("`api_keys:write`"),
        "{}",
        refused.detail
    );
    server.stop().await;
}

/// A grant limited to labels sees its runs but administers nothing, claims no task and reads no
/// tool statistics.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_limited_to_labels_does_not_administer() {
    let server = TestServer::start(&[]).await;
    let scoped = server.key(BuiltInRole::Owner, &[("team", "support")]).await;
    let identity = scoped.whoami().await.unwrap();
    assert_eq!(identity.permissions, Permission::ALL);
    assert_eq!(
        identity.grant.unwrap().allowed_labels,
        Labels::from_iter([("team", "support")])
    );
    scoped.list_runs(&RunFilter::default()).await.unwrap();
    scoped.list_permissions().await.unwrap();

    let refusals = [
        outcome(scoped.list_api_keys().await),
        outcome(
            scoped
                .create_api_key(&create(None, "reader", "reader", &[]))
                .await,
        ),
        outcome(scoped.revoke_api_key("unknown").await),
    ];
    for (status, detail) in refusals {
        assert_eq!(status, 403, "{detail}");
        assert!(detail.contains("limited to labels"), "{detail}");
    }
    let (status, detail) = outcome(scoped.tool_stats(&ToolStatsFilter::default()).await);
    assert_eq!(status, 403, "{detail}");
    let claim = TaskQuery {
        wait: Some("1s".parse().unwrap()),
        ..TaskQuery::default()
    };
    let (status, detail) = outcome(scoped.claim_tasks(&claim).await);
    assert_eq!(status, 403, "{detail}");
    server.stop().await;
}

/// The operator holds every permission, makes and revokes the keys of every tenant, and alone
/// stops the server; an owner reaches only its own tenant's keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_operator_reaches_the_keys_of_every_tenant() {
    let server = TestServer::start(&[]).await;
    let me = server.admin.whoami().await.unwrap();
    assert!(me.operator);
    assert_eq!(me.tenant.as_deref(), Some("acme"));
    assert_eq!(me.permissions, Permission::ALL);
    assert_eq!(me.grant.unwrap().role, OPERATOR);

    // Another tenant's keys, the tenant created with its first.
    let made = server
        .admin
        .create_api_key(&create(Some("beta"), "beta's owner", "owner", &[]))
        .await
        .unwrap();
    assert_eq!(made.tenant, "beta");
    let beta = server.client(made.key.as_deref());
    let beta_keys: Vec<String> = beta
        .list_api_keys()
        .await
        .unwrap()
        .keys
        .into_iter()
        .map(|key| key.id)
        .collect();
    assert_eq!(
        beta_keys,
        std::slice::from_ref(&made.id),
        "a tenant lists its own keys"
    );

    // An owner of acme neither sees nor revokes them.
    let owner = server.key(BuiltInRole::Owner, &[]).await;
    assert!(
        owner
            .list_api_keys()
            .await
            .unwrap()
            .keys
            .iter()
            .all(|key| key.tenant == "acme")
    );
    assert_eq!(
        problem(owner.revoke_api_key(&made.id).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );

    // An operator key, from the operator only, is the operator too.
    let second = server
        .admin
        .create_api_key(&create(None, "second operator", OPERATOR, &[]))
        .await
        .unwrap();
    assert!(
        server
            .client(second.key.as_deref())
            .whoami()
            .await
            .unwrap()
            .operator
    );

    server.admin.revoke_api_key(&made.id).await.unwrap();
    assert_eq!(
        problem(beta.whoami().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );

    // Stopping is the operator's: an owner is refused as not the operator (and this server,
    // not embedded, refuses the operator too).
    let refused = problem(owner.stop_server().await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
    assert!(refused.detail.contains("operator"), "{}", refused.detail);

    // The catalog, to any caller.
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    assert_eq!(
        worker.list_permissions().await.unwrap().permissions.len(),
        Permission::ALL.len()
    );
    server.stop().await;
}
