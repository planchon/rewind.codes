//! Secrets and signing keys (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/07-code-architecture.md`, "HTTP API /v1").
//!
//! | Endpoint | Permission | Who may |
//! | --- | --- | --- |
//! | `GET /v1/secrets`, `…/{name}/usage` | `secrets:read` | the organization's secrets carrying the caller's labels |
//! | `PUT`, `DELETE /v1/secrets/{name}` | `secrets:write` | not an end user's token; a grant limited to labels gives its labels |
//! | `GET /v1/namespaces/{name}/secrets`, `…/{secret}/usage` | `secrets:read` | a token, its own namespace |
//! | `PUT`, `DELETE /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | not a grant or a token limited to labels |
//! | `GET /v1/organization/signing-keys` | `secrets:read` | |
//! | `POST /v1/organization/signing-keys` | `organization:write` | |
//! | `GET /organizations/{tenant}/jwks.json` | none | `Cache-Control: max-age=300` |
//!
//! A value is written and never answered. Putting a secret again makes ready the calls parked by
//! a refusal or a missing secret in the runs that attach it (`Engine::put_secret`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Response;
use rewind_api::secrets::{PutSecret, SecretList, SecretUsage, SigningKey, SigningKeyList};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Body;

/// `GET /v1/secrets`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SecretList>, ApiError> {
    let _ = (app, caller);
    todo!(
        "engine-server: GET /v1/secrets, SecretStore::list of the organization under the caller's labels"
    )
}

/// `PUT /v1/secrets/{name}`: 201 when created, 200 when replaced, with the secret's metadata.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn put(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
    Body(secret): Body<PutSecret>,
) -> Result<Response, ApiError> {
    let _ = (app, caller, name, secret);
    todo!(
        "engine-server: PUT /v1/secrets/{{name}}, Engine::put_secret, then record the resumed tasks"
    )
}

/// `DELETE /v1/secrets/{name}`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn delete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let _ = (app, caller, name);
    todo!("engine-server: DELETE /v1/secrets/{{name}}, Engine::delete_secret")
}

/// `GET /v1/secrets/{name}/usage`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn usage(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<SecretUsage>, ApiError> {
    let _ = (app, caller, name);
    todo!(
        "engine-server: GET /v1/secrets/{{name}}/usage, JournalStore::secret_usage and SecretStore::meta"
    )
}

/// `GET /v1/namespaces/{name}/secrets`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn list_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(namespace): Path<String>,
) -> Result<Json<SecretList>, ApiError> {
    let _ = (app, caller, namespace);
    todo!("engine-server: GET /v1/namespaces/{{name}}/secrets, SecretStore::list of the namespace")
}

/// `PUT /v1/namespaces/{name}/secrets/{secret}`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn put_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
    Body(secret): Body<PutSecret>,
) -> Result<Response, ApiError> {
    let _ = (app, caller, namespace, name, secret);
    todo!("engine-server: PUT /v1/namespaces/{{name}}/secrets/{{secret}}, Engine::put_secret")
}

/// `DELETE /v1/namespaces/{name}/secrets/{secret}`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn delete_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let _ = (app, caller, namespace, name);
    todo!("engine-server: DELETE /v1/namespaces/{{name}}/secrets/{{secret}}, Engine::delete_secret")
}

/// `GET /v1/namespaces/{name}/secrets/{secret}/usage`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the secrets"
)]
pub(super) async fn usage_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<SecretUsage>, ApiError> {
    let _ = (app, caller, namespace, name);
    todo!("engine-server: GET /v1/namespaces/{{name}}/secrets/{{secret}}/usage")
}

/// `GET /v1/organization/signing-keys`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the signing keys"
)]
pub(super) async fn signing_keys(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SigningKeyList>, ApiError> {
    let _ = (app, caller);
    todo!("engine-server: GET /v1/organization/signing-keys, SecretStore::public_keys")
}

/// `POST /v1/organization/signing-keys`.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the signing keys"
)]
pub(super) async fn rotate_signing_key(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SigningKey>, ApiError> {
    let _ = (app, caller);
    todo!("engine-server: POST /v1/organization/signing-keys, SecretStore::rotate_signing_key")
}

/// `GET /organizations/{tenant}/jwks.json`, without authentication.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream serves the public keys"
)]
pub(super) async fn jwks(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
) -> Result<Response, ApiError> {
    let _ = (app, tenant);
    todo!(
        "engine-server: GET /organizations/{{tenant}}/jwks.json, SecretStore::public_keys as \
         Jwk::ed25519, Cache-Control: max-age=300"
    )
}
