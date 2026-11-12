//! The `http` tool against servers on 127.0.0.1. These tests treat 127.0.0.1 as a public
//! address and every other address as internal (a test policy), so that a local server stands
//! for the internet and any other answer of the resolver for the internal network. The real
//! policy refuses 127.0.0.1 too: the last tests use it.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, Method as AxumMethod, StatusCode as AxumStatus};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use ptc_core::{RunId, Seq, ToolFailure};
use ptc_engine::ports::{RunRef, TaskToken, TenantId};
use serde_json::{Value, json};

use super::*;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Only 127.0.0.1 is reached.
fn test_policy(address: IpAddr) -> Option<&'static str> {
    (address != LOCAL).then_some("test-internal")
}

/// Answers names from a script: each name has a list of answers, the last one repeated.
#[derive(Debug, Default)]
struct ScriptedResolver {
    answers: Mutex<BTreeMap<String, VecDeque<Vec<IpAddr>>>>,
    calls: AtomicUsize,
}

impl ScriptedResolver {
    fn new(answers: &[(&str, &[&[IpAddr]])]) -> Arc<Self> {
        let answers = answers
            .iter()
            .map(|(name, answers)| {
                (
                    (*name).to_owned(),
                    answers.iter().map(|answer| answer.to_vec()).collect(),
                )
            })
            .collect();
        Arc::new(Self {
            answers: Mutex::new(answers),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl NameResolver for ScriptedResolver {
    fn resolve(&self, host: &str) -> Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut answers = self.answers.lock().unwrap();
        let answer = match answers.get_mut(host) {
            Some(queue) if queue.len() > 1 => queue.pop_front(),
            Some(queue) => queue.front().cloned(),
            None => None,
        };
        Box::pin(async move {
            answer.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown name"))
        })
    }
}

/// What the test servers saw.
#[derive(Debug, Default)]
struct Seen {
    requests: AtomicUsize,
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((LOCAL, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

/// Echoes the request: method, headers, body.
async fn echo(method: AxumMethod, headers: HeaderMap, body: Bytes) -> Response {
    let headers: BTreeMap<String, String> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    axum::Json(json!({
        "method": method.as_str(),
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    }))
    .into_response()
}

fn redirect(status: AxumStatus, location: &str) -> Response {
    (status, [("location", location.to_owned())]).into_response()
}

fn tool(resolver: Arc<ScriptedResolver>, config: HttpConfig) -> HttpTool {
    HttpTool::build(config, resolver, test_policy).unwrap()
}

fn call(hosts: &[&str], args: Value) -> ToolCall {
    let run = RunRef::new(TenantId::local(), RunId::from_u128(7));
    ToolCall {
        idempotency_key: format!("{}/1", run.id),
        run,
        seq: Seq(1),
        tool: http_tool(hosts.iter().map(|host| (*host).to_owned()).collect()),
        server: None,
        args,
        attempt: 1,
        schema_change_approved: false,
        token: TaskToken("t".into()),
        trace_context: None,
    }
}

fn result(dispatched: Dispatched) -> Value {
    match dispatched {
        Dispatched::Result(value) => value,
        other => panic!("expected a result, got {other:?}"),
    }
}

fn failure(dispatched: Dispatched) -> (bool, ToolFailure) {
    match dispatched {
        Dispatched::Failed(failure) => (false, failure),
        Dispatched::Transient(failure) => (true, failure),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_request_reaches_an_allowed_host_and_returns_the_response() {
    let address = serve(Router::new().route("/echo", any(echo))).await;
    let resolver = ScriptedResolver::new(&[("api.test", &[&[LOCAL]])]);
    let http = tool(resolver.clone(), HttpConfig::default());
    let response = result(
        http.dispatch(
            call(
                &["api.test"],
                json!({
                    "url": format!("http://api.test:{}/echo", address.port()),
                    "method": "post",
                    "headers": [["content-type", "application/json"], ["x-key", "k"]],
                    "body": "{\"a\":1}"
                }),
            ),
            None,
        )
        .await,
    );
    assert_eq!(response["status"], 200);
    assert_eq!(response["statusText"], "OK");
    assert_eq!(response["redirected"], false);
    let echoed: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["body"], "{\"a\":1}");
    assert_eq!(echoed["headers"]["x-key"], "k");
    assert!(
        echoed["headers"]["user-agent"]
            .as_str()
            .unwrap()
            .starts_with("ai-ptc/")
    );
    assert_eq!(resolver.calls(), 1);
}

#[tokio::test]
async fn a_dns_answer_that_changes_after_the_check_is_never_used() {
    // The first answer is checked and used; an attacker's second answer, an internal address,
    // would be what a client resolving again at connection time reaches.
    let seen = Arc::new(Seen::default());
    let counter = seen.clone();
    let address = serve(Router::new().route(
        "/",
        get(move || {
            counter.requests.fetch_add(1, Ordering::SeqCst);
            async { "public" }
        }),
    ))
    .await;
    let internal = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
    let resolver = ScriptedResolver::new(&[("rebind.test", &[&[LOCAL], &[internal]])]);
    let http = tool(resolver.clone(), HttpConfig::default());
    let url = format!("http://rebind.test:{}/", address.port());
    let response = result(
        http.dispatch(call(&["rebind.test"], json!({ "url": url })), None)
            .await,
    );
    assert_eq!(response["body"], "public");
    assert_eq!(resolver.calls(), 1, "the name is resolved once");
    assert_eq!(seen.requests.load(Ordering::SeqCst), 1);

    // A new client resolves again, gets the internal address, and refuses it.
    let fresh = tool(resolver.clone(), HttpConfig::default());
    let (transient, refused) = failure(
        fresh
            .dispatch(call(&["rebind.test"], json!({ "url": url })), None)
            .await,
    );
    assert!(!transient);
    assert_eq!(
        refused.message,
        "rebind.test resolves to 169.254.169.254, a test-internal address: the http tool does not reach it"
    );
    assert_eq!(seen.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_name_with_any_internal_address_is_refused() {
    let resolver = ScriptedResolver::new(&[(
        "mixed.test",
        &[&[LOCAL, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]],
    )]);
    let http = tool(resolver, HttpConfig::default());
    let (transient, refused) = failure(
        http.dispatch(
            call(&["mixed.test"], json!({ "url": "http://mixed.test/" })),
            None,
        )
        .await,
    );
    assert!(!transient);
    assert_eq!(refused.kind, ToolFailureKind::Error);
    assert!(refused.message.contains("10.0.0.1"), "{}", refused.message);
}

#[tokio::test]
async fn redirects_are_checked_again() {
    let address = serve(
        Router::new()
            .route(
                "/to-metadata-name",
                get(|| async {
                    redirect(AxumStatus::FOUND, "http://metadata.test/latest/meta-data")
                }),
            )
            .route(
                "/to-metadata-address",
                get(|| async {
                    redirect(AxumStatus::FOUND, "http://169.254.169.254/latest/meta-data")
                }),
            )
            .route(
                "/to-unlisted",
                get(|| async { redirect(AxumStatus::MOVED_PERMANENTLY, "https://unlisted.test/") }),
            )
            .route(
                "/to-file",
                get(|| async { redirect(AxumStatus::FOUND, "file:///etc/passwd") }),
            ),
    )
    .await;
    let resolver = ScriptedResolver::new(&[
        ("api.test", &[&[LOCAL]]),
        (
            "metadata.test",
            &[&[IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))]],
        ),
    ]);
    let http = tool(resolver, HttpConfig::default());
    let hosts = ["api.test", "metadata.test", "169.254.169.254"];
    let refused = |path: &'static str| {
        let http = http.clone();
        async move {
            let url = format!("http://api.test:{}{path}", address.port());
            let (transient, failure) = failure(
                http.dispatch(call(&hosts, json!({ "url": url })), None)
                    .await,
            );
            assert!(!transient, "{path}");
            failure.message
        }
    };
    assert_eq!(
        refused("/to-metadata-name").await,
        "metadata.test resolves to 169.254.169.254, a test-internal address: the http tool does not reach it"
    );
    assert_eq!(
        refused("/to-metadata-address").await,
        "169.254.169.254 is a test-internal address: the http tool does not reach it"
    );
    assert_eq!(
        refused("/to-unlisted").await,
        "unlisted.test is not in the toolset's allow-list of the http tool (api.test, metadata.test, 169.254.169.254)"
    );
    assert_eq!(
        refused("/to-file").await,
        "the http tool only sends http and https requests, not file:"
    );
}

#[tokio::test]
async fn five_redirects_are_followed_and_a_sixth_is_refused() {
    let address = serve(Router::new().route(
        "/hop/{n}",
        get(|Path(n): Path<u32>| async move {
            if n == 0 {
                "arrived".into_response()
            } else {
                redirect(AxumStatus::FOUND, &format!("/hop/{}", n - 1))
            }
        }),
    ))
    .await;
    let resolver = ScriptedResolver::new(&[("api.test", &[&[LOCAL]])]);
    let http = tool(resolver, HttpConfig::default());
    let url = |n: u32| format!("http://api.test:{}/hop/{n}", address.port());
    let response = result(
        http.dispatch(call(&["api.test"], json!({ "url": url(5) })), None)
            .await,
    );
    assert_eq!(response["body"], "arrived");
    assert_eq!(response["redirected"], true);
    assert_eq!(response["url"], url(0));
    let (_, refused) = failure(
        http.dispatch(call(&["api.test"], json!({ "url": url(6) })), None)
            .await,
    );
    assert_eq!(
        refused.message,
        format!("GET {} redirects more than 5 times", url(1))
    );
}

#[tokio::test]
async fn redirects_change_the_method_and_drop_credentials_as_fetch_does() {
    let address = serve(
        Router::new()
            .route(
                "/see-other",
                any(|| async { redirect(AxumStatus::SEE_OTHER, "/echo") }),
            )
            .route(
                "/temporary",
                any(|| async { redirect(AxumStatus::TEMPORARY_REDIRECT, "/echo") }),
            )
            .route("/echo", any(echo)),
    )
    .await;
    let port = address.port();
    let resolver = ScriptedResolver::new(&[("api.test", &[&[LOCAL]]), ("other.test", &[&[LOCAL]])]);
    let http = tool(resolver, HttpConfig::default());
    let send = |path: &str, method: &str| {
        let http = http.clone();
        let args = json!({
            "url": format!("http://api.test:{port}{path}"),
            "method": method,
            "headers": [["authorization", "Bearer s3cret"], ["content-type", "text/plain"]],
            "body": "payload"
        });
        async move {
            let response = result(
                http.dispatch(call(&["api.test", "other.test"], args), None)
                    .await,
            );
            serde_json::from_str::<Value>(response["body"].as_str().unwrap()).unwrap()
        }
    };
    let see_other = send("/see-other", "POST").await;
    assert_eq!(see_other["method"], "GET");
    assert_eq!(see_other["body"], "");
    assert!(see_other["headers"].get("content-type").is_none());
    assert_eq!(see_other["headers"]["authorization"], "Bearer s3cret");

    let temporary = send("/temporary", "POST").await;
    assert_eq!(temporary["method"], "POST");
    assert_eq!(temporary["body"], "payload");

    // Another server redirects to the echo, under another name: another origin.
    let address = serve(Router::new().route(
        "/elsewhere",
        any(move || async move {
            redirect(AxumStatus::FOUND, &format!("http://other.test:{port}/echo"))
        }),
    ))
    .await;
    let response = result(
        http.dispatch(
            call(
                &["api.test", "other.test"],
                json!({
                    "url": format!("http://api.test:{}/elsewhere", address.port()),
                    "headers": [["authorization", "Bearer s3cret"]]
                }),
            ),
            None,
        )
        .await,
    );
    let elsewhere: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
    assert!(
        elsewhere["headers"].get("authorization").is_none(),
        "credentials do not follow a redirect to another origin"
    );
}

#[tokio::test]
async fn responses_are_bounded_in_size_and_time() {
    let address = serve(
        Router::new()
            .route("/big", get(|| async { "x".repeat(2048) }))
            .route(
                "/streamed",
                get(|| async {
                    let chunks =
                        (0..8).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'y'; 512])));
                    axum::body::Body::from_stream(futures_util::stream::iter(chunks))
                }),
            )
            .route(
                "/slow",
                any(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            ),
    )
    .await;
    let resolver = ScriptedResolver::new(&[("api.test", &[&[LOCAL]])]);
    let http = tool(
        resolver,
        HttpConfig {
            max_response_bytes: 1024,
            timeout: Duration::from_millis(300),
            ..HttpConfig::default()
        },
    );
    let url = |path: &str| format!("http://api.test:{}{path}", address.port());
    for path in ["/big", "/streamed"] {
        let (transient, refused) = failure(
            http.dispatch(call(&["api.test"], json!({ "url": url(path) })), None)
                .await,
        );
        assert!(!transient);
        assert_eq!(
            refused.message,
            format!(
                "the response of GET {} is larger than 1024 bytes",
                url(path)
            )
        );
    }
    let (transient, timed_out) = failure(
        http.dispatch(call(&["api.test"], json!({ "url": url("/slow") })), None)
            .await,
    );
    assert!(transient, "a GET is sent again");
    assert_eq!(timed_out.kind, ToolFailureKind::Timeout);
    let (transient, timed_out) = failure(
        http.dispatch(
            call(
                &["api.test"],
                json!({ "url": url("/slow"), "method": "POST", "body": "x" }),
            ),
            None,
        )
        .await,
    );
    assert!(!transient, "a POST may have taken effect: not sent again");
    assert_eq!(timed_out.kind, ToolFailureKind::Timeout);
}

#[tokio::test]
async fn statuses_are_results_and_binary_bodies_are_base64() {
    let address = serve(
        Router::new()
            .route("/missing", get(|| async { (AxumStatus::NOT_FOUND, "no") }))
            .route("/bytes", get(|| async { vec![0u8, 159, 146, 150] })),
    )
    .await;
    let resolver = ScriptedResolver::new(&[("api.test", &[&[LOCAL]])]);
    let http = tool(resolver, HttpConfig::default());
    let url = |path: &str| format!("http://api.test:{}{path}", address.port());
    let missing = result(
        http.dispatch(call(&["api.test"], json!({ "url": url("/missing") })), None)
            .await,
    );
    assert_eq!(missing["status"], 404);
    assert_eq!(missing["body"], "no");
    let bytes = result(
        http.dispatch(call(&["api.test"], json!({ "url": url("/bytes") })), None)
            .await,
    );
    assert_eq!(bytes["body"], "AJ+Slg==");
    assert_eq!(bytes["bodyEncoding"], "base64");
}

#[tokio::test]
async fn a_host_that_cannot_be_reached_is_a_transient_failure() {
    let listener = std::net::TcpListener::bind((LOCAL, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let resolver = ScriptedResolver::new(&[("down.test", &[&[LOCAL]])]);
    let http = tool(resolver, HttpConfig::default());
    let (transient, unreachable) = failure(
        http.dispatch(
            call(
                &["down.test"],
                json!({ "url": format!("http://down.test:{port}/"), "method": "POST" }),
            ),
            None,
        )
        .await,
    );
    assert!(transient, "nothing was sent: even a POST is sent again");
    assert_eq!(unreachable.kind, ToolFailureKind::Unavailable);
}

#[tokio::test]
async fn the_real_policy_refuses_local_and_internal_addresses() {
    let http = HttpTool::new(HttpConfig::default()).unwrap();
    for (hosts, url, expected) in [
        (
            &["127.0.0.1"][..],
            "http://127.0.0.1:9/",
            "127.0.0.1 is a loopback address: the http tool does not reach it",
        ),
        (
            &["[::1]"][..],
            "http://[::1]:9/",
            "::1 is a loopback address: the http tool does not reach it",
        ),
        (
            &["169.254.169.254"][..],
            "http://169.254.169.254/latest/meta-data/",
            "169.254.169.254 is a link-local (cloud metadata) address: the http tool does not reach it",
        ),
        (
            &["localhost"][..],
            "http://localhost:9/",
            "localhost resolves to",
        ),
    ] {
        let (transient, refused) = failure(
            http.dispatch(call(hosts, json!({ "url": url })), None)
                .await,
        );
        assert!(!transient, "{url}");
        assert!(refused.message.starts_with(expected), "{}", refused.message);
    }
}

#[tokio::test]
async fn webhook_and_other_routes_are_not_answered_here() {
    let http = HttpTool::new(HttpConfig::default()).unwrap();
    let mut webhook = call(&["a.test"], json!({ "name": "x" }));
    webhook.tool = Tool::new("webhook", ToolRoute::Builtin(Builtin::Webhook), json!({}));
    let (transient, failure) = failure(http.dispatch(webhook, None).await);
    assert!(!transient);
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
}
