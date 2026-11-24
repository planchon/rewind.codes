//! The dashboard's sign-in end to end (`archi/07-code-architecture.md`, "Sign-in (`/auth`)"), over
//! HTTP with a cookie jar, the engine's fake of WorkOS wired as the identity provider: a code by
//! email, GitHub's callback and its `state`, the steps WorkOS asks for, the waitlist, refreshes
//! and switches, sign-out; members' access tokens on `/v1`, which a worker refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::{Duration, Instant};

use clap::Parser;
use futures_util::StreamExt;
use rewind_api::access::Permission;
use rewind_api::auth::{AuthState, OAUTH_COOKIE, SESSION_COOKIE};
use rewind_api::events::{EventsQuery, StreamMessage};
use rewind_api::organization::{CreateOrganization, Principal};
use rewind_api::runs::{RunFilter, StartRun};
use rewind_api::{Client, Problem, ProblemType};
use rewind_engine::ports::IdentityProvider;
use rewind_server::{ServerArgs, StartupError};
use serde_json::json;
use support::{Browser, TestServer, WORKOS, answer, identity, token};

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn member(server: &TestServer, state: &AuthState) -> Client {
    server.client(Some(&token(state).0))
}

/// The `state` of the authorization URL a `GET /auth/github` redirected to.
fn state_of(location: &str) -> String {
    location
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("state="))
        .unwrap()
        .to_owned()
}

async fn github(browser: &mut Browser, query: &str) -> String {
    let response = browser.get(&format!("/auth/github{query}")).await;
    assert_eq!(response.status(), 302);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    assert!(
        location.starts_with("https://identity.invalid/authorize?provider=GitHubOAuth"),
        "{location}"
    );
    assert!(
        location.contains("redirect_uri=https%3A%2F%2Fapp.rewind.test%2Fauth%2Fcallback"),
        "{location}"
    );
    state_of(&location)
}

async fn callback(browser: &mut Browser, query: &str) -> String {
    let response = browser.get(&format!("/auth/callback?{query}")).await;
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["cache-control"], "no-store");
    response.headers()["location"].to_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_by_email_signs_in_and_a_member_without_organization_only_onboards() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    let mut browser = Browser::new(&server.url);
    let config = browser.get("/auth/config").await;
    assert_eq!(config.status(), 200);
    assert_eq!(
        config.json::<serde_json::Value>().await.unwrap(),
        json!({ "methods": ["github", "magic_auth"], "waitlist": true })
    );

    // Sign-ups open: a code for any email.
    for email in ["nobody@example.net", "ada@example.net"] {
        let sent = browser
            .post("/auth/magic-auth", json!({ "email": email }))
            .await;
        assert_eq!(sent.status(), 202);
        assert_eq!(sent.text().await.unwrap(), "");
    }
    let wrong = browser
        .step(
            "/auth/magic-auth/verify",
            json!({ "email": "ada@example.net", "code": "000000" }),
        )
        .await;
    assert_eq!(wrong.unwrap_err(), (400, "invalid-request".into()));
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let AuthState::Authenticated {
        access_token,
        person,
        organization_id,
        ..
    } = &state
    else {
        panic!("signed in: {state:?}");
    };
    assert_eq!(person.email, "ada@example.net");
    assert_eq!(organization_id, &None, "a new person has no organization");
    let session = &browser.cookies[SESSION_COOKIE];
    for attribute in ["Path=/auth", "HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(
            session.attributes.contains(attribute),
            "{}",
            session.attributes
        );
    }
    assert!(!session.value.contains(access_token.as_str()), "sealed");
    assert_eq!(browser.session().await.unwrap(), state, "no refresh yet");

    // Without organization: who am I, my invitations, create one; nothing else.
    let ada = member(&server, &state);
    let me = ada.whoami().await.unwrap();
    assert_eq!(me.organization_id, None);
    assert!(me.permissions.is_empty() && me.grant.is_none() && me.organizations.is_empty());
    let Some(Principal::Member { person }) = me.principal else {
        panic!("a member: {:?}", me.principal);
    };
    assert_eq!(person.email, "ada@example.net");
    assert!(ada.my_invitations().await.unwrap().invitations.is_empty());
    for refused in [
        ada.list_runs(&RunFilter::default()).await.map(|_| ()),
        ada.list_permissions().await.map(|_| ()),
        ada.list_roles().await.map(|_| ()),
        ada.server_info().await.map(|_| ()),
    ] {
        let refused = problem(refused.unwrap_err());
        assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
        assert!(
            refused.detail.contains("POST /v1/organizations"),
            "{}",
            refused.detail
        );
    }
    let acme = ada
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    let state = browser.switch(&acme.id).await.unwrap();
    assert_eq!(token(&state).1.as_deref(), Some(acme.id.as_str()));
    let ada = member(&server, &state);
    let me = ada.whoami().await.unwrap();
    assert_eq!(me.organization_id.as_deref(), Some(acme.id.as_str()));
    assert_eq!(me.grant.as_ref().unwrap().role, "owner");
    assert_eq!(me.grant.unwrap().granted_by, None, "the server gave it");
    assert_eq!(me.permissions, Permission::ALL);
    let organization = me.organization.unwrap();
    assert_eq!(
        (
            organization.name.as_str(),
            organization.default_role.as_str()
        ),
        ("Acme", "reader")
    );
    assert_eq!(me.organizations.len(), 1);
    ada.list_runs(&RunFilter::default()).await.unwrap();

    // Keys belong to their tenant: they create no organization and have no invitations.
    assert_eq!(
        problem(
            server
                .admin
                .create_organization(&CreateOrganization { name: "K".into() })
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::Forbidden)
    );
    assert_eq!(
        problem(server.admin.my_invitations().await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden)
    );
    let forged = server.client(Some("eyJhbGciOiJSUzI1NiJ9.forged.token"));
    assert_eq!(
        problem(forged.whoami().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_signs_in_through_the_callback_once_its_state_is_checked() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    let mut browser = Browser::new(&server.url);
    let code = identity.github_code("grace@example.net", true);

    let state = github(&mut browser, "").await;
    let oauth = &browser.cookies[OAUTH_COOKIE];
    for attribute in [
        "Path=/auth",
        "Max-Age=300",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
    ] {
        assert!(oauth.attributes.contains(attribute), "{}", oauth.attributes);
    }
    assert!(!oauth.value.contains(&state), "sealed");
    let location = callback(&mut browser, &format!("code={code}&state=wrong{state}")).await;
    assert_eq!(location, "/?auth_error=state_mismatch");
    assert!(!browser.cookies.contains_key(OAUTH_COOKIE), "cleared");
    assert_eq!(
        browser.session().await.unwrap_err(),
        (401, "unauthorized".into())
    );

    let state = github(&mut browser, "").await;
    let location = callback(
        &mut browser,
        &format!("error=access_denied&error_description=no&state={state}"),
    )
    .await;
    assert_eq!(location, "/?auth_error=provider_error");
    let state = github(&mut browser, "").await;
    let location = callback(&mut browser, &format!("code=unknown&state={state}")).await;
    assert_eq!(location, "/?auth_error=sign_in_failed");

    let state = github(&mut browser, "").await;
    let location = callback(&mut browser, &format!("code={code}&state={state}")).await;
    assert_eq!(location, "/");
    let signed_in = browser.session().await.unwrap();
    let AuthState::Authenticated { person, .. } = &signed_in else {
        panic!("signed in: {signed_in:?}");
    };
    assert_eq!(person.email, "grace@example.net");

    // A forged callback, in a browser with no GitHub sign-in in progress, changes nothing.
    let location = callback(&mut browser, &format!("code={code}&state={state}")).await;
    assert_eq!(location, "/?auth_error=state_mismatch");
    assert_eq!(browser.session().await.unwrap(), signed_in);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_steps_of_a_sign_in_verify_the_email_and_choose_an_organization() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;

    // GitHub vouches for an email it has not verified: WorkOS sends a code to verify it.
    let mut browser = Browser::new(&server.url);
    let code = identity.github_code("lin@example.net", false);
    let state = github(&mut browser, "").await;
    callback(&mut browser, &format!("code={code}&state={state}")).await;
    let step = browser.session().await.unwrap();
    assert_eq!(
        step,
        AuthState::EmailVerificationRequired {
            email: "lin@example.net".into()
        }
    );
    let elsewhere = browser
        .step("/auth/organization", json!({ "organizationId": "org_1" }))
        .await;
    assert_eq!(elsewhere.unwrap_err(), (400, "invalid-request".into()));
    let wrong = browser
        .step("/auth/email-verification", json!({ "code": "000000" }))
        .await;
    assert_eq!(wrong.unwrap_err(), (400, "invalid-request".into()));
    let verified = browser
        .step(
            "/auth/email-verification",
            json!({ "code": identity.last_code("lin@example.net").unwrap() }),
        )
        .await
        .unwrap();
    assert!(matches!(verified, AuthState::Authenticated { .. }));

    // A member of two organizations chooses one, and gets the default role there.
    let carol = identity.add_user("carol@example.net", true);
    let mut organizations = Vec::new();
    for name in ["Acme", "Globex"] {
        let organization = identity.create_organization(name).await.unwrap();
        identity
            .add_member(&organization.id, &carol.id)
            .await
            .unwrap();
        organizations.push(organization.id.to_string());
    }
    let mut browser = Browser::new(&server.url);
    let step = browser
        .sign_in(&identity, "carol@example.net", None)
        .await
        .unwrap();
    let AuthState::OrganizationSelectionRequired {
        organizations: offered,
        person,
    } = &step
    else {
        panic!("an organization is asked for: {step:?}");
    };
    assert_eq!(person.email, "carol@example.net");
    assert_eq!(offered.len(), 2);
    assert_eq!(browser.session().await.unwrap(), step, "the step is kept");
    let refused = browser
        .step(
            "/auth/organization",
            json!({ "organizationId": "org_of_someone_else" }),
        )
        .await;
    assert_eq!(refused.unwrap_err(), (403, "forbidden".into()));
    let chosen = browser
        .step(
            "/auth/organization",
            json!({ "organizationId": organizations[1] }),
        )
        .await
        .unwrap();
    assert_eq!(token(&chosen).1.as_deref(), Some(organizations[1].as_str()));
    let me = member(&server, &chosen).whoami().await.unwrap();
    let grant = me.grant.unwrap();
    assert_eq!((grant.role.as_str(), grant.granted_by), ("reader", None));
    assert_eq!(me.organizations.len(), 2);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_sign_ups_send_people_to_the_waitlist() {
    let (identity, _clock) = identity();
    identity.set_sign_ups_closed(true);
    let server = TestServer::signed_in(&identity, &[]).await;
    let mut browser = Browser::new(&server.url);
    let closed = browser
        .sign_in(&identity, "new@example.net", None)
        .await
        .unwrap();
    assert_eq!(
        closed,
        AuthState::SignUpClosed {
            email: Some("new@example.net".into())
        }
    );
    // No code was sent; the step is kept, as the others.
    assert_eq!(identity.last_code("new@example.net"), None);
    assert_eq!(browser.session().await.unwrap(), closed);
    let joined = browser
        .post("/auth/waitlist", json!({ "email": "new@example.net" }))
        .await;
    assert_eq!(joined.status(), 202);
    assert_eq!(identity.waitlist(), ["new@example.net"]);
    let blank = browser
        .post("/auth/waitlist", json!({ "email": " " }))
        .await;
    assert_eq!(blank.status(), 400);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_post_takes_json_only_and_no_session_is_401() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    let mut browser = Browser::new(&server.url);
    let form = "email=ada%40example.net";
    for content_type in [
        Some("application/x-www-form-urlencoded"),
        Some("text/plain"),
        None,
    ] {
        let refused = browser
            .post_raw("/auth/magic-auth", content_type, form)
            .await;
        assert_eq!(refused.status(), 415, "{content_type:?}");
        let refused = browser.post_raw("/auth/logout", content_type, "").await;
        assert_eq!(refused.status(), 415, "{content_type:?}");
    }
    assert_eq!(
        browser.session().await.unwrap_err(),
        (401, "unauthorized".into())
    );
    let refused = browser
        .step("/auth/email-verification", json!({ "code": "123456" }))
        .await;
    assert_eq!(refused.unwrap_err(), (401, "unauthorized".into()));
    // The dashboard signs out with `{}`; an empty body does as well.
    let out = browser.post("/auth/logout", json!({})).await;
    assert_eq!(out.status(), 204);
    let out = browser
        .post_raw("/auth/logout", Some("application/json"), "")
        .await;
    assert_eq!(out.status(), 204);
    // A session sealed with another password does not unseal: 401, and it is cleared.
    browser.cookies.insert(
        SESSION_COOKIE.into(),
        support::Cookie {
            value: "c2VhbGVkIGVsc2V3aGVyZQ==".into(),
            attributes: String::new(),
        },
    );
    let response = browser.post("/auth/session", json!({})).await;
    assert_eq!(response.status(), 401);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_refreshes_near_expiry_switches_organization_and_signs_out() {
    let (identity, clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    let ada = identity.add_user("ada@example.net", true);
    let globex = identity.create_organization("Globex").await.unwrap();
    identity.add_member(&globex.id, &ada.id).await.unwrap();
    let elsewhere = identity.create_organization("Initech").await.unwrap();

    // WorkOS's clock runs 250 s behind: its tokens have 50 s left by the server's.
    clock.set(-250_000);
    let mut browser = Browser::new(&server.url);
    let first = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    assert_eq!(token(&first).1.as_deref(), Some(globex.id.as_str()));
    let refreshed = browser.session().await.unwrap();
    assert_ne!(
        token(&refreshed).0,
        token(&first).0,
        "less than a minute left"
    );
    clock.set(0);
    let fresh = browser.session().await.unwrap();
    assert_ne!(token(&fresh).0, token(&refreshed).0);
    assert_eq!(
        token(&browser.session().await.unwrap()).0,
        token(&fresh).0,
        "five minutes left: no refresh"
    );

    // A switch, to an organization of the member's only.
    let acme = member(&server, &fresh)
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    let switched = browser.switch(&acme.id).await.unwrap();
    assert_eq!(token(&switched).1.as_deref(), Some(acme.id.as_str()));
    assert_ne!(token(&switched).0, token(&fresh).0);
    assert_eq!(
        member(&server, &switched)
            .whoami()
            .await
            .unwrap()
            .grant
            .unwrap()
            .role,
        "owner"
    );
    assert_eq!(
        browser.switch(elsewhere.id.as_str()).await.unwrap_err(),
        (403, "forbidden".into())
    );
    let back = browser.switch(globex.id.as_str()).await.unwrap();
    assert_eq!(token(&back).1.as_deref(), Some(globex.id.as_str()));

    // Signing out revokes the session: its cookie, kept aside, refreshes no more.
    let kept = browser.cookies[SESSION_COOKIE].clone();
    let out = browser.post("/auth/logout", json!({})).await;
    assert_eq!(out.status(), 204);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    assert_eq!(
        browser.session().await.unwrap_err(),
        (401, "unauthorized".into())
    );
    browser.cookies.insert(SESSION_COOKIE.into(), kept);
    assert_eq!(
        browser.switch(acme.id.as_str()).await.unwrap_err(),
        (401, "unauthorized".into())
    );
    assert!(!browser.cookies.contains_key(SESSION_COOKIE), "cleared");
    server.stop().await;
}

const SIGNAL: &str = r#"import { signal } from "rewind";
export default async function main() {
  return { decision: await signal<{ ok: boolean }>("approval") };
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_stream_opened_with_an_access_token_ends_at_its_expiry() {
    let (identity, clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    identity.add_user("ada@example.net", true);
    let mut browser = Browser::new(&server.url);
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let acme = member(&server, &state)
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    let owner = member(&server, &browser.switch(&acme.id).await.unwrap());
    let run = owner.start_run(&StartRun::new(SIGNAL), None).await.unwrap();

    // A token with 3 s left.
    clock.set(-297_000);
    let short = member(&server, &browser.switch(&acme.id).await.unwrap());
    let started = Instant::now();
    let mut stream = short
        .run_events(
            &run.id,
            &EventsQuery {
                follow: Some(true),
                from: None,
            },
        )
        .await
        .unwrap();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("the stream ends")
    {
        assert!(
            !matches!(event.unwrap(), StreamMessage::End { .. }),
            "the run still waits for its signal"
        );
    }
    let lasted = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(20)).contains(&lasted),
        "{lasted:?}"
    );
    // A key's stream follows the run as long as it lasts.
    owner
        .cancel_run(&run.id, &rewind_api::runs::CancelRun::default())
        .await
        .unwrap();
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_without_workos_accepts_only_api_keys() {
    let (identity, _clock) = identity();
    let signed_in = TestServer::signed_in(&identity, &[]).await;
    identity.add_user("ada@example.net", true);
    let mut browser = Browser::new(&signed_in.url);
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    member(&signed_in, &state).whoami().await.unwrap();

    let plain = TestServer::start(&[]).await;
    let mut at_plain = Browser::new(&plain.url);
    let config = at_plain.get("/auth/config").await;
    assert_eq!(config.status(), 404);
    assert_eq!(
        answer(at_plain.post("/auth/session", json!({})).await)
            .await
            .unwrap_err(),
        (404, "not-found".into())
    );
    assert_eq!(
        problem(member(&plain, &state).whoami().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    assert_eq!(
        problem(plain.admin.list_members().await.unwrap_err()).kind(),
        Some(ProblemType::NotFound),
        "no WorkOS, no members"
    );
    plain.admin.list_roles().await.unwrap();
    plain.stop().await;
    signed_in.stop().await;
}

#[tokio::test]
async fn workos_takes_its_four_variables_together_or_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = format!("turso:{}", dir.path().join("control.db").display());
    let key = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";
    for left_out in 0..4 {
        let mut args = vec![
            "rewind server",
            "--store",
            &store,
            "--master-key",
            key,
            "--listen",
            "127.0.0.1:0",
        ];
        for (at, pair) in WORKOS.chunks(2).enumerate() {
            if at != left_out {
                args.extend_from_slice(pair);
            }
        }
        let args = ServerArgs::try_parse_from(args).unwrap();
        match rewind_server::start(args).await {
            Err(StartupError::Config(message)) => {
                assert!(message.contains("together or not at all"), "{message}");
                assert!(!message.contains("sk_test_of_the_tests"), "{message}");
            }
            Err(other) => panic!("a usage error (exit 64): {other}"),
            Ok(running) => {
                running.stop().await;
                panic!("{left_out} left out: started");
            }
        }
    }
}
