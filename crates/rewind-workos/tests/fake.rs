//! `WorkOs` against a fake WorkOS over HTTP (wiremock): every call, every error mapped, and
//! access tokens signed with test RSA keys served as a JWKS.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use rewind_core::Timestamp;
use rewind_engine::ports::{
    AccessClaims, Authentication, Client, IdentityError, IdentityProvider, InvitationId,
    InvitationState, MembershipId, MembershipStatus, Organization, OrganizationId, PendingToken,
    Provider, Session, SessionId, UserId,
};
use rewind_workos::{ISSUER, WorkOs};
use serde_json::{Value, json};
use wiremock::matchers::{
    body_json, body_partial_json, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const CLIENT_ID: &str = "client_01TEST";
const API_KEY: &str = "sk_test_never_printed";
const JWKS_PATH: &str = "/sso/jwks/client_01TEST";
const AUTHENTICATE: &str = "/user_management/authenticate";
const AT: &str = "2026-11-14T10:00:00.000Z";

// ---------------------------------------------------------------------------------------------
// Keys and tokens

struct TestKey {
    kid: String,
    pair: KeyPair,
}

impl TestKey {
    fn new(kid: &str) -> Self {
        Self {
            kid: kid.to_owned(),
            pair: KeyPair::generate(KeySize::Rsa2048).expect("an RSA key"),
        }
    }

    fn jwk(&self) -> Value {
        let public = self.pair.public_key();
        json!({
            "kty": "RSA",
            "alg": "RS256",
            "use": "sig",
            "kid": self.kid,
            "n": BASE64_URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
            "e": BASE64_URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
        })
    }

    /// A JWT of `claims`, signed with this key but announcing `kid`.
    fn sign_as(&self, kid: &str, alg: &str, claims: &Value) -> String {
        let header = json!({ "alg": alg, "kid": kid, "typ": "JWT" });
        let input = format!(
            "{}.{}",
            BASE64_URL_SAFE_NO_PAD.encode(header.to_string()),
            BASE64_URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.pair.public_modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .expect("signed");
        format!("{input}.{}", BASE64_URL_SAFE_NO_PAD.encode(signature))
    }

    fn sign(&self, claims: &Value) -> String {
        self.sign_as(&self.kid, "RS256", claims)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}

/// The claims of a WorkOS access token, with the role and permission claims Rewind ignores.
fn claims(user: &str, session: &str, organization: Option<&str>) -> Value {
    let mut claims = json!({
        "iss": ISSUER,
        "sub": user,
        "sid": session,
        "jti": "01JTOKEN",
        "client_id": CLIENT_ID,
        "auth_time": now(),
        "iat": now(),
        "exp": now() + 300,
        "role": "admin",
        "roles": ["admin"],
        "permissions": ["everything"],
    });
    if let Some(organization) = organization {
        claims["org_id"] = json!(organization);
    }
    claims
}

fn with(mut claims: Value, key: &str, value: Value) -> Value {
    claims[key] = value;
    claims
}

fn without(mut claims: Value, key: &str) -> Value {
    claims.as_object_mut().expect("an object").remove(key);
    claims
}

/// The JWKS the fake serves, which a test may rotate.
#[derive(Clone)]
struct Jwks(Arc<Mutex<Value>>);

impl Respond for Jwks {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(self.0.lock().expect("jwks").clone())
    }
}

// ---------------------------------------------------------------------------------------------
// The fake

struct Fake {
    server: MockServer,
    key: TestKey,
    jwks: Jwks,
    identity: WorkOs,
}

impl Fake {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let key = TestKey::new("key_a");
        let jwks = Jwks(Arc::new(Mutex::new(json!({ "keys": [key.jwk()] }))));
        Mock::given(method("GET"))
            .and(path(JWKS_PATH))
            .respond_with(jwks.clone())
            .mount(&server)
            .await;
        let identity =
            WorkOs::with_base_url(CLIENT_ID, API_KEY, &server.uri()).expect("configured");
        Self {
            server,
            key,
            jwks,
            identity,
        }
    }

    fn serve_keys(&self, keys: &[&TestKey]) {
        *self.jwks.0.lock().expect("jwks") =
            json!({ "keys": keys.iter().map(|key| key.jwk()).collect::<Vec<_>>() });
    }

    async fn jwks_fetches(&self) -> usize {
        self.requests_to(JWKS_PATH).await.len()
    }

    async fn requests_to(&self, to: &str) -> Vec<Request> {
        self.server
            .received_requests()
            .await
            .expect("recorded")
            .into_iter()
            .filter(|request| request.url.path() == to)
            .collect()
    }

    /// Answers `POST /user_management/authenticate` whose body has `body`.
    async fn authenticate(&self, body: Value, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path(AUTHENTICATE))
            .and(body_partial_json(body))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    async fn answer(&self, verb: &str, to: &str, response: ResponseTemplate) {
        Mock::given(method(verb))
            .and(path(to))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    /// WorkOS's answer to a sign-in: the user, and a token signed with the fake's key.
    fn signed_in(&self, user: &str, session: &str, organization: Option<&str>) -> Value {
        json!({
            "user": user_json(user, "ada@example.net"),
            "organization_id": organization,
            "access_token": self.key.sign(&claims(user, session, organization)),
            "refresh_token": format!("refresh-{session}"),
            "authentication_method": "MagicAuth",
        })
    }
}

fn user_json(id: &str, email: &str) -> Value {
    json!({
        "object": "user",
        "id": id,
        "email": email,
        "email_verified": true,
        "first_name": "Ada",
        "last_name": null,
        "name": "Ada",
        "profile_picture_url": "https://example.net/ada.png",
        "metadata": {},
        "created_at": AT,
        "updated_at": AT,
    })
}

fn organization_json(id: &str, name: &str) -> Value {
    json!({
        "object": "organization",
        "id": id,
        "name": name,
        "domains": [],
        "metadata": {},
        "external_id": null,
        "allow_profiles_outside_organization": false,
        "created_at": AT,
        "updated_at": AT,
    })
}

fn membership_json(id: &str, user: &str, organization: &str, name: Option<&str>) -> Value {
    json!({
        "object": "organization_membership",
        "id": id,
        "user_id": user,
        "organization_id": organization,
        "organization_name": name,
        "status": "active",
        "directory_managed": false,
        "role": { "slug": "admin" },
        "roles": [{ "slug": "admin" }],
        "custom_attributes": {},
        "created_at": AT,
        "updated_at": AT,
        "user": user_json(user, &format!("{user}@example.net")),
    })
}

fn invitation_json(id: &str, state: &str, organization: Option<&str>) -> Value {
    json!({
        "object": "invitation",
        "id": id,
        "email": "bob@example.net",
        "state": state,
        "accepted_at": null,
        "revoked_at": null,
        "expires_at": "2026-11-21T10:00:00.000Z",
        "organization_id": organization,
        "inviter_user_id": "user_ada",
        "accepted_user_id": if state == "accepted" { json!("user_bob") } else { Value::Null },
        "role_slug": "member",
        "created_at": AT,
        "updated_at": AT,
        "token": "invitation-token-never-read",
        "accept_invitation_url": "https://example.net/accept?token=invitation-token-never-read",
    })
}

fn list(data: Vec<Value>, after: Option<&str>) -> Value {
    json!({ "object": "list", "data": data, "list_metadata": { "before": null, "after": after } })
}

fn error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "code": code, "message": message }))
}

fn oauth_error(status: u16, code: &str, description: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(json!({ "error": code, "error_description": description }))
}

fn not_found(entity: &str) -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({
        "code": "entity_not_found",
        "message": format!("Not found: '{entity}'."),
        "entity_id": entity,
    }))
}

fn browser() -> Client {
    Client {
        ip_address: Some("203.0.113.7".into()),
        user_agent: Some("Mozilla/5.0 (test)".into()),
    }
}

fn session(authentication: Authentication) -> Session {
    match authentication {
        Authentication::Authenticated(session) => session,
        other => panic!("not signed in: {other:?}"),
    }
}

fn refused(result: Result<impl std::fmt::Debug, IdentityError>) -> (String, String) {
    match result {
        Err(IdentityError::Refused { code, message }) => (code, message),
        other => panic!("not refused: {other:?}"),
    }
}

fn unavailable(result: Result<impl std::fmt::Debug, IdentityError>) -> String {
    match result {
        Err(IdentityError::Unavailable(message)) => message,
        other => panic!("not unavailable: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Sign-in

#[tokio::test]
async fn the_authorization_url_sends_the_browser_to_github() {
    let fake = Fake::start().await;
    let url = fake
        .identity
        .authorization_url(
            Provider::GitHub,
            "https://app.example.net/auth/callback",
            "state-123",
        )
        .unwrap();
    assert!(
        url.starts_with(&format!("{}/user_management/authorize?", fake.server.uri())),
        "{url}"
    );
    for part in [
        "provider=GitHubOAuth",
        "response_type=code",
        "client_id=client_01TEST",
        "redirect_uri=https%3A%2F%2Fapp.example.net%2Fauth%2Fcallback",
        "state=state-123",
    ] {
        assert!(url.contains(part), "{part} missing from {url}");
    }
    assert!(!url.contains(API_KEY));
}

#[tokio::test]
async fn a_code_by_email_signs_in_with_the_browser_and_the_invitation() {
    let fake = Fake::start().await;
    Mock::given(method("POST"))
        .and(path("/user_management/magic_auth"))
        .and(body_json(json!({
            "email": "ada@example.net",
            "invitation_token": "inv-token",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "object": "magic_auth",
            "id": "magic_auth_1",
            "user_id": "user_ada",
            "email": "ada@example.net",
            "code": "123456",
            "expires_at": AT,
            "created_at": AT,
            "updated_at": AT,
        })))
        .expect(1)
        .mount(&fake.server)
        .await;
    fake.authenticate(
        json!({
            "grant_type": "urn:workos:oauth:grant-type:magic-auth:code",
            "client_id": CLIENT_ID,
            "client_secret": API_KEY,
            "code": "123456",
            "email": "ada@example.net",
            "invitation_token": "inv-token",
            "ip_address": "203.0.113.7",
            "user_agent": "Mozilla/5.0 (test)",
        }),
        ResponseTemplate::new(200).set_body_json(fake.signed_in(
            "user_ada",
            "session_1",
            Some("org_acme"),
        )),
    )
    .await;

    fake.identity
        .send_magic_code(" ada@example.net ", Some("inv-token"))
        .await
        .unwrap();
    let signed_in = session(
        fake.identity
            .authenticate_magic_code("ada@example.net", "123456", Some("inv-token"), &browser())
            .await
            .unwrap(),
    );
    assert_eq!(signed_in.user.id, UserId::new("user_ada"));
    assert_eq!(signed_in.user.email, "ada@example.net");
    assert_eq!(signed_in.user.first_name.as_deref(), Some("Ada"));
    assert_eq!(
        signed_in.user.picture_url.as_deref(),
        Some("https://example.net/ada.png")
    );
    assert!(signed_in.user.email_verified);
    assert_eq!(
        signed_in.organization,
        Some(OrganizationId::new("org_acme"))
    );
    assert_eq!(signed_in.tokens.refresh_token, "refresh-session_1");
    assert_eq!(
        signed_in.claims,
        fake.identity
            .verify_access_token(&signed_in.tokens.access_token)
            .await
            .unwrap()
    );
    assert_eq!(signed_in.claims.session, SessionId::new("session_1"));
    assert_eq!(
        signed_in.claims.organization,
        Some(OrganizationId::new("org_acme"))
    );
}

#[tokio::test]
async fn github_s_code_is_exchanged() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({
            "grant_type": "authorization_code",
            "code": "github-code",
            "invitation_token": "inv-token",
            "ip_address": "203.0.113.7",
            "user_agent": "Mozilla/5.0 (test)",
        }),
        ResponseTemplate::new(200).set_body_json(fake.signed_in("user_ada", "session_1", None)),
    )
    .await;
    let signed_in = session(
        fake.identity
            .authenticate_code("github-code", Some("inv-token"), &browser())
            .await
            .unwrap(),
    );
    assert_eq!(signed_in.organization, None);
    assert_eq!(signed_in.claims.organization, None);
    assert_eq!(signed_in.claims.user, UserId::new("user_ada"));
}

#[tokio::test]
async fn several_organizations_ask_for_one() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({ "grant_type": "urn:workos:oauth:grant-type:magic-auth:code" }),
        ResponseTemplate::new(403).set_body_json(json!({
            "code": "organization_selection_required",
            "message": "The user must choose an organization to finish their authentication.",
            "pending_authentication_token": "pending-1",
            "user": user_json("user_ada", "ada@example.net"),
            "organizations": [
                { "id": "org_acme", "name": "Acme" },
                { "id": "org_beta", "name": "Beta" },
            ],
        })),
    )
    .await;
    fake.authenticate(
        json!({
            "grant_type": "urn:workos:oauth:grant-type:organization-selection",
            "pending_authentication_token": "pending-1",
            "organization_id": "org_beta",
            "ip_address": "203.0.113.7",
        }),
        ResponseTemplate::new(200).set_body_json(fake.signed_in(
            "user_ada",
            "session_1",
            Some("org_beta"),
        )),
    )
    .await;

    let Authentication::OrganizationSelectionRequired {
        pending,
        user,
        organizations,
    } = fake
        .identity
        .authenticate_magic_code("ada@example.net", "123456", None, &browser())
        .await
        .unwrap()
    else {
        panic!("an organization is asked for");
    };
    assert_eq!(pending.expose(), "pending-1");
    assert_eq!(user.id, UserId::new("user_ada"));
    assert_eq!(
        organizations,
        [
            Organization {
                id: OrganizationId::new("org_acme"),
                name: "Acme".into()
            },
            Organization {
                id: OrganizationId::new("org_beta"),
                name: "Beta".into()
            },
        ]
    );
    let signed_in = session(
        fake.identity
            .select_organization(&pending, &OrganizationId::new("org_beta"), &browser())
            .await
            .unwrap(),
    );
    assert_eq!(
        signed_in.claims.organization,
        Some(OrganizationId::new("org_beta"))
    );
}

#[tokio::test]
async fn an_unverified_email_is_verified_with_a_code() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({ "grant_type": "authorization_code" }),
        ResponseTemplate::new(403).set_body_json(json!({
            "code": "email_verification_required",
            "message": "Email ownership must be verified before authentication.",
            "pending_authentication_token": "pending-2",
            "email": "ada@example.net",
            "email_verification_id": "email_verification_1",
        })),
    )
    .await;
    fake.authenticate(
        json!({
            "grant_type": "urn:workos:oauth:grant-type:email-verification:code",
            "pending_authentication_token": "pending-2",
            "code": "654321",
            "user_agent": "Mozilla/5.0 (test)",
        }),
        ResponseTemplate::new(200).set_body_json(fake.signed_in("user_ada", "session_1", None)),
    )
    .await;

    let Authentication::EmailVerificationRequired { pending, email } = fake
        .identity
        .authenticate_code("github-code", None, &browser())
        .await
        .unwrap()
    else {
        panic!("the email is to be verified");
    };
    assert_eq!(email, "ada@example.net");
    session(
        fake.identity
            .verify_email(&pending, "654321", &browser())
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn a_closed_sign_up_is_an_answer() {
    let fake = Fake::start().await;
    let closed = oauth_error(
        403,
        "sign_up_not_allowed",
        "The request failed due to: invitation_invalid.",
    );
    fake.answer("POST", "/user_management/magic_auth", closed.clone())
        .await;
    fake.authenticate(json!({}), closed).await;

    // The same answer as for an email with an account.
    fake.identity
        .send_magic_code("eve@example.net", None)
        .await
        .unwrap();
    assert_eq!(
        fake.identity
            .authenticate_magic_code(" Eve@Example.net ", "123456", None, &browser())
            .await
            .unwrap(),
        Authentication::SignUpClosed {
            email: Some("eve@example.net".into())
        }
    );
    assert_eq!(
        fake.identity
            .authenticate_code("github-code", None, &browser())
            .await
            .unwrap(),
        Authentication::SignUpClosed { email: None }
    );
}

#[tokio::test]
async fn refresh_switches_organization_and_ended_sessions_are_invalid() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({
            "grant_type": "refresh_token",
            "refresh_token": "refresh-good",
            "organization_id": "org_beta",
            "ip_address": "203.0.113.7",
            "user_agent": "Mozilla/5.0 (test)",
        }),
        ResponseTemplate::new(200).set_body_json(fake.signed_in(
            "user_ada",
            "session_1",
            Some("org_beta"),
        )),
    )
    .await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-ended" }),
        oauth_error(400, "invalid_grant", "Session has already ended."),
    )
    .await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-good", "organization_id": "org_other" }),
        oauth_error(
            400,
            "organization_membership_not_found",
            "User is not a member of the organization.",
        ),
    )
    .await;

    let switched = session(
        fake.identity
            .refresh(
                "refresh-good",
                Some(&OrganizationId::new("org_beta")),
                &browser(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(switched.organization, Some(OrganizationId::new("org_beta")));
    assert_eq!(
        fake.identity
            .refresh("refresh-ended", None, &browser())
            .await,
        Err(IdentityError::InvalidToken)
    );
    let (code, _) = refused(
        fake.identity
            .refresh(
                "refresh-good",
                Some(&OrganizationId::new("org_other")),
                &browser(),
            )
            .await,
    );
    assert_eq!(code, "organization_membership_not_found");
}

#[tokio::test]
async fn refusals_never_repeat_the_secrets_sent() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({ "grant_type": "urn:workos:oauth:grant-type:organization-selection", "pending_authentication_token": "pending-expired" }),
        error(400, "invalid_pending_authentication_token", "The pending_authentication_token has expired or is invalid."),
    )
    .await;
    fake.authenticate(
        json!({ "grant_type": "urn:workos:oauth:grant-type:email-verification:code", "pending_authentication_token": "pending-expired" }),
        error(400, "invalid_pending_authentication_token", "The pending_authentication_token has expired or is invalid."),
    )
    .await;
    fake.authenticate(
        json!({ "grant_type": "urn:workos:oauth:grant-type:organization-selection", "pending_authentication_token": "pending-secret" }),
        error(
            400,
            "user_not_member_of_organization",
            "The user for pending authentication pending-secret does not belong to the organization 'org_other'.",
        ),
    )
    .await;
    fake.authenticate(
        json!({ "grant_type": "authorization_code", "code": "code-secret" }),
        oauth_error(
            400,
            "invalid_grant",
            "The code 'code-secret' has expired or is invalid.",
        ),
    )
    .await;
    fake.authenticate(
        json!({ "grant_type": "urn:workos:oauth:grant-type:magic-auth:code" }),
        error(400, "invalid_one_time_code", "Invalid one-time code"),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/magic_auth",
        not_found("invitation-secret"),
    )
    .await;

    let expired = PendingToken::new("pending-expired");
    assert_eq!(
        fake.identity
            .select_organization(&expired, &OrganizationId::new("org_acme"), &browser())
            .await,
        Err(IdentityError::InvalidToken)
    );
    assert_eq!(
        fake.identity
            .verify_email(&expired, "123456", &browser())
            .await,
        Err(IdentityError::InvalidToken)
    );
    let (code, message) = refused(
        fake.identity
            .select_organization(
                &PendingToken::new("pending-secret"),
                &OrganizationId::new("org_other"),
                &browser(),
            )
            .await,
    );
    assert_eq!(code, "user_not_member_of_organization");
    assert_eq!(
        message,
        "The user for pending authentication <redacted> does not belong to the organization 'org_other'."
    );
    // A spent or unknown code is refused: it is not one of the port's tokens.
    let (code, message) = refused(
        fake.identity
            .authenticate_code("code-secret", None, &browser())
            .await,
    );
    assert_eq!(code, "invalid_grant");
    assert!(!message.contains("code-secret"), "{message}");
    let (code, _) = refused(
        fake.identity
            .authenticate_magic_code("ada@example.net", "000000", None, &browser())
            .await,
    );
    assert_eq!(code, "invalid_one_time_code");
    // An unknown invitation's token is refused, never named.
    let (code, message) = refused(
        fake.identity
            .send_magic_code("ada@example.net", Some("invitation-secret"))
            .await,
    );
    assert_eq!(code, "entity_not_found");
    assert!(!message.contains("invitation-secret"), "{message}");
}

#[tokio::test]
async fn workos_failing_is_unavailable() {
    let fake = Fake::start().await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-401" }),
        error(401, "unauthorized", "Unauthorized"),
    )
    .await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-429" }),
        error(429, "rate_limit_exceeded", "Slow down"),
    )
    .await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-500" }),
        error(500, "internal_error", "Boom"),
    )
    .await;
    fake.authenticate(
        json!({ "refresh_token": "refresh-garbage" }),
        ResponseTemplate::new(200).set_body_json(json!({ "user": "not a user" })),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/organization_memberships",
        ResponseTemplate::new(503).set_body_string("upstream timeout"),
    )
    .await;

    for refresh_token in [
        "refresh-401",
        "refresh-429",
        "refresh-500",
        "refresh-garbage",
    ] {
        let message = unavailable(fake.identity.refresh(refresh_token, None, &browser()).await);
        assert!(!message.contains(API_KEY), "{message}");
        assert!(!message.contains(refresh_token), "{message}");
    }
    unavailable(
        fake.identity
            .add_member(&OrganizationId::new("org_acme"), &UserId::new("user_ada"))
            .await,
    );

    // Nothing listens on the port: the connection fails.
    let unreachable = WorkOs::with_base_url(CLIENT_ID, API_KEY, "http://127.0.0.1:9").unwrap();
    let message = unavailable(unreachable.create_organization("Acme").await);
    assert!(message.contains("cannot be reached"), "{message}");
    let message = unavailable(
        unreachable
            .verify_access_token(&fake.key.sign(&claims("user_ada", "session_1", None)))
            .await,
    );
    assert!(message.contains("keys"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// Access tokens

#[tokio::test]
async fn access_tokens_are_checked_against_the_jwks() {
    let fake = Fake::start().await;
    let good = claims("user_ada", "session_1", Some("org_acme"));
    let verified = fake
        .identity
        .verify_access_token(&fake.key.sign(&good))
        .await
        .unwrap();
    assert_eq!(
        verified,
        AccessClaims {
            user: UserId::new("user_ada"),
            session: SessionId::new("session_1"),
            organization: Some(OrganizationId::new("org_acme")),
            expires_at: Timestamp::from_millis(good["exp"].as_u64().unwrap() * 1000),
        }
    );
    let no_organization = fake
        .identity
        .verify_access_token(&fake.key.sign(&claims("user_ada", "session_1", None)))
        .await
        .unwrap();
    assert_eq!(no_organization.organization, None);
    // Within the 30 s of leeway.
    fake.identity
        .verify_access_token(&fake.key.sign(&with(good.clone(), "exp", json!(now() - 20))))
        .await
        .unwrap();

    let other = TestKey::new("key_other");
    let invalid = [
        (
            "signed by another key",
            other.sign_as("key_a", "RS256", &good),
        ),
        (
            "another issuer",
            fake.key.sign(&with(
                good.clone(),
                "iss",
                json!("https://evil.example.net"),
            )),
        ),
        (
            "the issuer with a slash",
            fake.key
                .sign(&with(good.clone(), "iss", json!("https://api.workos.com/"))),
        ),
        ("no issuer", fake.key.sign(&without(good.clone(), "iss"))),
        (
            "another client",
            fake.key
                .sign(&with(good.clone(), "client_id", json!("client_other"))),
        ),
        (
            "no client",
            fake.key.sign(&without(good.clone(), "client_id")),
        ),
        (
            "expired",
            fake.key.sign(&with(good.clone(), "exp", json!(now() - 31))),
        ),
        ("no expiry", fake.key.sign(&without(good.clone(), "exp"))),
        ("no session", fake.key.sign(&without(good.clone(), "sid"))),
        (
            "another kind of token",
            fake.key
                .sign(&with(good.clone(), "sub_type", json!("agent"))),
        ),
        ("not RS256", fake.key.sign_as("key_a", "RS384", &good)),
        ("no key id", fake.key.sign_as("", "RS256", &good)),
        ("not a JWT", "not.a.jwt".to_owned()),
    ];
    for (why, token) in invalid {
        assert_eq!(
            fake.identity.verify_access_token(&token).await,
            Err(IdentityError::InvalidToken),
            "{why}"
        );
    }
    assert_eq!(fake.jwks_fetches().await, 1, "the key set is cached");
}

#[tokio::test]
async fn an_unknown_key_fetches_the_jwks_again() {
    let fake = Fake::start().await;
    fake.identity
        .verify_access_token(&fake.key.sign(&claims("user_ada", "session_1", None)))
        .await
        .unwrap();
    assert_eq!(fake.jwks_fetches().await, 1);

    // WorkOS rotates its key.
    let rotated = TestKey::new("key_b");
    fake.serve_keys(&[&fake.key, &rotated]);
    let claims = fake
        .identity
        .verify_access_token(&rotated.sign(&claims("user_ada", "session_2", None)))
        .await
        .unwrap();
    assert_eq!(claims.session, SessionId::new("session_2"));
    assert_eq!(fake.jwks_fetches().await, 2);

    // A key still unknown right after does not call WorkOS again.
    let forged = TestKey::new("key_forged");
    assert_eq!(
        fake.identity
            .verify_access_token(&forged.sign(&claims_for("user_ada")))
            .await,
        Err(IdentityError::InvalidToken)
    );
    assert_eq!(fake.jwks_fetches().await, 2);
}

fn claims_for(user: &str) -> Value {
    claims(user, "session_x", None)
}

#[tokio::test]
async fn keys_that_cannot_be_fetched_make_the_provider_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(JWKS_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let identity = WorkOs::with_base_url(CLIENT_ID, API_KEY, &server.uri()).unwrap();
    let key = TestKey::new("key_a");
    unavailable(
        identity
            .verify_access_token(&key.sign(&claims_for("user_ada")))
            .await,
    );
}

// ---------------------------------------------------------------------------------------------
// Sessions and the waitlist

#[tokio::test]
async fn sessions_are_revoked_and_the_waitlist_joined() {
    let fake = Fake::start().await;
    Mock::given(method("POST"))
        .and(path("/user_management/sessions/revoke"))
        .and(body_json(json!({ "session_id": "session_1" })))
        .respond_with(ResponseTemplate::new(200))
        .mount(&fake.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/user_management/sessions/revoke"))
        .and(body_json(json!({ "session_id": "session_gone" })))
        .respond_with(not_found("session_gone"))
        .mount(&fake.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/user_management/waitlists/default/entries"))
        .and(body_json(
            json!({ "email": "eve@example.net", "send_confirmation_email": true }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "object": "waitlist_entry",
            "id": "wl_user_1",
            "email": "eve@example.net",
            "state": "pending",
            "approved_at": null,
            "additional_fields": {},
            "waitlist_id": "waitlist_1",
            "created_at": AT,
            "updated_at": AT,
        })))
        .mount(&fake.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/user_management/waitlists/default/entries"))
        .and(body_partial_json(json!({ "email": "ada@example.net" })))
        .respond_with(error(
            422,
            "user_already_exists",
            "A user with this email already exists.",
        ))
        .mount(&fake.server)
        .await;

    fake.identity
        .revoke_session(&SessionId::new("session_1"))
        .await
        .unwrap();
    assert_eq!(
        fake.identity
            .revoke_session(&SessionId::new("session_gone"))
            .await,
        Err(IdentityError::NotFound("session session_gone".into()))
    );
    fake.identity
        .join_waitlist(" eve@example.net")
        .await
        .unwrap();
    let (code, _) = refused(fake.identity.join_waitlist("ada@example.net").await);
    assert_eq!(code, "user_already_exists");
}

// ---------------------------------------------------------------------------------------------
// Organizations, users, memberships

#[tokio::test]
async fn organizations_are_created_renamed_read_and_deleted() {
    let fake = Fake::start().await;
    Mock::given(method("POST"))
        .and(path("/organizations"))
        .and(body_json(json!({ "name": "Acme" })))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(organization_json("org_acme", "Acme")),
        )
        .mount(&fake.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/organizations/org_acme"))
        .and(body_json(json!({ "name": "Acme 2" })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(organization_json("org_acme", "Acme 2")),
        )
        .mount(&fake.server)
        .await;
    fake.answer(
        "GET",
        "/organizations/org_acme",
        ResponseTemplate::new(200).set_body_json(organization_json("org_acme", "Acme 2")),
    )
    .await;
    fake.answer("GET", "/organizations/org_gone", not_found("org_gone"))
        .await;
    fake.answer(
        "DELETE",
        "/organizations/org_acme",
        ResponseTemplate::new(202),
    )
    .await;
    fake.answer("DELETE", "/organizations/org_gone", not_found("org_gone"))
        .await;
    fake.answer("PUT", "/organizations/org_gone", not_found("org_gone"))
        .await;

    let acme = OrganizationId::new("org_acme");
    let gone = OrganizationId::new("org_gone");
    assert_eq!(
        fake.identity.create_organization("Acme").await.unwrap(),
        Organization {
            id: acme.clone(),
            name: "Acme".into()
        }
    );
    assert_eq!(
        fake.identity
            .rename_organization(&acme, "Acme 2")
            .await
            .unwrap()
            .name,
        "Acme 2"
    );
    assert_eq!(
        fake.identity
            .organization(&acme)
            .await
            .unwrap()
            .unwrap()
            .name,
        "Acme 2"
    );
    assert_eq!(fake.identity.organization(&gone).await, Ok(None));
    assert_eq!(
        fake.identity.organization(&OrganizationId::new("")).await,
        Ok(None)
    );
    fake.identity.delete_organization(&acme).await.unwrap();
    let missing = Err(IdentityError::NotFound("organization org_gone".into()));
    assert_eq!(fake.identity.delete_organization(&gone).await, missing);
    assert_eq!(
        fake.identity
            .rename_organization(&gone, "Gone")
            .await
            .map(|_| ()),
        missing
    );
}

#[tokio::test]
async fn users_are_read() {
    let fake = Fake::start().await;
    fake.answer(
        "GET",
        "/user_management/users/user_ada",
        ResponseTemplate::new(200).set_body_json(user_json("user_ada", "ada@example.net")),
    )
    .await;
    fake.answer(
        "GET",
        "/user_management/users/user_gone",
        not_found("user_gone"),
    )
    .await;
    let ada = fake
        .identity
        .user(&UserId::new("user_ada"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ada.email, "ada@example.net");
    assert_eq!(
        fake.identity.user(&UserId::new("user_gone")).await,
        Ok(None)
    );
}

#[tokio::test]
async fn memberships_are_the_active_ones_with_their_organization_and_user() {
    let fake = Fake::start().await;
    let memberships = "/user_management/organization_memberships";
    Mock::given(method("GET"))
        .and(path(memberships))
        .and(query_param("user_id", "user_ada"))
        .and(query_param("statuses", "active"))
        .and(query_param("order", "asc"))
        .and(query_param("limit", "100"))
        .and(query_param_is_missing("after"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(
            vec![
                membership_json("om_1", "user_ada", "org_acme", Some("Acme")),
                membership_json("om_2", "user_ada", "org_beta", Some("Beta")),
            ],
            Some("om_2"),
        )))
        .mount(&fake.server)
        .await;
    Mock::given(method("GET"))
        .and(path(memberships))
        .and(query_param("user_id", "user_ada"))
        .and(query_param("after", "om_2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(
            vec![membership_json("om_3", "user_ada", "org_gamma", None)],
            None,
        )))
        .mount(&fake.server)
        .await;
    fake.answer(
        "GET",
        "/organizations/org_gamma",
        ResponseTemplate::new(200).set_body_json(organization_json("org_gamma", "Gamma")),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(memberships))
        .and(query_param("organization_id", "org_acme"))
        .and(query_param("statuses", "active"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(
            vec![
                membership_json("om_1", "user_ada", "org_acme", Some("Acme")),
                membership_json("om_4", "user_bob", "org_acme", Some("Acme")),
            ],
            None,
        )))
        .mount(&fake.server)
        .await;

    let of_ada = fake
        .identity
        .memberships_of_user(&UserId::new("user_ada"))
        .await
        .unwrap();
    let names: Vec<(&str, &str)> = of_ada
        .iter()
        .map(|membership| {
            (
                membership.id.as_str(),
                membership.organization.name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [("om_1", "Acme"), ("om_2", "Beta"), ("om_3", "Gamma")]
    );
    assert!(
        of_ada
            .iter()
            .all(|membership| membership.status == MembershipStatus::Active)
    );

    let members = fake
        .identity
        .members(&OrganizationId::new("org_acme"))
        .await
        .unwrap();
    let users: Vec<(&str, &str)> = members
        .iter()
        .map(|membership| (membership.user.id.as_str(), membership.user.email.as_str()))
        .collect();
    assert_eq!(
        users,
        [
            ("user_ada", "user_ada@example.net"),
            ("user_bob", "user_bob@example.net")
        ]
    );
}

#[tokio::test]
async fn members_are_added_without_a_role_and_removed() {
    let fake = Fake::start().await;
    Mock::given(method("POST"))
        .and(path("/user_management/organization_memberships"))
        .and(body_json(
            json!({ "user_id": "user_bob", "organization_id": "org_acme" }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(membership_json(
            "om_4",
            "user_bob",
            "org_acme",
            Some("Acme"),
        )))
        .mount(&fake.server)
        .await;
    fake.answer(
        "DELETE",
        "/user_management/organization_memberships/om_4",
        ResponseTemplate::new(200),
    )
    .await;
    fake.answer(
        "DELETE",
        "/user_management/organization_memberships/om_gone",
        not_found("om_gone"),
    )
    .await;

    let added = fake
        .identity
        .add_member(&OrganizationId::new("org_acme"), &UserId::new("user_bob"))
        .await
        .unwrap();
    assert_eq!(added.id, MembershipId::new("om_4"));
    assert_eq!(added.user.id, UserId::new("user_bob"));
    assert_eq!(added.organization.name, "Acme");
    assert_eq!(added.status, MembershipStatus::Active);
    fake.identity
        .remove_member(&MembershipId::new("om_4"))
        .await
        .unwrap();
    assert_eq!(
        fake.identity
            .remove_member(&MembershipId::new("om_gone"))
            .await,
        Err(IdentityError::NotFound("membership om_gone".into()))
    );
}

// ---------------------------------------------------------------------------------------------
// Invitations

#[tokio::test]
async fn invitations_carry_no_role() {
    let fake = Fake::start().await;
    let invitations = "/user_management/invitations";
    Mock::given(method("POST"))
        .and(path(invitations))
        .and(body_json(json!({
            "email": "bob@example.net",
            "organization_id": "org_acme",
            "inviter_user_id": "user_ada",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(invitation_json(
            "invitation_1",
            "pending",
            Some("org_acme"),
        )))
        .mount(&fake.server)
        .await;
    Mock::given(method("POST"))
        .and(path(invitations))
        .and(body_json(json!({ "email": "carol@example.net" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(with(
            with(
                invitation_json("invitation_2", "pending", None),
                "email",
                json!("carol@example.net"),
            ),
            "inviter_user_id",
            Value::Null,
        )))
        .mount(&fake.server)
        .await;
    Mock::given(method("POST"))
        .and(path(invitations))
        .and(body_partial_json(json!({ "email": "dave@example.net" })))
        .respond_with(error(
            400,
            "email_already_invited_to_organization",
            "Email already invited to organization.",
        ))
        .mount(&fake.server)
        .await;

    let invited = fake
        .identity
        .invite(
            Some(&OrganizationId::new("org_acme")),
            "bob@example.net",
            Some(&UserId::new("user_ada")),
        )
        .await
        .unwrap();
    assert_eq!(invited.id, InvitationId::new("invitation_1"));
    assert_eq!(invited.email, "bob@example.net");
    assert_eq!(invited.organization, Some(OrganizationId::new("org_acme")));
    assert_eq!(invited.state, InvitationState::Pending);
    assert_eq!(invited.inviter, Some(UserId::new("user_ada")));
    assert_eq!(invited.accepted_user, None);
    assert_eq!(
        invited.created_at,
        Timestamp::from_millis(1_794_650_400_000)
    );
    assert_eq!(
        invited.expires_at,
        Timestamp::from_millis(1_795_255_200_000)
    );

    let to_deployment = fake
        .identity
        .invite(None, "carol@example.net", None)
        .await
        .unwrap();
    assert_eq!(to_deployment.organization, None);
    assert_eq!(to_deployment.inviter, None);

    let (code, _) = refused(
        fake.identity
            .invite(
                Some(&OrganizationId::new("org_acme")),
                "dave@example.net",
                None,
            )
            .await,
    );
    assert_eq!(code, "email_already_invited_to_organization");
}

#[tokio::test]
async fn invitations_are_listed_read_accepted_resent_and_revoked() {
    let fake = Fake::start().await;
    let invitations = "/user_management/invitations";
    Mock::given(method("GET"))
        .and(path(invitations))
        .and(query_param("organization_id", "org_acme"))
        .and(query_param("order", "asc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(
            vec![
                invitation_json("invitation_1", "pending", Some("org_acme")),
                invitation_json("invitation_0", "revoked", Some("org_acme")),
            ],
            None,
        )))
        .mount(&fake.server)
        .await;
    Mock::given(method("GET"))
        .and(path(invitations))
        .and(query_param("email", "bob@example.net"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(
            vec![
                invitation_json("invitation_1", "pending", Some("org_acme")),
                invitation_json("invitation_9", "expired", Some("org_beta")),
            ],
            None,
        )))
        .mount(&fake.server)
        .await;
    fake.answer(
        "GET",
        "/user_management/invitations/invitation_1",
        ResponseTemplate::new(200).set_body_json(invitation_json(
            "invitation_1",
            "pending",
            Some("org_acme"),
        )),
    )
    .await;
    fake.answer(
        "GET",
        "/user_management/invitations/invitation_gone",
        not_found("invitation_gone"),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/invitations/invitation_1/accept",
        ResponseTemplate::new(200).set_body_json(invitation_json(
            "invitation_1",
            "accepted",
            Some("org_acme"),
        )),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/invitations/invitation_1/resend",
        error(400, "invite_accepted", "Invite has already been accepted."),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/invitations/invitation_2/resend",
        ResponseTemplate::new(200).set_body_json(invitation_json("invitation_2", "pending", None)),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/invitations/invitation_2/revoke",
        ResponseTemplate::new(200).set_body_json(invitation_json("invitation_2", "revoked", None)),
    )
    .await;
    fake.answer(
        "POST",
        "/user_management/invitations/invitation_gone/revoke",
        not_found("invitation_gone"),
    )
    .await;

    let of_acme = fake
        .identity
        .invitations_of(&OrganizationId::new("org_acme"))
        .await
        .unwrap();
    assert_eq!(
        of_acme
            .iter()
            .map(|invitation| invitation.state)
            .collect::<Vec<_>>(),
        [InvitationState::Pending, InvitationState::Revoked]
    );
    let for_bob = fake
        .identity
        .invitations_for(" Bob@Example.net")
        .await
        .unwrap();
    assert_eq!(
        for_bob
            .iter()
            .map(|invitation| invitation.state)
            .collect::<Vec<_>>(),
        [InvitationState::Pending, InvitationState::Expired]
    );
    let first = InvitationId::new("invitation_1");
    assert_eq!(
        fake.identity
            .invitation(&first)
            .await
            .unwrap()
            .unwrap()
            .state,
        InvitationState::Pending
    );
    assert_eq!(
        fake.identity
            .invitation(&InvitationId::new("invitation_gone"))
            .await,
        Ok(None)
    );
    let accepted = fake.identity.accept_invitation(&first).await.unwrap();
    assert_eq!(accepted.state, InvitationState::Accepted);
    assert_eq!(accepted.accepted_user, Some(UserId::new("user_bob")));
    let (code, _) = refused(fake.identity.resend_invitation(&first).await);
    assert_eq!(code, "invite_accepted");

    let second = InvitationId::new("invitation_2");
    assert_eq!(
        fake.identity
            .resend_invitation(&second)
            .await
            .unwrap()
            .state,
        InvitationState::Pending
    );
    assert_eq!(
        fake.identity
            .revoke_invitation(&second)
            .await
            .unwrap()
            .state,
        InvitationState::Revoked
    );
    assert_eq!(
        fake.identity
            .revoke_invitation(&InvitationId::new("invitation_gone"))
            .await,
        Err(IdentityError::NotFound("invitation invitation_gone".into()))
    );
}

// ---------------------------------------------------------------------------------------------
// Configuration

#[test]
fn the_configuration_is_checked_and_the_key_never_printed() {
    let identity = WorkOs::new(CLIENT_ID, API_KEY).unwrap();
    let printed = format!("{identity:?}");
    assert!(printed.contains(CLIENT_ID), "{printed}");
    assert!(!printed.contains(API_KEY), "{printed}");
    assert!(WorkOs::new("", API_KEY).is_err());
    assert!(WorkOs::new(CLIENT_ID, " ").is_err());
    assert!(WorkOs::new(CLIENT_ID, "sk_bad\nkey").is_err());
    assert!(WorkOs::with_base_url(CLIENT_ID, API_KEY, "api.workos.com").is_err());
}
