//! People's connections and the OAuth authorization server end to end
//! (`archi/06-storage-and-scale.md`, "Connections"; `archi/07-code-architecture.md`, "MCP and
//! OAuth"), with the engine's fake of WorkOS: a client's metadata document served by a fake on
//! 127.0.0.1 (let through with `--allow-internal-mcp`), a registered client, the consent, the
//! tokens, `/v1` with `Rewind-Organization`, refresh, revocation, and the refusals.
//!
//! The full flow runs on Turso and on Postgres (Docker; skipped with a message without it,
//! unless `REWIND_REQUIRE_DOCKER` is set).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use axum::routing::get;
use reqwest::StatusCode;
use rewind_api::Client;
use rewind_api::access::SetGrant;
use rewind_api::auth::AuthState;
use rewind_api::common::Labels;
use rewind_api::organization::CreateOrganization;
use rewind_engine::adapters::MemoryIdentity;
use rewind_engine::ports::{IdentityProvider, OrganizationId};
use rewind_server::McpPrincipal;
use serde_json::{Value, json};
use support::{Browser, Postgres, TestServer, identity, token};

/// RFC 7636, appendix B.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

const PUBLIC_URL: &str = "https://api.rewind.test";
const RESOURCE: &str = "https://api.rewind.test/mcp";
const CLAUDE_CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";

/// A client's metadata document, served on 127.0.0.1: its URL is its `client_id`.
struct Document {
    url: String,
}

impl Document {
    async fn serve() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/oauth/client.json",
            listener.local_addr().unwrap().port()
        );
        let document = json!({
            "client_id": url,
            "client_name": "Claude",
            "client_uri": "https://claude.ai",
            "redirect_uris": [CLAUDE_CALLBACK, "http://localhost/callback"],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
        });
        let app = axum::Router::new().route(
            "/oauth/client.json",
            get(move || {
                let document = document.clone();
                async move { ([("cache-control", "max-age=3600")], axum::Json(document)) }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url }
    }
}

/// A server with a sign-in, a public URL, and the fake document's host let through.
async fn server_on(identity: &MemoryIdentity, store: Option<&str>) -> TestServer {
    let extra = [
        "--public-url",
        PUBLIC_URL,
        "--allow-internal-mcp",
        "127.0.0.1",
    ];
    match store {
        None => TestServer::signed_in(identity, &extra).await,
        Some(store) => {
            let dir = tempfile::tempdir().unwrap();
            let blobs = dir.path().join("blobs");
            TestServer::start_with(dir, store, &blobs, &extra, Some(identity.clone())).await
        }
    }
}

/// A person signed in on the dashboard.
struct Person {
    browser: Browser,
    token: String,
    user: String,
}

async fn sign_in(server: &TestServer, identity: &MemoryIdentity, email: &str) -> Person {
    identity.add_user(email, true);
    let mut browser = Browser::new(&server.url);
    let state = browser.sign_in(identity, email, None).await.unwrap();
    let user = match &state {
        AuthState::Authenticated { user, .. } => user.id.clone(),
        other => panic!("not signed in: {other:?}"),
    };
    Person {
        browser,
        token: token(&state).0,
        user,
    }
}

impl Person {
    /// Switches the session to `organization`.
    async fn switch(&mut self, organization: &str) {
        let state = self.browser.switch(organization).await.unwrap();
        self.token = token(&state).0;
    }
}

/// Acme, whose owner is Ada, and Bob, a member of Acme with the role `harness`.
struct World {
    server: TestServer,
    identity: MemoryIdentity,
    http: reqwest::Client,
    ada: Person,
    bob: Person,
    acme: String,
    owner: Client,
    document: Document,
}

impl World {
    async fn new(store: Option<&str>) -> Self {
        let (identity, _clock) = identity();
        identity.set_sign_ups_closed(true);
        let server = server_on(&identity, store).await;
        let mut ada = sign_in(&server, &identity, "ada@example.net").await;
        let acme = server
            .client(Some(&ada.token))
            .create_organization(&CreateOrganization {
                name: "Acme".into(),
            })
            .await
            .unwrap()
            .id;
        ada.switch(&acme).await;
        let owner = server.client(Some(&ada.token));
        let bob_user = identity.add_user("bob@example.net", true);
        identity
            .add_member(&OrganizationId::new(&acme), &bob_user.id)
            .await
            .unwrap();
        let bob = sign_in(&server, &identity, "bob@example.net").await;
        owner
            .set_member_grant(
                &bob.user,
                &SetGrant {
                    role: "harness".into(),
                    allowed_labels: Labels::new(),
                },
            )
            .await
            .unwrap();
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            document: Document::serve().await,
            server,
            identity,
            ada,
            bob,
            acme,
            owner,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.url)
    }

    /// `GET /oauth/authorize`, without following its redirect.
    async fn authorize(&self, params: &[(&str, &str)]) -> reqwest::Response {
        let query = serde_html_form::to_string(params).unwrap();
        self.http
            .get(self.url(&format!("/oauth/authorize?{query}")))
            .send()
            .await
            .unwrap()
    }

    /// The sealed request `GET /oauth/authorize` sends the browser to the consent with.
    async fn request(&self, client_id: &str, redirect_uri: &str) -> String {
        let response = self
            .authorize(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", redirect_uri),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("state", "the-state"),
                ("scope", "offline_access"),
                ("resource", RESOURCE),
            ])
            .await;
        assert_eq!(response.status(), StatusCode::FOUND);
        let location = location(&response);
        let (page, query) = location.split_once('?').unwrap();
        assert_eq!(page, "https://app.rewind.test/connect/authorize");
        let params: Vec<(String, String)> = serde_html_form::from_str(query).unwrap();
        params
            .into_iter()
            .find(|(key, _)| key == "request")
            .unwrap()
            .1
    }

    /// A request to `/v1` with `bearer`, and `Rewind-Organization` when given.
    fn v1(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: &str,
        organization: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, self.url(path))
            .bearer_auth(bearer);
        if let Some(organization) = organization {
            request = request.header("Rewind-Organization", organization);
        }
        request
    }

    /// The person's consent: allows `organizations` with `labels`; the code given.
    async fn consent(
        &self,
        person: &Person,
        request: &str,
        organizations: Value,
        labels: Value,
    ) -> reqwest::Response {
        self.v1(
            reqwest::Method::POST,
            "/v1/oauth/consent",
            &person.token,
            None,
        )
        .json(&json!({
            "request": request,
            "decision": "allow",
            "organizations": organizations,
            "allowedLabels": labels,
        }))
        .send()
        .await
        .unwrap()
    }

    /// The code of an allowed consent's redirect.
    async fn code(&self, person: &Person, request: &str, organizations: Value) -> String {
        let response = self
            .consent(person, request, organizations, json!({ "via": "claude" }))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        let redirect = body["redirectTo"].as_str().unwrap();
        let (base, query) = redirect.split_once('?').unwrap();
        assert_eq!(base, CLAUDE_CALLBACK.split('?').next().unwrap_or(base));
        let params: Vec<(String, String)> = serde_html_form::from_str(query).unwrap();
        let param = |name: &str| {
            params
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(param("state").as_deref(), Some("the-state"));
        param("code").unwrap()
    }

    /// `POST /oauth/token` with a form, and HTTP Basic credentials when given.
    async fn token(&self, form: &[(&str, &str)], basic: Option<(&str, &str)>) -> (u16, Value) {
        let mut request = self.form("/oauth/token", form);
        if let Some((id, secret)) = basic {
            request = request.basic_auth(id, Some(secret));
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        assert_eq!(
            response.headers()["cache-control"],
            "no-store",
            "token answers are never cached"
        );
        (status, response.json().await.unwrap())
    }

    /// Exchanges a code of the metadata document's client.
    async fn exchange(&self, code: &str) -> (u16, Value) {
        self.token(
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", CLAUDE_CALLBACK),
                ("client_id", &self.document.url),
                ("code_verifier", VERIFIER),
            ],
            None,
        )
        .await
    }

    /// A connection of `person` through the metadata document's client, opening `organizations`.
    async fn connect(&self, person: &Person, organizations: Value) -> Tokens {
        let request = self.request(&self.document.url, CLAUDE_CALLBACK).await;
        let code = self.code(person, &request, organizations).await;
        let (status, tokens) = self.exchange(&code).await;
        assert_eq!(status, 200, "{tokens}");
        Tokens::of(&tokens)
    }

    /// A `POST` of a form, as OAuth's endpoints take it.
    fn form(&self, path: &str, form: &[(&str, &str)]) -> reqwest::RequestBuilder {
        self.http
            .post(self.url(path))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(serde_html_form::to_string(form).unwrap())
    }

    async fn status(&self, request: reqwest::RequestBuilder) -> (u16, Value) {
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }
}

#[derive(Debug, Clone)]
struct Tokens {
    access: String,
    refresh: String,
}

impl Tokens {
    fn of(answer: &Value) -> Self {
        assert_eq!(answer["token_type"], "Bearer");
        assert_eq!(answer["expires_in"], 3600);
        assert_eq!(answer["scope"], "offline_access");
        let access = answer["access_token"].as_str().unwrap().to_owned();
        let refresh = answer["refresh_token"].as_str().unwrap().to_owned();
        assert!(access.starts_with("rewind_ct_"), "{access}");
        assert!(refresh.starts_with("rewind_rt_"), "{refresh}");
        Self { access, refresh }
    }
}

fn location(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("location")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

fn problem_type(body: &Value) -> &str {
    body["type"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("urn:rewind:problem:")
}

/// The whole flow: discovery, authorize, consent, token, `/v1` with `Rewind-Organization`,
/// refresh, revocation.
async fn the_whole_flow(world: &World) {
    // Discovery.
    let (status, resource) = world
        .status(
            world
                .http
                .get(world.url("/.well-known/oauth-protected-resource/mcp")),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        resource,
        json!({
            "resource": RESOURCE,
            "authorization_servers": [PUBLIC_URL],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["offline_access"],
            "resource_name": "Rewind",
        })
    );
    let (_, root) = world
        .status(
            world
                .http
                .get(world.url("/.well-known/oauth-protected-resource")),
        )
        .await;
    assert_eq!(root, resource);
    let (status, server) = world
        .status(
            world
                .http
                .get(world.url("/.well-known/oauth-authorization-server")),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(server["issuer"], PUBLIC_URL);
    assert_eq!(
        server["token_endpoint"],
        format!("{PUBLIC_URL}/oauth/token")
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(server["client_id_metadata_document_supported"], true);
    let info = world.owner.server_info().await.unwrap();
    assert_eq!(info.mcp_url.as_deref(), Some(RESOURCE));
    assert!(info.oauth);

    // The consent page reads the request.
    let request = world.request(&world.document.url, CLAUDE_CALLBACK).await;
    let (status, consent) = world
        .status(world.v1(
            reqwest::Method::GET,
            &format!(
                "/v1/oauth/consent?{}",
                serde_html_form::to_string([("request", request.as_str())]).unwrap()
            ),
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 200, "{consent}");
    assert_eq!(consent["client"]["name"], "Claude");
    assert_eq!(consent["client"]["kind"], "metadata_document");
    assert_eq!(consent["client"]["uri"], "https://claude.ai");
    assert_eq!(consent["redirectHost"], "claude.ai");
    assert_eq!(consent["loopback"], false);
    assert_eq!(consent["allowedLabels"], json!({ "via": "claude" }));
    assert_eq!(
        consent["organizations"],
        json!([{
            "id": world.acme,
            "name": "Acme",
            "role": "owner",
            "accesses": ["read", "run"],
            "current": true,
        }])
    );

    // Allowed: a code, exchanged once for the connection and its tokens.
    let code = world
        .code(
            &world.ada,
            &request,
            json!([{ "id": world.acme, "access": "run" }]),
        )
        .await;
    let (status, answer) = world.exchange(&code).await;
    assert_eq!(status, 200, "{answer}");
    let tokens = Tokens::of(&answer);

    // `/v1` with the connection's token: who it is, and what it may do in Acme.
    let (status, whoami) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &tokens.access, None))
        .await;
    assert_eq!(status, 200, "{whoami}");
    assert_eq!(whoami["principal"]["kind"], "connection");
    assert_eq!(whoami["principal"]["person"]["email"], "ada@example.net");
    assert_eq!(whoami["principal"]["client"]["name"], "Claude");
    assert_eq!(
        whoami["principal"]["organizations"],
        json!([{
            "id": world.acme,
            "name": "Acme",
            "access": "run",
            "allowedLabels": { "via": "claude" },
        }])
    );
    assert!(whoami.get("tenant").is_none());
    let (status, whoami) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/whoami",
            &tokens.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(status, 200, "{whoami}");
    assert_eq!(whoami["tenant"], world.acme.as_str());
    assert_eq!(
        whoami["permissions"],
        json!([
            "runs:read",
            "runs:start",
            "runs:control",
            "toolsets:read",
            "namespaces:read"
        ]),
        "a connection never administers, whatever its person's role"
    );
    let (status, run) = world
        .status(
            world
                .v1(
                    reqwest::Method::POST,
                    "/v1/runs",
                    &tokens.access,
                    Some(&world.acme),
                )
                .header("Rewind-Namespace", "claude")
                .json(&json!({
                    "script": "export default async function main() { return 1; }",
                    "check": false,
                })),
        )
        .await;
    assert_eq!(status, 201, "{run}");
    assert_eq!(
        run["labels"],
        json!({ "via": "claude" }),
        "what the connection starts carries its labels"
    );
    let (status, problem) = world
        .status(world.v1(reqwest::Method::GET, "/v1/runs", &tokens.access, None))
        .await;
    assert_eq!(
        (status, problem_type(&problem)),
        (400, "invalid-request"),
        "{problem}"
    );
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("Rewind-Organization")
    );
    let (status, problem) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &tokens.access,
            Some("org_not_opened"),
        ))
        .await;
    assert_eq!(
        (status, problem_type(&problem)),
        (403, "forbidden"),
        "{problem}"
    );
    let (status, problem) = world
        .status(
            world
                .v1(
                    reqwest::Method::POST,
                    "/v1/api-keys",
                    &tokens.access,
                    Some(&world.acme),
                )
                .json(&json!({ "name": "x", "role": "reader" })),
        )
        .await;
    assert_eq!(
        (status, problem_type(&problem)),
        (403, "forbidden"),
        "{problem}"
    );

    // The person sees it, and so does the organization.
    let (status, mine) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/connections",
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 200);
    let connections = mine["connections"].as_array().unwrap();
    assert_eq!(connections.len(), 1);
    let connection = &connections[0];
    let id = connection["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("cn_"));
    assert_eq!(connection["person"]["email"], "ada@example.net");
    assert_eq!(connection["client"]["id"], world.document.url.as_str());
    assert!(connection["lastUsedAt"].is_string(), "{connection}");
    let (status, theirs) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/organization/connections",
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 200);
    assert_eq!(theirs["connections"][0]["id"], id.as_str());

    // The Bearer of `/mcp`.
    let running = world.server.running.as_ref().unwrap();
    let caller = running.authenticate_mcp(&tokens.access).await.unwrap();
    assert!(
        matches!(caller.principal, McpPrincipal::Connection { ref id, .. } if id.starts_with("cn_"))
    );
    assert_eq!(caller.organizations.len(), 1);
    assert_eq!(caller.organizations[0].id, world.acme);
    assert_eq!(caller.organizations[0].name.as_deref(), Some("Acme"));
    assert_eq!(caller.organizations[0].permissions.len(), 5);
    assert!(
        running.authenticate_mcp(&world.ada.token).await.is_err(),
        "a member's token"
    );

    // Refreshed: new tokens, the refresh token used.
    let (status, answer) = world
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &tokens.refresh),
                ("client_id", &world.document.url),
            ],
            None,
        )
        .await;
    assert_eq!(status, 200, "{answer}");
    let renewed = Tokens::of(&answer);
    assert_ne!(renewed.access, tokens.access);
    let (status, _) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &renewed.access, None))
        .await;
    assert_eq!(status, 200);

    // Revoked by the client: every token of the connection is refused.
    let response = world
        .form(
            "/oauth/revoke",
            &[
                ("token", renewed.refresh.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", world.document.url.as_str()),
            ],
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for bearer in [&tokens.access, &renewed.access] {
        let (status, problem) = world
            .status(world.v1(reqwest::Method::GET, "/v1/whoami", bearer, None))
            .await;
        assert_eq!((status, problem_type(&problem)), (401, "unauthorized"));
    }
    let (_, mine) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/connections",
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(mine["connections"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_connects_claude_on_turso() {
    let world = World::new(None).await;
    the_whole_flow(&world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_connects_claude_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let world = World::new(Some(&postgres.url())).await;
    the_whole_flow(&world).await;
    // A refresh token used twice revokes the connection there too.
    let tokens = world
        .connect(&world.ada, json!([{ "id": world.acme, "access": "read" }]))
        .await;
    a_reused_refresh_token_revokes(&world, &tokens).await;
}

async fn a_reused_refresh_token_revokes(world: &World, tokens: &Tokens) {
    let refresh = |token: String| async move {
        world
            .token(
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &token),
                    ("client_id", &world.document.url),
                ],
                None,
            )
            .await
    };
    let (status, renewed) = refresh(tokens.refresh.clone()).await;
    assert_eq!(status, 200, "{renewed}");
    let renewed = Tokens::of(&renewed);
    let (status, refused) = refresh(tokens.refresh.clone()).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    assert!(
        refused["error_description"]
            .as_str()
            .unwrap()
            .contains("revoked")
    );
    let (status, _) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &renewed.access, None))
        .await;
    assert_eq!(status, 401, "the connection is revoked");
    let (status, refused) = refresh(renewed.refresh).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
}

/// Exchanges a registered client's code, with HTTP Basic credentials or with its `client_id`
/// alone.
async fn exchange_registered(
    world: &World,
    code: &str,
    id: &str,
    callback: &str,
    basic: Option<(&str, &str)>,
) -> (u16, Value) {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", callback),
        ("code_verifier", VERIFIER),
    ];
    if basic.is_none() {
        form.push(("client_id", id));
    }
    world.token(&form, basic).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_client_connects_with_its_secret() {
    let world = World::new(None).await;
    let response = world
        .http
        .post(world.url("/oauth/register"))
        .json(&json!({
            "client_name": "Cursor",
            "redirect_uris": ["http://127.0.0.1/callback", "cursor://anysphere.cursor-mcp/oauth/callback"],
            "token_endpoint_auth_method": "client_secret_basic",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let registered: Value = response.json().await.unwrap();
    let id = registered["client_id"].as_str().unwrap().to_owned();
    let secret = registered["client_secret"].as_str().unwrap().to_owned();
    assert!(id.starts_with("rc_") && secret.starts_with("rewind_cs_"));

    // A loopback redirect URI matches on any port.
    let callback = "http://127.0.0.1:51234/callback";
    let request = world.request(&id, callback).await;
    let response = world
        .consent(
            &world.bob,
            &request,
            json!([{ "id": world.acme, "access": "run" }]),
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    let redirect = body["redirectTo"].as_str().unwrap();
    assert!(
        redirect.starts_with(&format!("{callback}?code=")),
        "{redirect}"
    );
    let code = redirect
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let (status, refused) = exchange_registered(&world, &code, &id, callback, None).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (401, Some("invalid_client")),
        "a confidential client authenticates"
    );
    let (status, refused) =
        exchange_registered(&world, &code, &id, callback, Some((&id, "rewind_cs_wrong"))).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (401, Some("invalid_client"))
    );
    let (status, answer) =
        exchange_registered(&world, &code, &id, callback, Some((&id, &secret))).await;
    assert_eq!(status, 200, "{answer}");
    let tokens = Tokens::of(&answer);
    let (_, whoami) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &tokens.access, None))
        .await;
    assert_eq!(whoami["principal"]["client"]["kind"], "registered");
    assert_eq!(whoami["principal"]["client"]["name"], "Cursor");
    assert_eq!(
        whoami["principal"]["organizations"][0]["allowedLabels"],
        json!({})
    );

    // Registration refuses what a client may not list.
    for (metadata, error) in [
        (json!({ "redirect_uris": [] }), "invalid_redirect_uri"),
        (
            json!({ "redirect_uris": ["javascript:alert(1)"] }),
            "invalid_redirect_uri",
        ),
        (
            json!({ "redirect_uris": ["http://example.com/cb"] }),
            "invalid_redirect_uri",
        ),
        (
            json!({ "redirect_uris": ["https://a.example/cb"], "grant_types": ["password"] }),
            "invalid_client_metadata",
        ),
    ] {
        let response = world
            .http
            .post(world.url("/oauth/register"))
            .json(&metadata)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{metadata}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], error, "{metadata}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorization_requests_are_refused_where_the_client_cannot_be_trusted() {
    let world = World::new(None).await;
    let params = |client: &str, redirect: &str| -> Vec<(String, String)> {
        [
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", redirect),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "s"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
    };
    let world = &world;
    let authorize = |params: Vec<(String, String)>| async move {
        let pairs: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        world.authorize(&pairs).await
    };
    // Answered here, never redirected.
    for (client, redirect, reason) in [
        ("rc_unknown", CLAUDE_CALLBACK, "no client rc_unknown"),
        ("rc_\u{0}", CLAUDE_CALLBACK, "is registered here"),
        (
            world.document.url.as_str(),
            "https://evil.example/callback",
            "not one of the client's redirect URIs",
        ),
        (
            "https://10.1.2.3/client.json",
            CLAUDE_CALLBACK,
            "private address",
        ),
        (
            "https://169.254.169.254/client.json",
            CLAUDE_CALLBACK,
            "link-local",
        ),
        (
            "http://192.168.1.2/client.json",
            CLAUDE_CALLBACK,
            "https URL",
        ),
        ("https://claude.ai", CLAUDE_CALLBACK, "path"),
    ] {
        let response = authorize(params(client, redirect)).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{client} {redirect}"
        );
        assert_eq!(location(&response), "", "never redirected");
        let problem: Value = response.json().await.unwrap();
        assert_eq!(problem_type(&problem), "invalid-request");
        assert!(
            problem["detail"].as_str().unwrap().contains(reason),
            "{client}: {problem}"
        );
    }
    // Other faults go back to the client.
    let mut plain = params(&world.document.url, CLAUDE_CALLBACK);
    plain[4].1 = "plain".into();
    let response = authorize(plain).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(location(&response).starts_with(&format!("{CLAUDE_CALLBACK}?error=invalid_request")));
    assert!(location(&response).ends_with("&state=s"));
    let mut target = params(&world.document.url, CLAUDE_CALLBACK);
    target.push(("resource".into(), "https://other.example/mcp".into()));
    let response = authorize(target).await;
    assert!(
        location(&response).contains("error=invalid_target"),
        "{}",
        location(&response)
    );
    let mut token = params(&world.document.url, CLAUDE_CALLBACK);
    token[0].1 = "token".into();
    let response = authorize(token).await;
    assert!(location(&response).contains("error=unsupported_response_type"));

    // A request that lapsed: sealed by this server, 10 minutes ago.
    let seal = rewind_workos::CookieSeal::new(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    )
    .unwrap();
    let lapsed = seal
        .seal(&json!({
            "clientId": world.document.url,
            "redirectUri": CLAUDE_CALLBACK,
            "codeChallenge": CHALLENGE,
            "state": "s",
            "resource": RESOURCE,
            "expiresAt": 1_000,
        }))
        .unwrap();
    let (status, problem) = world
        .status(world.v1(
            reqwest::Method::GET,
            &format!(
                "/v1/oauth/consent?{}",
                serde_html_form::to_string([("request", lapsed.as_str())]).unwrap()
            ),
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!((status, problem_type(&problem)), (400, "invalid-request"));
    assert!(
        problem["detail"].as_str().unwrap().contains("lapsed"),
        "{problem}"
    );
    let response = world
        .consent(
            &world.ada,
            &lapsed,
            json!([{ "id": world.acme, "access": "read" }]),
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/oauth/consent?request=forged",
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 400);

    // Denied: back to the client with access_denied.
    let request = world.request(&world.document.url, CLAUDE_CALLBACK).await;
    let (status, denied) = world
        .status(
            world
                .v1(
                    reqwest::Method::POST,
                    "/v1/oauth/consent",
                    &world.ada.token,
                    None,
                )
                .json(&json!({ "request": request, "decision": "deny" })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        denied["redirectTo"],
        format!(
            "{CLAUDE_CALLBACK}?error=access_denied&error_description=the+person+denied+the+request&state=the-state"
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codes_and_refresh_tokens_are_used_once() {
    let world = World::new(None).await;
    // A wrong verifier is refused, and the code stays good.
    let request = world.request(&world.document.url, CLAUDE_CALLBACK).await;
    let code = world
        .code(
            &world.ada,
            &request,
            json!([{ "id": world.acme, "access": "run" }]),
        )
        .await;
    let (status, refused) = world
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", CLAUDE_CALLBACK),
                ("client_id", &world.document.url),
                ("code_verifier", &VERIFIER.replace('d', "e")),
            ],
            None,
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let (status, answer) = world.exchange(&code).await;
    assert_eq!(status, 200, "{answer}");
    let tokens = Tokens::of(&answer);

    // The code exchanged again: refused, and its connection revoked.
    let (status, refused) = world.exchange(&code).await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let (status, _) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &tokens.access, None))
        .await;
    assert_eq!(status, 401, "the connection the code made is revoked");
    let (status, refused) = world.exchange("an-unknown-code").await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );

    // A refresh token used twice revokes its connection.
    let tokens = world
        .connect(&world.ada, json!([{ "id": world.acme, "access": "run" }]))
        .await;
    a_reused_refresh_token_revokes(&world, &tokens).await;

    // Another client's refresh token is refused, and stays good.
    let tokens = world
        .connect(&world.ada, json!([{ "id": world.acme, "access": "run" }]))
        .await;
    let (status, refused) = world
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &tokens.refresh),
                ("client_id", "https://other.example/client.json"),
            ],
            None,
        )
        .await;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let (status, _) = world
        .token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &tokens.refresh),
                ("client_id", &world.document.url),
            ],
            None,
        )
        .await;
    assert_eq!(status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_never_holds_more_than_its_person() {
    let world = World::new(None).await;
    // Bob, a reader of Acme after all: he may open it to read, not to run.
    world
        .owner
        .set_member_grant(
            &world.bob.user,
            &SetGrant {
                role: "reader".into(),
                allowed_labels: Labels::new(),
            },
        )
        .await
        .unwrap();
    let request = world.request(&world.document.url, CLAUDE_CALLBACK).await;
    let (_, consent) = world
        .status(world.v1(
            reqwest::Method::GET,
            &format!(
                "/v1/oauth/consent?{}",
                serde_html_form::to_string([("request", request.as_str())]).unwrap()
            ),
            &world.bob.token,
            None,
        ))
        .await;
    assert_eq!(consent["organizations"][0]["accesses"], json!(["read"]));
    let response = world
        .consent(
            &world.bob,
            &request,
            json!([{ "id": world.acme, "access": "run" }]),
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let problem: Value = response.json().await.unwrap();
    assert!(
        problem["detail"].as_str().unwrap().contains("`runs:start`"),
        "{problem}"
    );

    // An organization where the person holds no grant: Grace's own, and Acme, which she joined
    // in the identity provider without a session there.
    let mut grace = sign_in(&world.server, &world.identity, "grace@example.net").await;
    let globex = world
        .server
        .client(Some(&grace.token))
        .create_organization(&CreateOrganization {
            name: "Globex".into(),
        })
        .await
        .unwrap()
        .id;
    grace.switch(&globex).await;
    let grace_user = rewind_engine::ports::UserId::new(grace.user.clone());
    world
        .identity
        .add_member(&OrganizationId::new(&world.acme), &grace_user)
        .await
        .unwrap();
    let response = world
        .consent(
            &grace,
            &request,
            json!([{ "id": world.acme, "access": "read" }]),
            json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let problem: Value = response.json().await.unwrap();
    assert!(
        problem["detail"].as_str().unwrap().contains("no grant"),
        "{problem}"
    );
    let response = world
        .consent(
            &world.bob,
            &request,
            json!([{ "id": globex, "access": "read" }]),
            json!({}),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "Bob is not a member of Globex"
    );
    let response = world
        .consent(&world.bob, &request, json!([]), json!({}))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "no organization"
    );

    // Bob's read access: it reads, and starts nothing.
    let code = world
        .code(
            &world.bob,
            &request,
            json!([{ "id": world.acme, "access": "read" }]),
        )
        .await;
    let (_, answer) = world.exchange(&code).await;
    let reader = Tokens::of(&answer);
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &reader.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(status, 200);
    let (status, problem) = world
        .status(
            world
                .v1(
                    reqwest::Method::POST,
                    "/v1/runs",
                    &reader.access,
                    Some(&world.acme),
                )
                .header("Rewind-Namespace", "claude")
                .json(
                    &json!({ "script": "export default async function main() {}", "check": false }),
                ),
        )
        .await;
    assert_eq!((status, problem_type(&problem)), (403, "forbidden"));
    assert!(
        problem["detail"].as_str().unwrap().contains("`runs:start`"),
        "{problem}"
    );

    // Bob, a harness again, changes his connection to run: it controls runs (404 for one that
    // does not exist, past the permission).
    world
        .owner
        .set_member_grant(
            &world.bob.user,
            &SetGrant {
                role: "harness".into(),
                allowed_labels: Labels::new(),
            },
        )
        .await
        .unwrap();
    let (_, mine) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/connections",
            &world.bob.token,
            None,
        ))
        .await;
    let id = mine["connections"][0]["id"].as_str().unwrap().to_owned();
    let (status, changed) = world
        .status(
            world
                .v1(
                    reqwest::Method::PATCH,
                    &format!("/v1/connections/{id}"),
                    &world.bob.token,
                    None,
                )
                .json(&json!({ "organizations": [{ "id": world.acme, "access": "run" }] })),
        )
        .await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["organizations"][0]["access"], "run");
    let cancel = |bearer: String| {
        let world = &world;
        async move {
            world
                .status(world.v1(
                    reqwest::Method::POST,
                    "/v1/runs/0190b7a8-0000-7000-8000-000000000000/cancel",
                    &bearer,
                    Some(&world.acme),
                ))
                .await
                .0
        }
    };
    assert_eq!(cancel(reader.access.clone()).await, 404);

    // Someone else's connection is not found.
    let (status, _) = world
        .status(
            world
                .v1(
                    reqwest::Method::PATCH,
                    &format!("/v1/connections/{id}"),
                    &world.ada.token,
                    None,
                )
                .json(&json!({ "organizations": [] })),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::DELETE,
            &format!("/v1/connections/{id}"),
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 404);
    for odd in ["cn_%00", "cn_a%20b"] {
        let (status, _) = world
            .status(world.v1(
                reqwest::Method::DELETE,
                &format!("/v1/connections/{odd}"),
                &world.bob.token,
                None,
            ))
            .await;
        assert_eq!(status, 404, "{odd}: never reaches the store");
    }

    // The person's grant narrowed: the connection follows at its next request.
    world
        .owner
        .set_member_grant(
            &world.bob.user,
            &SetGrant {
                role: "reader".into(),
                allowed_labels: Labels::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(cancel(reader.access.clone()).await, 403);
    let (_, whoami) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/whoami",
            &reader.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(
        whoami["permissions"],
        json!(["runs:read", "toolsets:read", "namespaces:read"])
    );

    // An organization's administrator removes it from the connection.
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::DELETE,
            &format!("/v1/organization/connections/{id}"),
            &world.bob.token,
            None,
        ))
        .await;
    assert_eq!(status, 403, "members:manage");
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::DELETE,
            &format!("/v1/organization/connections/{id}"),
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 204);
    let (status, problem) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &reader.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!((status, problem_type(&problem)), (403, "forbidden"));
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("does not open"),
        "{problem}"
    );
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::DELETE,
            &format!("/v1/organization/connections/{id}"),
            &world.ada.token,
            None,
        ))
        .await;
    assert_eq!(status, 404);

    // Removing a member removes their connections' grants there.
    let tokens = world
        .connect(&world.bob, json!([{ "id": world.acme, "access": "read" }]))
        .await;
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &tokens.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(status, 200);
    world.owner.remove_member(&world.bob.user).await.unwrap();
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &tokens.access,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(status, 403);
    let (_, whoami) = world
        .status(world.v1(reqwest::Method::GET, "/v1/whoami", &tokens.access, None))
        .await;
    assert_eq!(whoami["principal"]["organizations"], json!([]));
    let caller = world
        .server
        .running
        .as_ref()
        .unwrap()
        .authenticate_mcp(&tokens.access)
        .await
        .unwrap();
    assert_eq!(caller.organizations, []);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_callers_repeat_their_own_organization() {
    let world = World::new(None).await;
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &world.ada.token,
            Some(&world.acme),
        ))
        .await;
    assert_eq!(status, 200);
    let (status, problem) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &world.ada.token,
            Some("org_other"),
        ))
        .await;
    assert_eq!((status, problem_type(&problem)), (403, "forbidden"));
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/whoami",
            &world.ada.token,
            Some("org_other"),
        ))
        .await;
    assert_eq!(status, 403);
    let (status, _) = world
        .status(world.v1(
            reqwest::Method::GET,
            "/v1/runs",
            &world.ada.token,
            Some("not an id"),
        ))
        .await;
    assert_eq!(status, 400);

    // An API key at `/mcp`: its tenant.
    let key = world
        .server
        .admin
        .create_api_key(&rewind_api::access::CreateApiKey {
            tenant: Some(world.acme.clone()),
            name: "harness".into(),
            role: "harness".into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap()
        .key
        .unwrap();
    let caller = world
        .server
        .running
        .as_ref()
        .unwrap()
        .authenticate_mcp(&key)
        .await
        .unwrap();
    assert!(matches!(caller.principal, McpPrincipal::Key { .. }));
    assert_eq!(caller.organizations.len(), 1);
    assert_eq!(caller.organizations[0].id, world.acme);
    assert_eq!(caller.organizations[0].name.as_deref(), Some("Acme"));
    assert!(caller.organizations[0].permissions.len() > 5);
    assert!(
        world
            .server
            .running
            .as_ref()
            .unwrap()
            .authenticate_mcp("rewind_ct_unknown")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_public_url_there_is_no_oauth() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    let http = reqwest::Client::new();
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource/mcp",
        "/oauth/authorize?client_id=rc_x",
    ] {
        let response = http
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let response = http
        .post(format!("{}/oauth/token", server.url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("grant_type=refresh_token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let info = server.admin.server_info().await.unwrap();
    assert_eq!((info.mcp_url, info.oauth), (None, false));
}

// -------------------------------------------------------------------------------------------
// `/mcp` (`archi/07-code-architecture.md`, "MCP and OAuth")

/// One exchange with `/mcp`, as an MCP client without session sends it: the status, the
/// `WWW-Authenticate` header, and the JSON-RPC answer.
async fn mcp(
    world: &World,
    bearer: Option<&str>,
    method: &str,
    params: Value,
) -> (u16, String, Value) {
    let mut request = world
        .http
        .post(world.url("/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let challenge = response
        .headers()
        .get("www-authenticate")
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, challenge, body)
}

/// `tools/call`: the text of its result, and whether it is an error.
async fn call(world: &World, bearer: &str, tool: &str, arguments: Value) -> (String, bool) {
    let (status, _, body) = mcp(
        world,
        Some(bearer),
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let result = &body["result"];
    assert!(result.is_object(), "{tool}: {body}");
    let text = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|content| content["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (text, result["isError"].as_bool().unwrap_or(false))
}

async fn tool_names(world: &World, bearer: &str) -> Vec<String> {
    let (status, _, body) = mcp(world, Some(bearer), "tools/list", json!({})).await;
    assert_eq!(status, 200, "{body}");
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_answers_401_with_the_resource_metadata() {
    let world = World::new(None).await;
    let metadata =
        "resource_metadata=\"https://api.rewind.test/.well-known/oauth-protected-resource/mcp\"";

    let (status, challenge, _) = mcp(&world, None, "tools/list", json!({})).await;
    assert_eq!(status, 401);
    assert_eq!(challenge, format!("Bearer {metadata}"));

    let (status, challenge, _) =
        mcp(&world, Some("rewind_ct_unknown"), "tools/list", json!({})).await;
    assert_eq!(status, 401);
    assert_eq!(
        challenge,
        format!("Bearer {metadata}, error=\"invalid_token\"")
    );

    // A member's access token is the dashboard's, never an MCP client's.
    let (status, challenge, _) = mcp(&world, Some(&world.ada.token), "tools/list", json!({})).await;
    assert_eq!(status, 401);
    assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");

    // A revoked connection is refused at its next request.
    let tokens = world
        .connect(&world.ada, json!([{ "id": world.acme, "access": "read" }]))
        .await;
    assert!(!tool_names(&world, &tokens.access).await.is_empty());
    let (status, _) = world
        .status(world.form(
            "/oauth/revoke",
            &[
                ("token", &tokens.refresh),
                ("client_id", &world.document.url),
            ],
        ))
        .await;
    assert_eq!(status, 200);
    let (status, _, _) = mcp(&world, Some(&tokens.access), "tools/list", json!({})).await;
    assert_eq!(status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_runs_a_script_over_mcp() {
    let world = World::new(None).await;
    let tokens = world
        .connect(&world.ada, json!([{ "id": world.acme, "access": "run" }]))
        .await;

    let (status, _, body) = mcp(
        &world,
        Some(&tokens.access),
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "claude-test", "version": "1" }
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["serverInfo"]["name"], "Rewind");
    assert!(
        body["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("list_orgs")
    );

    let names = tool_names(&world, &tokens.access).await;
    for name in [
        "list_orgs",
        "list_namespaces",
        "list_toolsets",
        "run_script",
        "cancel_run",
    ] {
        assert!(names.iter().any(|tool| tool == name), "{name} in {names:?}");
    }
    assert!(
        !names.iter().any(|tool| tool == "save_workflow"),
        "{names:?}"
    );

    let (text, error) = call(&world, &tokens.access, "list_orgs", json!({})).await;
    assert!(!error, "{text}");
    assert!(text.contains(&world.acme), "{text}");

    // The namespace is named per call; without it, the model is told to give one.
    let script = "export default async function main() {\n  return 41 + 1;\n}\n";
    let (text, error) = call(
        &world,
        &tokens.access,
        "run_script",
        json!({ "code": script }),
    )
    .await;
    assert!(error, "{text}");
    assert!(text.contains("namespace"), "{text}");

    let (text, error) = call(
        &world,
        &tokens.access,
        "run_script",
        json!({ "code": script, "namespace": "demo", "wait": "30s" }),
    )
    .await;
    assert!(!error, "{text}");
    assert!(text.contains("42"), "{text}");

    // The run is in the namespace named, with the connection's label, once the read model has
    // it.
    let mut page = Value::Null;
    for _ in 0..50 {
        let (status, answer) = world
            .status(world.v1(
                reqwest::Method::GET,
                "/v1/runs?q=%40via%3Aclaude",
                &world.ada.token,
                None,
            ))
            .await;
        assert_eq!(status, 200, "{answer}");
        page = answer;
        if !page["runs"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let runs = page["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{page}");
    assert_eq!(runs[0]["namespace"], "demo");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_tools_follow_the_access_given() {
    let world = World::new(None).await;
    // Bob holds `harness` in Acme, and opens it to Claude for reading only.
    let tokens = world
        .connect(&world.bob, json!([{ "id": world.acme, "access": "read" }]))
        .await;
    let names = tool_names(&world, &tokens.access).await;
    assert!(names.iter().any(|tool| tool == "get_run"), "{names:?}");
    assert!(!names.iter().any(|tool| tool == "run_script"), "{names:?}");
    let (text, error) = call(
        &world,
        &tokens.access,
        "run_script",
        json!({ "code": "export default () => 1;\n", "namespace": "demo" }),
    )
    .await;
    assert!(error, "{text}");

    // An API key reaches its own tenant, with its role's permissions.
    let key = world
        .owner
        .create_api_key(&rewind_api::access::CreateApiKey {
            tenant: None,
            name: "ci".into(),
            role: "reader".into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap()
        .key
        .unwrap();
    let (text, error) = call(&world, &key, "list_orgs", json!({})).await;
    assert!(!error, "{text}");
    assert!(text.contains(&world.acme), "{text}");
    let names = tool_names(&world, &key).await;
    assert!(!names.iter().any(|tool| tool == "run_script"), "{names:?}");
}
