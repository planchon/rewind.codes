//! End users' tokens and the keys that sign them (`archi/06-storage-and-scale.md`, "Who calls";
//! `archi/07-code-architecture.md`, "HTTP API `/v1`", group Tokens). Whoever mints a token or
//! registers a key holds every permission it gives, among those a token may hold.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::access::Permission;
use rewind_api::common::timestamp;
use rewind_api::namespaces::{
    CreateToken, CreateTokenKey, RevokedTokens, TOKEN_DEFAULT_TTL, TOKEN_MAX_TTL,
    Token as ApiToken, TokenKey as ApiTokenKey, TokenKeyList,
};
use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{AccessStore, CatalogStore, NamespaceWrite, Principal, Token, TokenKey};
use rewind_engine::text::{refuse_nul, refuse_nul_in_labels};

use crate::app::App;
use crate::auth::{Caller, key_hash, listed};
use crate::error::ApiError;
use crate::extract::{Body, OptionalBody};
use crate::http::namespaces::{deleting, name, reach};
use crate::tokens::{PublicKey, new_kid, new_token};
use crate::views;

/// The longest name of a token key.
const MAX_KEY_NAME: usize = 200;

/// The principal who mints a token or registers a key; none over the embedded server's socket,
/// which serves no end user (its socket takes no token).
fn author(caller: &Caller) -> Result<Principal, ApiError> {
    caller.principal.clone().ok_or_else(|| {
        ApiError::forbidden(
            "end users' tokens authenticate over HTTP, which the embedded server does not serve: \
             mint them on a deployed server, with a key or as a member",
        )
    })
}

/// The permissions a token or a key gives: `asked`, each one a token may hold (400) and the
/// caller holds (403); without `asked`, `default` within what the caller holds.
fn givable(
    caller: &Caller,
    asked: Option<&[Permission]>,
    default: &[Permission],
) -> Result<BTreeSet<Permission>, ApiError> {
    let Some(asked) = asked else {
        return Ok(default
            .iter()
            .copied()
            .filter(|permission| caller.permissions.contains(permission))
            .collect());
    };
    let asked: BTreeSet<Permission> = asked.iter().copied().collect();
    let outside: Vec<Permission> = asked
        .iter()
        .copied()
        .filter(|permission| !permission.in_token())
        .collect();
    if !outside.is_empty() {
        return Err(ApiError::invalid(format!(
            "an end user's token never holds {}: it holds only permissions that act within its \
             namespace, among {}",
            listed(&outside),
            listed(&Permission::TOKEN)
        )));
    }
    let lacking = caller.lacks(&asked);
    if !lacking.is_empty() {
        return Err(ApiError::forbidden(format!(
            "{} does not hold {}, so it cannot give them: no one gives more than they hold",
            caller.who(),
            listed(&lacking)
        )));
    }
    Ok(asked)
}

fn names(permissions: &BTreeSet<Permission>) -> BTreeSet<String> {
    permissions
        .iter()
        .map(|permission| permission.as_str().to_owned())
        .collect()
}

fn parsed(names: &BTreeSet<String>) -> Vec<Permission> {
    let permissions: BTreeSet<Permission> = names
        .iter()
        .filter_map(|name| Permission::parse(name))
        .collect();
    permissions.into_iter().collect()
}

fn token_view(token: &Token, secret: Option<String>) -> ApiToken {
    ApiToken {
        id: token.hash.to_hex(),
        namespace: views::namespace(&token.namespace),
        permissions: parsed(&token.permissions),
        labels: token.labels.clone().into(),
        expires_at: timestamp(token.expires_at),
        created_at: timestamp(token.created_at),
        token: secret,
    }
}

/// `POST /v1/namespaces/{name}/tokens`: an opaque token of the namespace, created at its first
/// use. The secret is only in this answer.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
    OptionalBody(request): OptionalBody<CreateToken>,
) -> Result<Response, ApiError> {
    caller.require(Permission::TokensWrite)?;
    let namespace = name(&text)?;
    reach(&caller, &namespace)?;
    let created_by = author(&caller)?;
    let ttl = request.ttl.map_or(TOKEN_DEFAULT_TTL, |ttl| ttl.get());
    if ttl.is_zero() || ttl > TOKEN_MAX_TTL {
        return Err(ApiError::invalid(format!(
            "ttl {}: a token lives more than 0 and at most 30 days",
            rewind_api::common::Duration::from(ttl)
        )));
    }
    let permissions = givable(
        &caller,
        request.permissions.as_deref(),
        &Permission::TOKEN_DEFAULT,
    )?;
    let labels = caller.within(request.labels.0)?;
    refuse_nul_in_labels(&labels)?;
    let now = app.now();
    if app
        .store
        .put_namespace(&caller.tenant, &namespace, None, app.config.partitions, now)
        .await?
        == NamespaceWrite::Deleting
    {
        return Err(deleting());
    }
    let secret = new_token()?;
    let token = Token {
        hash: key_hash(&secret),
        tenant: caller.tenant.clone(),
        namespace,
        permissions: names(&permissions),
        labels,
        expires_at: Timestamp::from_millis(
            now.as_millis()
                .saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)),
        ),
        created_by,
        created_at: now,
        revoked_at: None,
    };
    app.store.put_token(&token).await?;
    Ok((StatusCode::CREATED, Json(token_view(&token, Some(secret)))).into_response())
}

/// `DELETE /v1/namespaces/{name}/tokens`: the namespace's opaque tokens are revoked, and its
/// signed tokens issued before now refused.
pub(super) async fn revoke_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
) -> Result<Json<RevokedTokens>, ApiError> {
    caller.require(Permission::TokensWrite)?;
    let namespace = name(&text)?;
    reach(&caller, &namespace)?;
    let now = app.now();
    if !app
        .store
        .revoke_namespace_tokens(&caller.tenant, &namespace, now)
        .await?
    {
        return Err(ApiError::not_found(format!(
            "no namespace {namespace}: it has no token to revoke"
        )));
    }
    let revoked = app
        .store
        .revoke_tokens(&caller.tenant, &namespace, now)
        .await?;
    Ok(Json(RevokedTokens {
        revoked,
        revoked_before: timestamp(now),
    }))
}

/// `DELETE /v1/tokens/{id}`: revokes one opaque token; a token already revoked stays so.
pub(super) async fn revoke(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::TokensWrite)?;
    let missing = || {
        ApiError::not_found(format!(
            "no token {id}: a token's id is the SHA-256 of its secret, as POST \
             /v1/namespaces/{{name}}/tokens answered it"
        ))
    };
    let hash = Digest::from_hex(&id).map_err(|_| missing())?;
    let token = app
        .store
        .find_token(hash)
        .await?
        .filter(|token| token.tenant == caller.tenant && caller.reaches(&token.namespace))
        .ok_or_else(missing)?;
    if token.revoked_at.is_none() {
        app.store
            .revoke_token(&caller.tenant, hash, app.now())
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

fn key_view(key: &TokenKey) -> Result<ApiTokenKey, ApiError> {
    let public = PublicKey::stored(&key.jwk)
        .ok_or_else(|| ApiError::internal(format!("the stored key {} cannot be read", key.kid)))?;
    Ok(ApiTokenKey {
        kid: key.kid.clone(),
        name: key.name.clone(),
        algorithm: public.algorithm,
        jwk: public.jwk,
        permissions: parsed(&key.permissions),
        created_by: Some(key.created_by.to_string()),
        created_at: timestamp(key.created_at),
    })
}

/// `GET /v1/organization/token-keys`.
pub(super) async fn list_keys(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<TokenKeyList>, ApiError> {
    caller.require(Permission::TokensWrite)?;
    let keys = app.store.list_token_keys(&caller.tenant).await?;
    Ok(Json(TokenKeyList {
        keys: keys.iter().map(key_view).collect::<Result<_, _>>()?,
    }))
}

/// `POST /v1/organization/token-keys`: a public key the customer signs its end users' tokens
/// with, and the permissions they may carry. Answers its `kid`.
pub(super) async fn create_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateTokenKey>,
) -> Result<Response, ApiError> {
    caller.require(Permission::TokensWrite)?;
    let created_by = author(&caller)?;
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > MAX_KEY_NAME {
        return Err(ApiError::invalid(format!(
            "name: 1 to {MAX_KEY_NAME} characters, what the key is for"
        )));
    }
    refuse_nul("the key's name", name)?;
    let public = PublicKey::parse(&request.jwk)?;
    let permissions = givable(&caller, request.permissions.as_deref(), &Permission::TOKEN)?;
    let key = TokenKey {
        kid: new_kid()?,
        tenant: caller.tenant.clone(),
        name: name.to_owned(),
        jwk: public.jwk.to_string(),
        permissions: names(&permissions),
        created_by,
        created_at: app.now(),
    };
    app.store.put_token_key(&key).await?;
    Ok((StatusCode::CREATED, Json(key_view(&key)?)).into_response())
}

/// `DELETE /v1/organization/token-keys/{kid}`: every token it signed is refused from the next
/// request.
pub(super) async fn delete_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(kid): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::TokensWrite)?;
    // A `kid` holding a NUL character is no key's, and never reaches a store.
    if kid.contains('\0') || !app.store.delete_token_key(&caller.tenant, &kid).await? {
        return Err(ApiError::not_found(format!(
            "no token key {kid}: GET /v1/organization/token-keys lists them"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}
