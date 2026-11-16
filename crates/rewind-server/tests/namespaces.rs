//! Namespaces end to end over HTTP, on the engine's in-memory adapters
//! (`archi/06-storage-and-scale.md`, "Namespaces", "Quotas", "Who calls"): an end user's token
//! reaches its namespace only, the customer names the namespace of its requests, quotas refuse
//! what they bound, tokens are revoked, signed tokens are checked, and the endpoints of
//! namespaces, tokens and token keys answer as 07 says.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _, RSA_PKCS1_SHA256,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::StreamExt;
use rewind_api::access::{BuiltInRole, Permission, PutRole};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::events::EventsQuery;
use rewind_api::namespaces::{
    CreateToken, CreateTokenKey, NamespaceListQuery, NamespaceName, NamespaceOrder, NamespaceQuery,
    PutCeiling, PutNamespace, PutNamespaceDefaults, QuotaAlertQuery, QuotaKind, QuotaScope, Quotas,
    TokenAlgorithm,
};
use rewind_api::organization::{Principal, TokenKind};
use rewind_api::problem::QuotaReached;
use rewind_api::reading::ToolStatsFilter;
use rewind_api::runs::{CancelRun, ForkRun, RunFilter, StartRun};
use rewind_api::scripts::{CheckRequest, TestRequest};
use rewind_api::tasks::TaskQuery;
use rewind_api::toolsets::{CreateToolset, Credentials, HostTool};
use rewind_api::{Client, Problem, ProblemType};
use serde_json::{Value, json};
use support::{ADMIN_KEY, TestServer, eventually, wait};

/// A run that waits for a signal: it stays active.
const SIGNAL: &str = r#"import { signal } from "rewind";
export default async function main() {
  const decision = await signal<{ ok: boolean }>("approval");
  return { decision };
}
"#;

/// A run that asks the harness's host tool.
const ASK: &str = r#"import { ask_user } from "rewind:tools";
export default async function main() {
  return { answer: await ask_user({ question: "ready?" }) };
}
"#;

/// One line.
const SHORT: &str = "export default async () => 1;";

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

/// The status and kind of a refusal.
fn refused<T: std::fmt::Debug>(result: Result<T, rewind_api::Error>) -> (u16, Option<ProblemType>) {
    let problem = problem(result.unwrap_err());
    (problem.status, problem.kind())
}

/// One test of `script`, without a world or expectations.
fn scenario(script: &str) -> TestRequest {
    TestRequest {
        script: Some(script.into()),
        tests: vec![rewind_api::testing::TestInput::Inline(Box::new(
            serde_json::from_value(json!({ "name": "runs" })).unwrap(),
        ))],
        ..TestRequest::default()
    }
}

fn ns(name: &str) -> NamespaceName {
    name.parse().unwrap()
}

/// A run of `script`, marked so that each run of a test has its own script.
fn waiting(marker: &str) -> StartRun {
    StartRun::new(format!("// {marker}\n{SIGNAL}"))
}

async fn token(customer: &Client, namespace: &str, create: &CreateToken) -> String {
    customer
        .create_token(&ns(namespace), create)
        .await
        .unwrap()
        .token
        .unwrap()
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

// -------------------------------------------------------------------------------------------
// Keys the customer signs tokens with

enum Signer {
    Ed(Ed25519KeyPair),
    Ec(EcdsaKeyPair),
    Rsa(aws_lc_rs::rsa::KeyPair),
}

impl Signer {
    fn ed25519() -> Self {
        Signer::Ed(Ed25519KeyPair::generate().unwrap())
    }

    fn p256() -> Self {
        Signer::Ec(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
    }

    fn rsa() -> Self {
        Signer::Rsa(aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap())
    }

    fn alg(&self) -> &'static str {
        match self {
            Signer::Ed(_) => "EdDSA",
            Signer::Ec(_) => "ES256",
            Signer::Rsa(_) => "RS256",
        }
    }

    /// The public key, as a JWK.
    fn jwk(&self) -> Value {
        match self {
            Signer::Ed(pair) => json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "x": URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
            }),
            Signer::Ec(pair) => {
                let point = pair.public_key().as_ref();
                json!({
                    "kty": "EC",
                    "crv": "P-256",
                    "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                    "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
                })
            }
            Signer::Rsa(pair) => {
                let public = pair.public_key();
                json!({
                    "kty": "RSA",
                    "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
                    "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
                })
            }
        }
    }

    /// A JWT of `claims`, signed with this key, naming `kid` and `alg`.
    fn sign_as(&self, kid: &str, alg: &str, claims: &Value) -> String {
        let header = json!({ "alg": alg, "kid": kid, "typ": "JWT" });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = match self {
            Signer::Ed(pair) => pair.sign(input.as_bytes()).as_ref().to_vec(),
            Signer::Ec(pair) => pair
                .sign(&SystemRandom::new(), input.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            Signer::Rsa(pair) => {
                let mut signature = vec![0; pair.public_modulus_len()];
                pair.sign(
                    &RSA_PKCS1_SHA256,
                    &SystemRandom::new(),
                    input.as_bytes(),
                    &mut signature,
                )
                .unwrap();
                signature
            }
        };
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn sign(&self, kid: &str, claims: &Value) -> String {
        self.sign_as(kid, self.alg(), claims)
    }
}

/// The claims of a token of `namespace` issued now, for 10 minutes.
fn claims(namespace: &str) -> Value {
    json!({ "sub": namespace, "aud": "rewind", "iat": now(), "exp": now() + 600 })
}

async fn register(
    customer: &Client,
    signer: &Signer,
    permissions: Option<Vec<Permission>>,
) -> String {
    let key = customer
        .create_token_key(&CreateTokenKey {
            name: format!("{} key of the tests", signer.alg()),
            jwk: signer.jwk(),
            permissions,
        })
        .await
        .unwrap();
    assert!(key.kid.starts_with("rk_"), "{}", key.kid);
    key.kid
}

// -------------------------------------------------------------------------------------------
// Isolation

/// Alice's token reaches Alice's runs only: Bob's are missing whatever the request, the lists
/// hold hers, and a header naming another namespace is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_end_user_s_token_reaches_its_namespace_only() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let alice = server.bare(&token(&customer, "alice", &CreateToken::default()).await);
    let bob = server.bare(&token(&customer, "bob", &CreateToken::default()).await);

    let hers = alice.start_run(&waiting("alice"), None).await.unwrap();
    assert_eq!(hers.namespace, ns("alice"));
    let his = bob.start_run(&waiting("bob"), None).await.unwrap();
    assert_eq!(his.namespace, ns("bob"));

    // Bob's run is missing for Alice, whatever she does with it.
    assert_eq!(
        refused(alice.get_run(&his.id, None).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(
            alice
                .signal_run(&his.id, "approval", &json!({ "ok": true }), None)
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(alice.cancel_run(&his.id, &CancelRun::default()).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(alice.fork_run(&his.id, &ForkRun::new(SHORT), None).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(
            alice
                .run_trace(&his.id, rewind_api::runs::TraceFormat::Llm)
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(alice.run_events(&his.id, &EventsQuery::default()).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(alice.get_script(&his.script).await),
        (404, Some(ProblemType::NotFound)),
        "a script her namespace never ran"
    );
    alice.get_script(&hers.script).await.unwrap();
    let her_input = hers.input.clone().unwrap().digest;
    assert_eq!(
        refused(alice.get_payload(&her_input, None).await),
        (403, Some(ProblemType::Forbidden)),
        "a token names the run of a payload"
    );
    assert_eq!(
        refused(alice.get_payload(&her_input, Some(&his.id)).await),
        (404, Some(ProblemType::NotFound))
    );

    // Lists, statistics and scripts hold her namespace only.
    let listed = eventually(|| async {
        let page = alice.list_runs(&RunFilter::default()).await.unwrap();
        (!page.runs.is_empty()).then_some(page)
    })
    .await;
    assert!(listed.runs.iter().all(|run| run.namespace == ns("alice")));
    assert!(listed.runs.iter().all(|run| run.id != his.id));
    let everyone = server.tenant_key("acme", "reader").await;
    eventually(|| async {
        let page = everyone.list_runs(&RunFilter::default()).await.unwrap();
        (page.runs.len() == 2).then_some(())
    })
    .await;
    let scripts = alice
        .list_scripts(&rewind_api::scripts::ScriptFilter::default())
        .await
        .unwrap();
    assert!(
        scripts
            .scripts
            .iter()
            .all(|script| script.digest != his.script)
    );
    let stats = alice
        .run_stats(&rewind_api::runs::RunStatsQuery::default())
        .await
        .unwrap();
    assert_eq!(stats.runs, 1);
    alice.tool_stats(&ToolStatsFilter::default()).await.unwrap();

    // A header naming another namespace is refused; her own may be repeated.
    let astray = alice.with_namespace(ns("bob"));
    assert_eq!(
        refused(astray.list_runs(&RunFilter::default()).await),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(astray.start_run(&StartRun::new(SHORT), None).await),
        (403, Some(ProblemType::Forbidden))
    );
    alice
        .with_namespace(ns("alice"))
        .get_run(&hers.id, None)
        .await
        .unwrap();

    // Her signal reaches her run.
    alice
        .signal_run(&hers.id, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
    let ended = alice.get_run(&hers.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed);

    // Who she is, and what she may do: the default permissions, nothing else.
    let identity = alice.whoami().await.unwrap();
    assert_eq!(identity.tenant.as_deref(), Some("acme"));
    assert_eq!(identity.grant, None);
    assert!(!identity.operator);
    assert_eq!(identity.permissions, Permission::TOKEN_DEFAULT.to_vec());
    let Some(Principal::Token {
        token,
        id,
        namespace,
        kid,
        ..
    }) = identity.principal
    else {
        panic!("not a token: {:?}", identity.principal)
    };
    assert_eq!(
        (token, namespace, kid),
        (TokenKind::Opaque, ns("alice"), None)
    );
    assert_eq!(id.unwrap().len(), 64);
    let missing = problem(alice.delete_run(&hers.id).await.unwrap_err());
    assert_eq!(missing.status, 403);
    assert!(
        missing.detail.contains("`runs:delete`"),
        "{}",
        missing.detail
    );
    assert_eq!(
        refused(alice.list_namespaces(&NamespaceListQuery::default()).await),
        (403, Some(ProblemType::Forbidden)),
        "a token never reads the namespaces"
    );
    assert_eq!(
        refused(
            alice
                .set_toolset_credentials(&hers.toolset, &Credentials::default())
                .await
        ),
        (403, Some(ProblemType::Forbidden)),
        "nor holds the permission of a toolset's credentials"
    );
    server.stop().await;
}

/// A worker acting for an end user claims the tasks of its namespace's runs only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_claims_the_tasks_of_its_namespace_only() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let toolset = customer
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "ask_user".into(),
                description: "Asks the user.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "question": { "type": "string" } },
                    "required": ["question"]
                }),
                output_schema: Some(json!({ "type": "string" })),
                policy: None,
            }],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let mut asking = StartRun::new(ASK);
    asking.toolset = Some(toolset.id.clone());
    let bobs = customer
        .with_namespace(ns("bob"))
        .start_run(&asking, None)
        .await
        .unwrap();
    let alices = customer
        .with_namespace(ns("alice"))
        .start_run(&asking, None)
        .await
        .unwrap();
    let worker = CreateToken {
        permissions: Some(vec![Permission::TasksWork]),
        ..CreateToken::default()
    };
    let alice = server.bare(&token(&customer, "alice", &worker).await);
    let query = TaskQuery {
        toolset: Some(toolset.id.clone()),
        tools: vec!["ask_user".into()],
        limit: Some(10),
        wait: Some("10s".parse().unwrap()),
    };
    let batch = alice.claim_tasks(&query).await.unwrap();
    assert_eq!(batch.tasks.len(), 1, "{batch:?}");
    assert_eq!(batch.tasks[0].run_id, alices.id);
    assert_eq!(batch.tasks[0].namespace, ns("alice"));
    let quick = TaskQuery {
        wait: Some("200ms".parse().unwrap()),
        ..query.clone()
    };
    assert!(alice.claim_tasks(&quick).await.unwrap().tasks.is_empty());

    // Bob's task is the customer's worker's, whose answer Alice's token cannot give.
    let customers_worker = server.tenant_key("acme", "worker").await;
    let theirs = customers_worker.claim_tasks(&query).await.unwrap();
    assert_eq!(theirs.tasks.len(), 1);
    assert_eq!(theirs.tasks[0].run_id, bobs.id);
    assert_eq!(
        refused(alice.heartbeat_task(&theirs.tasks[0].token).await),
        (410, Some(ProblemType::TaskGone))
    );
    alice.heartbeat_task(&batch.tasks[0].token).await.unwrap();
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// The customer names the namespace

/// A key names the namespace to create in, and narrows its reads with it; a fork stays in its
/// parent's namespace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_customer_names_the_namespace_of_its_requests() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let refusal = problem(
        customer
            .start_run(&StartRun::new(SHORT), None)
            .await
            .unwrap_err(),
    );
    assert_eq!(refusal.kind(), Some(ProblemType::InvalidRequest));
    assert!(
        refusal.detail.contains("Rewind-Namespace"),
        "{}",
        refusal.detail
    );
    let check = CheckRequest {
        script: SHORT.into(),
        toolset: None,
    };
    assert_eq!(
        refused(customer.check(&check).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    let test = scenario(SHORT);
    assert_eq!(
        refused(customer.test(&test).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    let alice = customer.with_namespace(ns("alice"));
    let bob = customer.with_namespace(ns("bob"));
    alice.check(&check).await.unwrap();
    alice.test(&test).await.unwrap();

    // A namespace is created at its first use.
    assert_eq!(
        refused(
            customer
                .get_namespace(&ns("alice"), &NamespaceQuery::default())
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    let parent = alice.start_run(&waiting("parent"), None).await.unwrap();
    customer
        .get_namespace(&ns("alice"), &NamespaceQuery::default())
        .await
        .unwrap();
    eventually(|| async {
        let run = alice.get_run(&parent.id, None).await.unwrap();
        (run.history_len > 0).then_some(())
    })
    .await;

    // Another namespace's header misses the run, for a fork, a signal or a cancellation too.
    assert_eq!(
        refused(bob.get_run(&parent.id, None).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(bob.fork_run(&parent.id, &ForkRun::new(SIGNAL), None).await),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(
            bob.signal_run(&parent.id, "approval", &json!({}), None)
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    assert_eq!(
        refused(bob.cancel_run(&parent.id, &CancelRun::default()).await),
        (404, Some(ProblemType::NotFound))
    );

    // A fork takes its parent's namespace, with or without the header.
    let fork = customer
        .fork_run(&parent.id, &ForkRun::new(SIGNAL), None)
        .await
        .unwrap();
    assert_eq!(fork.namespace, ns("alice"));
    let fork = alice
        .fork_run(&parent.id, &ForkRun::new(SIGNAL), None)
        .await
        .unwrap();
    assert_eq!(fork.namespace, ns("alice"));

    // Reads: every namespace without the header, that one with it.
    bob.start_run(&waiting("bob"), None).await.unwrap();
    eventually(|| async {
        let all = customer.list_runs(&RunFilter::default()).await.unwrap();
        let hers = alice.list_runs(&RunFilter::default()).await.unwrap();
        let his = bob.list_runs(&RunFilter::default()).await.unwrap();
        (all.runs.len() == 4 && hers.runs.len() == 3 && his.runs.len() == 1).then_some(())
    })
    .await;
    assert_eq!(
        refused(
            customer
                .with_namespace(ns("bob"))
                .get_payload(&"ab".repeat(32), None)
                .await
        ),
        (403, Some(ProblemType::Forbidden)),
        "a read bound to a namespace names the run of a payload"
    );
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// Quotas

/// `maxScriptLines` and `maxTtl` refuse the request, at start, fork, check and test; the
/// counted quotas refuse a start with 429, and a monthly one says when it frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quotas_refuse_what_they_bound() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    customer
        .put_namespace_defaults(&PutNamespaceDefaults {
            defaults: Quotas {
                max_script_lines: Some(3),
                max_ttl: Some("1h".parse().unwrap()),
                ..Quotas::default()
            },
        })
        .await
        .unwrap();
    let alice = customer.with_namespace(ns("alice"));

    // maxScriptLines: 422 invalid-script with its diagnostic, from start, fork, check and test.
    let too_long = |refusal: Problem| {
        assert_eq!(
            refusal.kind(),
            Some(ProblemType::InvalidScript),
            "{refusal}"
        );
        assert_eq!(refusal.diagnostics[0].code, "rewind/max-script-lines");
    };
    too_long(problem(
        alice
            .start_run(&StartRun::new(SIGNAL), None)
            .await
            .unwrap_err(),
    ));
    too_long(problem(
        alice
            .check(&CheckRequest {
                script: SIGNAL.into(),
                toolset: None,
            })
            .await
            .unwrap_err(),
    ));
    too_long(problem(alice.test(&scenario(SIGNAL)).await.unwrap_err()));
    let mut request = StartRun::new(SHORT);
    request.wait = wait("20s");
    let short = alice.start_run(&request, None).await.unwrap();
    assert_eq!(short.status, RunStatus::Completed);
    too_long(problem(
        alice
            .fork_run(&short.id, &ForkRun::new(SIGNAL), None)
            .await
            .unwrap_err(),
    ));

    // maxTtl: 400, and the default TTL is lowered to it.
    let mut long = StartRun::new(SHORT);
    long.ttl = Some("2h".parse().unwrap());
    assert_eq!(
        refused(alice.start_run(&long, None).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    let mut fork = ForkRun::new(SHORT);
    fork.ttl = Some("2h".parse().unwrap());
    assert_eq!(
        refused(alice.fork_run(&short.id, &fork, None).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    let lifetime = short.expires_at.as_millisecond() - short.created_at.as_millisecond();
    assert!(lifetime <= 3_600_000, "{lifetime} ms");

    // An exception lifts a default for one namespace.
    customer
        .put_namespace(
            &ns("vip"),
            &PutNamespace {
                exceptions: Quotas {
                    max_script_lines: Some(100),
                    ..Quotas::default()
                },
            },
        )
        .await
        .unwrap();
    customer
        .with_namespace(ns("vip"))
        .start_run(&StartRun::new(SIGNAL), None)
        .await
        .unwrap();

    // runsPerMonth: 429 with when it frees, and Retry-After.
    customer
        .put_namespace(
            &ns("monthly"),
            &PutNamespace {
                exceptions: Quotas {
                    runs_per_month: Some(1),
                    ..Quotas::default()
                },
            },
        )
        .await
        .unwrap();
    let monthly = customer.with_namespace(ns("monthly"));
    monthly
        .start_run(&StartRun::new(SHORT), None)
        .await
        .unwrap();
    let raw = reqwest::Client::new()
        .post(format!("{}/v1/runs", server.url))
        .bearer_auth(harness_secret(&server).await)
        .header("Rewind-Namespace", "monthly")
        .json(&json!({ "script": SHORT }))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 429);
    let retry_after: u64 = raw.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        retry_after > 0 && retry_after <= 31 * 86_400,
        "{retry_after}"
    );
    let refusal: Problem = raw.json().await.unwrap();
    let QuotaReached {
        quota,
        scope,
        limit,
        used,
        resets_at,
    } = refusal.quota.clone().unwrap();
    assert_eq!(
        (quota, scope, limit, used),
        (QuotaKind::RunsPerMonth, QuotaScope::Namespace, 1, 1)
    );
    assert!(resets_at.is_some());
    assert!(
        !refusal.detail.contains("monthly"),
        "never names a namespace: {}",
        refusal.detail
    );

    // activeRuns: a blocked end user; no Retry-After, the quota frees when runs end.
    customer
        .put_namespace(
            &ns("blocked"),
            &PutNamespace {
                exceptions: Quotas {
                    active_runs: Some(0),
                    ..Quotas::default()
                },
            },
        )
        .await
        .unwrap();
    let blocked = problem(
        customer
            .with_namespace(ns("blocked"))
            .start_run(&StartRun::new(SHORT), None)
            .await
            .unwrap_err(),
    );
    assert_eq!(blocked.status, 429);
    let reached = blocked.quota.unwrap();
    assert_eq!(
        (reached.quota, reached.resets_at),
        (QuotaKind::ActiveRuns, None)
    );

    // The operator's ceiling bounds the tenant's totals.
    let admin = Client::remote(&server.url, Some(ADMIN_KEY)).unwrap();
    admin
        .put_ceiling(
            "acme",
            &PutCeiling {
                ceiling: Quotas {
                    scripts: Some(1),
                    ..Quotas::default()
                },
            },
        )
        .await
        .unwrap();
    let ceiling = problem(
        customer
            .with_namespace(ns("fresh"))
            .start_run(
                &StartRun::new("// new\nexport default async () => 2;"),
                None,
            )
            .await
            .unwrap_err(),
    );
    let reached = ceiling.quota.unwrap();
    assert_eq!(
        (reached.quota, reached.scope),
        (QuotaKind::Scripts, QuotaScope::Organization)
    );
    let defaults = customer
        .namespace_defaults(&NamespaceQuery::default())
        .await
        .unwrap();
    assert_eq!(defaults.ceiling.scripts, Some(1));
    assert_eq!(defaults.defaults.max_script_lines, Some(3));
    assert!(defaults.usage.runs_started >= 3, "{defaults:?}");
    server.stop().await;
}

/// The secret of a new harness key of `acme`, for requests sent without the client.
async fn harness_secret(server: &TestServer) -> String {
    server
        .admin
        .create_api_key(&rewind_api::access::CreateApiKey {
            tenant: Some("acme".into()),
            name: "raw requests".into(),
            role: BuiltInRole::Harness.as_str().into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap()
        .key
        .unwrap()
}

// -------------------------------------------------------------------------------------------
// Revocation

/// A token revoked one by one, all those of a namespace, a signed token issued before the
/// revocation, a token of a deleted key: each is refused from the next request, and closes the
/// event stream it opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_tokens_are_refused_from_the_next_request() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;

    // One opaque token.
    let minted = customer
        .create_token(&ns("alice"), &CreateToken::default())
        .await
        .unwrap();
    assert_eq!(minted.permissions, Permission::TOKEN_DEFAULT.to_vec());
    let alice = server.bare(minted.token.as_deref().unwrap());
    let run = alice.start_run(&waiting("alice"), None).await.unwrap();
    let query = EventsQuery {
        follow: Some(true),
        ..EventsQuery::default()
    };
    let mut events = alice.run_events(&run.id, &query).await.unwrap();
    assert!(
        events.next().await.is_some(),
        "the stream sends the run's entries"
    );
    customer.revoke_token(&minted.id).await.unwrap();
    customer.revoke_token(&minted.id).await.unwrap();
    let unauthorized = |result: Result<_, rewind_api::Error>| {
        assert_eq!(
            refused::<rewind_api::runs::Run>(result),
            (401, Some(ProblemType::Unauthorized))
        );
    };
    unauthorized(alice.get_run(&run.id, None).await);
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while events.next().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "the stream closes once its token is revoked"
    );
    assert_eq!(
        refused(customer.revoke_token(&"0".repeat(64)).await),
        (404, Some(ProblemType::NotFound))
    );

    // Every token of a namespace: the opaque ones, and the signed ones issued before now.
    let first = server.bare(&token(&customer, "bob", &CreateToken::default()).await);
    let second = server.bare(&token(&customer, "bob", &CreateToken::default()).await);
    let signer = Signer::ed25519();
    let kid = register(&customer, &signer, None).await;
    let mut early = claims("bob");
    early["iat"] = json!(now() - 5);
    let signed = server.bare(&signer.sign(&kid, &early));
    for client in [&first, &second, &signed] {
        client.whoami().await.unwrap();
    }
    let detail = customer
        .get_namespace(&ns("bob"), &NamespaceQuery::default())
        .await
        .unwrap();
    assert_eq!(detail.tokens, 2);
    let revoked = customer.revoke_namespace_tokens(&ns("bob")).await.unwrap();
    assert_eq!(revoked.revoked, 2);
    for client in [&first, &second, &signed] {
        assert_eq!(
            refused(client.whoami().await),
            (401, Some(ProblemType::Unauthorized))
        );
    }
    let mut after = claims("bob");
    after["iat"] = json!(now() + 2);
    server
        .bare(&signer.sign(&kid, &after))
        .whoami()
        .await
        .unwrap();
    let detail = customer
        .get_namespace(&ns("bob"), &NamespaceQuery::default())
        .await
        .unwrap();
    assert_eq!(detail.tokens, 0);
    assert_eq!(detail.tokens_revoked_before, Some(revoked.revoked_before));
    assert_eq!(
        refused(customer.revoke_namespace_tokens(&ns("nobody")).await),
        (404, Some(ProblemType::NotFound))
    );

    // A deleted key: every token it signed.
    let carol = server.bare(&signer.sign(&kid, &claims("carol")));
    carol.whoami().await.unwrap();
    customer.delete_token_key(&kid).await.unwrap();
    assert_eq!(
        refused(carol.whoami().await),
        (401, Some(ProblemType::Unauthorized))
    );
    assert_eq!(
        refused(customer.delete_token_key(&kid).await),
        (404, Some(ProblemType::NotFound))
    );

    // An expired token.
    let brief = CreateToken {
        ttl: Some("1s".parse().unwrap()),
        ..CreateToken::default()
    };
    let brief = server.bare(&token(&customer, "dave", &brief).await);
    brief.whoami().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        refused(brief.whoami().await),
        (401, Some(ProblemType::Unauthorized))
    );
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// Signed tokens

/// Tokens signed with Ed25519, P-256 and RSA keys the customer registered: their claims are
/// checked, and their namespace is created at their first use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn customers_sign_tokens_with_the_keys_they_registered() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    for (signer, algorithm) in [
        (Signer::ed25519(), TokenAlgorithm::EdDsa),
        (Signer::p256(), TokenAlgorithm::Es256),
        (Signer::rsa(), TokenAlgorithm::Rs256),
    ] {
        let kid = register(
            &customer,
            &signer,
            Some(vec![Permission::RunsRead, Permission::RunsStart]),
        )
        .await;
        let listed = customer.list_token_keys().await.unwrap();
        let key = listed.keys.iter().find(|key| key.kid == kid).unwrap();
        assert_eq!(key.algorithm, algorithm);
        assert_eq!(
            key.permissions,
            [Permission::RunsRead, Permission::RunsStart]
        );

        let namespace = format!("user-{}", signer.alg());
        let mut claims = claims(&namespace);
        claims["jti"] = json!("j-1");
        claims["labels"] = json!({ "agent": "triage" });
        let client = server.bare(&signer.sign(&kid, &claims));
        let identity = client.whoami().await.unwrap();
        let Some(Principal::Token {
            token,
            kid: signed_by,
            jti,
            labels,
            ..
        }) = identity.principal
        else {
            panic!("not a token: {:?}", identity.principal)
        };
        assert_eq!(token, TokenKind::Signed);
        assert_eq!(signed_by.as_deref(), Some(kid.as_str()));
        assert_eq!(jti.as_deref(), Some("j-1"));
        assert_eq!(labels, Labels::from_iter([("agent", "triage")]));
        assert_eq!(
            identity.permissions,
            [Permission::RunsRead, Permission::RunsStart],
            "without permissions, the defaults its key may give"
        );
        customer
            .get_namespace(&ns(&namespace), &NamespaceQuery::default())
            .await
            .unwrap();
        let run = client.start_run(&StartRun::new(SHORT), None).await.unwrap();
        assert_eq!(run.namespace, ns(&namespace));
        assert_eq!(run.labels, Labels::from_iter([("agent", "triage")]));

        // Refused: a permission the key may not give, another audience, a life above 30 days,
        // an expiry past, an algorithm the key does not sign with, a forged signature.
        let unauthorized = |token: String| {
            let client = server.bare(&token);
            async move {
                assert_eq!(
                    refused(client.whoami().await),
                    (401, Some(ProblemType::Unauthorized))
                );
            }
        };
        let mut more = claims.clone();
        more["permissions"] = json!(["runs:read", "runs:delete"]);
        unauthorized(signer.sign(&kid, &more)).await;
        let mut audience = claims.clone();
        audience["aud"] = json!("someone-else");
        unauthorized(signer.sign(&kid, &audience)).await;
        let mut long = claims.clone();
        long["exp"] = json!(now() + 31 * 86_400);
        unauthorized(signer.sign(&kid, &long)).await;
        let mut expired = claims.clone();
        expired["iat"] = json!(now() - 3_600);
        expired["exp"] = json!(now() - 600);
        unauthorized(signer.sign(&kid, &expired)).await;
        let other = if signer.alg() == "EdDSA" {
            "ES256"
        } else {
            "EdDSA"
        };
        unauthorized(signer.sign_as(&kid, other, &claims)).await;
        unauthorized(Signer::ed25519().sign(&kid, &claims)).await;
        let mut sub = claims.clone();
        sub["sub"] = json!("not a namespace");
        unauthorized(signer.sign(&kid, &sub)).await;
    }
    assert_eq!(
        refused(
            server
                .bare(&Signer::ed25519().sign("rk_unknown", &claims("x")))
                .whoami()
                .await
        ),
        (401, Some(ProblemType::Unauthorized))
    );
    server.stop().await;
}

/// Who mints a token or registers a key gives only what it holds, among what a token may hold;
/// a key is public.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_and_keys_give_within_what_their_author_holds() {
    let server = TestServer::in_memory(&[]).await;
    let admin = Client::remote(&server.url, Some(ADMIN_KEY)).unwrap();
    admin
        .put_role(
            "minter",
            &PutRole {
                description: "Mints read-only tokens.".into(),
                permissions: vec![Permission::RunsRead, Permission::TokensWrite],
            },
        )
        .await
        .unwrap();
    let minter = server.tenant_key("acme", "minter").await;
    let given = minter
        .create_token(&ns("alice"), &CreateToken::default())
        .await
        .unwrap();
    assert_eq!(
        given.permissions,
        [Permission::RunsRead],
        "the defaults its author holds"
    );
    let more = CreateToken {
        permissions: Some(vec![Permission::RunsRead, Permission::RunsStart]),
        ..CreateToken::default()
    };
    assert_eq!(
        refused(minter.create_token(&ns("alice"), &more).await),
        (403, Some(ProblemType::Forbidden))
    );
    let admin_token = CreateToken {
        permissions: Some(vec![Permission::NamespacesRead]),
        ..CreateToken::default()
    };
    assert_eq!(
        refused(admin.create_token(&ns("alice"), &admin_token).await),
        (400, Some(ProblemType::InvalidRequest)),
        "a token never holds an administration permission"
    );
    let forever = CreateToken {
        ttl: Some("31d".parse().unwrap()),
        ..CreateToken::default()
    };
    assert_eq!(
        refused(admin.create_token(&ns("alice"), &forever).await),
        (400, Some(ProblemType::InvalidRequest))
    );
    let signer = Signer::ed25519();
    let key = minter
        .create_token_key(&CreateTokenKey {
            name: "read-only".into(),
            jwk: signer.jwk(),
            permissions: None,
        })
        .await
        .unwrap();
    assert_eq!(
        key.permissions,
        [Permission::RunsRead],
        "what its author holds"
    );
    let mut private = signer.jwk();
    private["d"] = json!(URL_SAFE_NO_PAD.encode([1u8; 32]));
    let refusal = problem(
        minter
            .create_token_key(&CreateTokenKey {
                name: "leaked".into(),
                jwk: private,
                permissions: None,
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refusal.kind(), Some(ProblemType::InvalidRequest));
    assert!(
        refusal.detail.contains("private part"),
        "{}",
        refusal.detail
    );
    assert_eq!(
        refused(
            minter
                .create_token_key(&CreateTokenKey {
                    name: "too much".into(),
                    jwk: signer.jwk(),
                    permissions: Some(vec![Permission::RunsDelete]),
                })
                .await
        ),
        (403, Some(ProblemType::Forbidden))
    );
    let reader = server.tenant_key("acme", "reader").await;
    assert_eq!(
        refused(reader.list_token_keys().await),
        (403, Some(ProblemType::Forbidden)),
        "the keys need tokens:write"
    );
    assert_eq!(
        refused(
            reader
                .create_token(&ns("alice"), &CreateToken::default())
                .await
        ),
        (403, Some(ProblemType::Forbidden))
    );
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// The endpoints of namespaces

/// The list with its search, order and filter of the namespaces above 80 %, a namespace's
/// detail, its exceptions, the defaults and the alerts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn namespaces_are_listed_with_their_use_and_alerts() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let reader = server.tenant_key("acme", "reader").await;
    let busy = PutNamespace {
        exceptions: Quotas {
            runs_per_month: Some(2),
            ..Quotas::default()
        },
    };
    let created = customer
        .put_namespace(&ns("team:busy"), &busy)
        .await
        .unwrap();
    assert_eq!(created.namespace.exceptions.runs_per_month, Some(2));
    assert_eq!(created.namespace.limits.runs_per_month, Some(2));
    for marker in ["one", "two"] {
        customer
            .with_namespace(ns("team:busy"))
            .start_run(&StartRun::new(format!("// {marker}\n{SHORT}")), None)
            .await
            .unwrap();
    }
    customer
        .with_namespace(ns("team:idle"))
        .start_run(&StartRun::new(SHORT), None)
        .await
        .unwrap();
    customer
        .put_namespace(&ns("solo"), &PutNamespace::default())
        .await
        .unwrap();

    let all = reader
        .list_namespaces(&NamespaceListQuery::default())
        .await
        .unwrap();
    assert_eq!(all.namespaces.len(), 3);
    assert_eq!(all.total, 3);
    let by_name = NamespaceListQuery {
        order: Some(NamespaceOrder::Name),
        ..NamespaceListQuery::default()
    };
    let names: Vec<String> = reader
        .list_namespaces(&by_name)
        .await
        .unwrap()
        .namespaces
        .iter()
        .map(|namespace| namespace.name.to_string())
        .collect();
    assert_eq!(names, ["solo", "team:busy", "team:idle"]);
    let by_runs = NamespaceListQuery {
        order: Some(NamespaceOrder::Runs),
        limit: Some(1),
        ..NamespaceListQuery::default()
    };
    let first = reader.list_namespaces(&by_runs).await.unwrap();
    assert_eq!(first.namespaces[0].name, ns("team:busy"));
    assert_eq!(first.namespaces[0].usage.runs_started, 2);
    assert_eq!(first.total, 3, "the total counts every page");
    let second = reader
        .list_namespaces(&NamespaceListQuery {
            after: first.next.clone(),
            ..by_runs.clone()
        })
        .await
        .unwrap();
    assert_eq!(second.namespaces[0].name, ns("team:idle"));
    let prefixed = reader
        .list_namespaces(&NamespaceListQuery {
            prefix: Some("team:".into()),
            ..NamespaceListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(prefixed.namespaces.len(), 2);
    assert_eq!(prefixed.total, 2);
    let above = reader
        .list_namespaces(&NamespaceListQuery {
            above: Some(0.8),
            ..NamespaceListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(above.namespaces.len(), 1);
    assert_eq!(above.total, 1);
    assert_eq!(above.namespaces[0].name, ns("team:busy"));
    let narrowed = reader
        .with_namespace(ns("team:idle"))
        .list_namespaces(&NamespaceListQuery::default())
        .await
        .unwrap();
    assert_eq!(narrowed.namespaces.len(), 1);
    assert_eq!(narrowed.total, 1, "a namespace bound counts itself alone");
    assert_eq!(narrowed.namespaces[0].name, ns("team:idle"));
    let busy_alone = reader
        .with_namespace(ns("team:busy"))
        .list_namespaces(&NamespaceListQuery {
            prefix: Some("team:".into()),
            ..NamespaceListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(busy_alone.total, 1);
    assert_eq!(
        refused(
            reader
                .list_namespaces(&NamespaceListQuery {
                    month: Some("November".into()),
                    ..NamespaceListQuery::default()
                })
                .await
        ),
        (400, Some(ProblemType::InvalidRequest))
    );

    // One namespace: its days, its alerts (80 % then 100 % of runsPerMonth), its tokens.
    customer
        .create_token(&ns("team:busy"), &CreateToken::default())
        .await
        .unwrap();
    let detail = reader
        .get_namespace(&ns("team:busy"), &NamespaceQuery::default())
        .await
        .unwrap();
    assert_eq!(detail.days.len(), 1);
    assert_eq!(detail.days[0].runs_started, 2);
    assert_eq!(detail.tokens, 1);
    let thresholds: Vec<u8> = detail.alerts.iter().map(|alert| alert.threshold).collect();
    assert_eq!(thresholds.len(), 2, "{:?}", detail.alerts);
    assert!(thresholds.contains(&80) && thresholds.contains(&100));
    let alerts = reader
        .quota_alerts(&QuotaAlertQuery::default())
        .await
        .unwrap();
    assert_eq!(alerts.alerts.len(), 2);
    assert!(
        alerts
            .alerts
            .iter()
            .all(|alert| alert.namespace == Some(ns("team:busy")))
    );
    let theirs = reader
        .quota_alerts(&QuotaAlertQuery {
            namespace: Some(ns("solo")),
            ..QuotaAlertQuery::default()
        })
        .await
        .unwrap();
    assert!(theirs.alerts.is_empty());
    assert!(
        reader
            .with_namespace(ns("solo"))
            .quota_alerts(&QuotaAlertQuery::default())
            .await
            .unwrap()
            .alerts
            .is_empty(),
        "a read bound to a namespace holds its alerts"
    );

    // Exceptions are replaced: a quota absent follows the default again.
    customer
        .put_namespace_defaults(&PutNamespaceDefaults {
            defaults: Quotas {
                runs_per_month: Some(1000),
                ..Quotas::default()
            },
        })
        .await
        .unwrap();
    let reset = customer
        .put_namespace(&ns("team:busy"), &PutNamespace::default())
        .await
        .unwrap();
    assert_eq!(reset.namespace.exceptions, Quotas::default());
    assert_eq!(reset.namespace.limits.runs_per_month, Some(1000));

    // namespaces:read and namespaces:write.
    assert_eq!(
        refused(
            reader
                .put_namespace(&ns("solo"), &PutNamespace::default())
                .await
        ),
        (403, Some(ProblemType::Forbidden))
    );
    let worker = server.tenant_key("acme", "worker").await;
    assert_eq!(
        refused(worker.list_namespaces(&NamespaceListQuery::default()).await),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(
            reader
                .put_namespace_defaults(&PutNamespaceDefaults::default())
                .await
        ),
        (403, Some(ProblemType::Forbidden))
    );
    assert_eq!(
        refused(customer.put_ceiling("acme", &PutCeiling::default()).await),
        (403, Some(ProblemType::Forbidden)),
        "the ceiling is the operator's"
    );
    let admin = Client::remote(&server.url, Some(ADMIN_KEY)).unwrap();
    assert_eq!(
        refused(admin.put_ceiling("nobody", &PutCeiling::default()).await),
        (404, Some(ProblemType::NotFound))
    );
    server.stop().await;
}

/// Erasing a namespace answers 202: it takes no run, no token and no credentials any more, its
/// tokens are refused and its active runs cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_erased_namespace_takes_nothing_more() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let alice = customer.with_namespace(ns("alice"));
    let run = alice.start_run(&waiting("alice"), None).await.unwrap();
    let hers = server.bare(&token(&customer, "alice", &CreateToken::default()).await);
    hers.whoami().await.unwrap();

    customer.delete_namespace(&ns("alice")).await.unwrap();
    let cancelled = customer.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    assert_eq!(
        refused(hers.whoami().await),
        (401, Some(ProblemType::Unauthorized))
    );
    let deleting = |result: Result<_, rewind_api::Error>| {
        assert_eq!(
            refused::<()>(result.map(|_: ()| ())),
            (409, Some(ProblemType::NamespaceDeleting))
        );
    };
    deleting(
        alice
            .start_run(&StartRun::new(SHORT), None)
            .await
            .map(|_| ()),
    );
    deleting(
        customer
            .create_token(&ns("alice"), &CreateToken::default())
            .await
            .map(|_| ()),
    );
    deleting(
        customer
            .put_namespace(&ns("alice"), &PutNamespace::default())
            .await
            .map(|_| ()),
    );
    deleting(
        customer
            .set_namespace_credentials(&ns("alice"), &run.toolset, &Credentials::default())
            .await,
    );
    let detail = customer
        .get_namespace(&ns("alice"), &NamespaceQuery::default())
        .await
        .unwrap();
    assert!(detail.namespace.deleting_since.is_some());
    customer.delete_namespace(&ns("alice")).await.unwrap();
    assert_eq!(
        refused(customer.delete_namespace(&ns("nobody")).await),
        (404, Some(ProblemType::NotFound))
    );
    server.stop().await;
}

/// An end user's credentials for a toolset: the customer sets any namespace's, a token its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_are_set_per_end_user() {
    let server = TestServer::in_memory(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let run = customer
        .with_namespace(ns("alice"))
        .start_run(&StartRun::new(SHORT), None)
        .await
        .unwrap();
    customer
        .set_namespace_credentials(&ns("alice"), &run.toolset, &Credentials::default())
        .await
        .unwrap();
    assert_eq!(
        refused(
            customer
                .set_namespace_credentials(&ns("alice"), &"ab".repeat(32), &Credentials::default())
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    let writer = CreateToken {
        permissions: Some(vec![Permission::CredentialsWrite]),
        ..CreateToken::default()
    };
    let alice = server.bare(&token(&customer, "alice", &writer).await);
    alice
        .set_namespace_credentials(&ns("alice"), &run.toolset, &Credentials::default())
        .await
        .unwrap();
    assert_eq!(
        refused(
            alice
                .set_namespace_credentials(&ns("bob"), &run.toolset, &Credentials::default())
                .await
        ),
        (404, Some(ProblemType::NotFound))
    );
    let reader = server.tenant_key("acme", "reader").await;
    assert_eq!(
        refused(
            reader
                .set_namespace_credentials(&ns("alice"), &run.toolset, &Credentials::default())
                .await
        ),
        (403, Some(ProblemType::Forbidden))
    );
    server.stop().await;
}
