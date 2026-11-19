//! People's connections on Turso (`AccessStore`, `archi/06-storage-and-scale.md`,
//! "Connections"): the OAuth clients and codes, the connections, their tokens and their grants.
//! The in-memory `MemoryAccess` of `rewind-engine` is the reference of every behavior here.
//!
//! The writes that read before they write (a code exchanged, a refresh token rotated, a
//! connection revoked or changed) are one transaction on the store's one connection: the check
//! and the write are atomic.

use std::collections::BTreeMap;

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    self, Access, ClientAuthMethod, ClientKind, CodeRedemption, ConnectionToken,
    ConnectionTokenKind, Grant, NewConnection, NewToken, OAuthClient, OAuthCode,
    OpenedOrganization, Principal, Rotation, StoreError, TenantId, UserId,
};
use serde_json::Value;
use turso::{Connection as Db, Row};

use crate::codec::{decode_labels, decode_value, encode_labels};
use crate::sql::{self, Columns, digest, opt_text, opt_ts, tenant, text, transaction, ts};
use crate::{Error, TursoStore};

const CLIENT_COLUMNS: &str = "id, kind, name, uri, logo_uri, redirect_uris, auth_method, \
     secret_hash, fetched_at, expires_at, created_at, last_used_at";

const CODE_COLUMNS: &str = "hash, client_id, user_id, redirect_uri, code_challenge, resource, \
     organizations, labels, expires_at, used_at, connection_id";

const CONNECTION_COLUMNS: &str =
    "id, user_id, client_id, client_name, created_at, last_used_at, revoked_at, expires_at";

const TOKEN_COLUMNS: &str = "hash, connection_id, kind, expires_at, created_at, used_at";

const GRANT_COLUMNS: &str = "tenant, principal, role, allowed_labels, granted_by, granted_at";

fn encode_strings(items: &[String]) -> String {
    Value::Array(items.iter().cloned().map(Value::String).collect()).to_string()
}

fn decode_strings(json: &str) -> Result<Vec<String>, Error> {
    let Value::Array(items) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("a list of strings {json:?}")));
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(item) => Ok(item),
            other => Err(Error::Corrupt(format!("{other} is not a string"))),
        })
        .collect()
}

fn encode_organizations(organizations: &BTreeMap<TenantId, Access>) -> String {
    let object: serde_json::Map<String, Value> = organizations
        .iter()
        .map(|(tenant, access)| (tenant.to_string(), Value::String(access.as_str().into())))
        .collect();
    Value::Object(object).to_string()
}

fn decode_organizations(json: &str) -> Result<BTreeMap<TenantId, Access>, Error> {
    let Value::Object(object) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("organizations {json:?}")));
    };
    object
        .into_iter()
        .map(|(id, access)| {
            let tenant =
                TenantId::new(id.clone()).map_err(|_| Error::Corrupt(format!("tenant {id:?}")))?;
            let access = access
                .as_str()
                .and_then(Access::parse)
                .ok_or_else(|| Error::Corrupt(format!("access {access}")))?;
            Ok((tenant, access))
        })
        .collect()
}

fn client_from_row(row: &Row) -> Result<OAuthClient, Error> {
    let kind = row.text(1)?;
    let auth_method = row.text(6)?;
    Ok(OAuthClient {
        id: row.text(0)?,
        kind: ClientKind::parse(&kind)
            .ok_or_else(|| Error::Corrupt(format!("client kind {kind:?}")))?,
        name: row.text(2)?,
        uri: row.opt_text(3)?,
        logo_uri: row.opt_text(4)?,
        redirect_uris: decode_strings(&row.text(5)?)?,
        auth_method: ClientAuthMethod::parse(&auth_method)
            .ok_or_else(|| Error::Corrupt(format!("authentication method {auth_method:?}")))?,
        secret_hash: row.opt_digest(7)?,
        fetched_at: row.opt_ts(8)?,
        expires_at: row.opt_ts(9)?,
        created_at: row.ts(10)?,
        last_used_at: row.opt_ts(11)?,
    })
}

fn code_from_row(row: &Row) -> Result<OAuthCode, Error> {
    Ok(OAuthCode {
        hash: row.digest(0)?,
        client_id: row.text(1)?,
        user: UserId::new(row.text(2)?),
        redirect_uri: row.text(3)?,
        code_challenge: row.text(4)?,
        resource: row.text(5)?,
        organizations: decode_organizations(&row.text(6)?)?,
        labels: decode_labels(&row.text(7)?)?,
        expires_at: row.ts(8)?,
        used_at: row.opt_ts(9)?,
        connection: row.opt_text(10)?,
    })
}

fn connection_from_row(row: &Row) -> Result<ports::Connection, Error> {
    Ok(ports::Connection {
        id: row.text(0)?,
        user: UserId::new(row.text(1)?),
        client_id: row.text(2)?,
        client_name: row.text(3)?,
        created_at: row.ts(4)?,
        last_used_at: row.opt_ts(5)?,
        revoked_at: row.opt_ts(6)?,
        expires_at: row.ts(7)?,
    })
}

fn token_from_row(row: &Row) -> Result<ConnectionToken, Error> {
    let kind = row.text(2)?;
    Ok(ConnectionToken {
        hash: row.digest(0)?,
        connection: row.text(1)?,
        kind: ConnectionTokenKind::parse(&kind)
            .ok_or_else(|| Error::Corrupt(format!("token kind {kind:?}")))?,
        expires_at: row.ts(3)?,
        created_at: row.ts(4)?,
        used_at: row.opt_ts(5)?,
    })
}

fn grant_from_row(row: &Row) -> Result<Grant, Error> {
    let principal =
        |text: &str| Principal::parse(text).map_err(|error| Error::Corrupt(error.to_string()));
    Ok(Grant {
        tenant: row.tenant(0)?,
        principal: principal(&row.text(1)?)?,
        role: row.text(2)?,
        allowed_labels: decode_labels(&row.text(3)?)?,
        granted_by: row.opt_text(4)?.map(|by| principal(&by)).transpose()?,
        granted_at: row.ts(5)?,
    })
}

async fn find_connection(db: &Db, id: &str) -> Result<Option<ports::Connection>, Error> {
    let row = sql::query_row(
        db,
        &format!("SELECT {CONNECTION_COLUMNS} FROM connections WHERE id = ?"),
        vec![text(id)],
    )
    .await?;
    row.map(|row| connection_from_row(&row)).transpose()
}

/// Replaces the grants of a connection with one per organization, given by its person.
async fn open(
    db: &Db,
    connection: &ports::Connection,
    organizations: &[OpenedOrganization],
    now: Timestamp,
) -> Result<(), Error> {
    let principal = connection.principal().to_string();
    sql::execute(
        db,
        "DELETE FROM grants WHERE principal = ?",
        vec![text(principal.as_str())],
    )
    .await?;
    for opened in organizations {
        sql::execute(
            db,
            "INSERT OR REPLACE INTO grants (tenant, principal, role, allowed_labels, granted_by, \
             granted_at) VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                tenant(&opened.tenant),
                text(principal.as_str()),
                text(opened.access.role()),
                text(encode_labels(&opened.allowed_labels)),
                text(Principal::from(&connection.user).to_string()),
                ts(now),
            ],
        )
        .await?;
    }
    Ok(())
}

/// Deletes the grants and the tokens of a connection.
async fn forget(db: &Db, id: &str) -> Result<(), Error> {
    sql::execute(
        db,
        "DELETE FROM grants WHERE principal = ?",
        vec![text(Principal::Connection(id.to_owned()).to_string())],
    )
    .await?;
    sql::execute(
        db,
        "DELETE FROM connection_tokens WHERE connection_id = ?",
        vec![text(id)],
    )
    .await?;
    Ok(())
}

/// Revokes a live connection: its grants and tokens go. `false` when it is unknown or revoked.
async fn revoke(db: &Db, id: &str, now: Timestamp) -> Result<bool, Error> {
    let revoked = sql::execute(
        db,
        "UPDATE connections SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
        vec![ts(now), text(id)],
    )
    .await?;
    if revoked == 0 {
        return Ok(false);
    }
    forget(db, id).await?;
    Ok(true)
}

/// Stores new tokens of a connection; a hash already stored is a `Conflict`.
async fn issue(
    db: &Db,
    connection: &str,
    tokens: [(&NewToken, ConnectionTokenKind); 2],
    now: Timestamp,
) -> Result<(), StoreError> {
    for (token, kind) in tokens {
        let taken = sql::query_row(
            db,
            "SELECT 1 FROM connection_tokens WHERE hash = ?",
            vec![digest(token.hash)],
        )
        .await?;
        if taken.is_some() {
            return Err(StoreError::Conflict(
                "a connection's token with this hash already exists".into(),
            ));
        }
        sql::execute(
            db,
            "INSERT INTO connection_tokens (hash, connection_id, kind, expires_at, created_at, \
             used_at) VALUES (?, ?, ?, ?, ?, NULL)",
            vec![
                digest(token.hash),
                text(connection),
                text(kind.as_str()),
                ts(token.expires_at),
                ts(now),
            ],
        )
        .await?;
    }
    Ok(())
}

impl TursoStore {
    pub(crate) async fn put_oauth_client_row(&self, client: &OAuthClient) -> Result<(), Error> {
        let db = self.connection().await?;
        sql::execute(
            &db,
            &format!(
                "INSERT INTO oauth_clients ({CLIENT_COLUMNS}) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, name = excluded.name, \
                 uri = excluded.uri, logo_uri = excluded.logo_uri, \
                 redirect_uris = excluded.redirect_uris, auth_method = excluded.auth_method, \
                 secret_hash = excluded.secret_hash, fetched_at = excluded.fetched_at, \
                 expires_at = excluded.expires_at"
            ),
            vec![
                text(client.id.as_str()),
                text(client.kind.as_str()),
                text(client.name.as_str()),
                opt_text(client.uri.clone()),
                opt_text(client.logo_uri.clone()),
                text(encode_strings(&client.redirect_uris)),
                text(client.auth_method.as_str()),
                opt_text(client.secret_hash.map(|hash| hash.to_hex())),
                opt_ts(client.fetched_at),
                opt_ts(client.expires_at),
                ts(client.created_at),
                opt_ts(client.last_used_at),
            ],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn oauth_client_row(&self, id: &str) -> Result<Option<OAuthClient>, Error> {
        let db = self.connection().await?;
        let row = sql::query_row(
            &db,
            &format!("SELECT {CLIENT_COLUMNS} FROM oauth_clients WHERE id = ?"),
            vec![text(id)],
        )
        .await?;
        row.map(|row| client_from_row(&row)).transpose()
    }

    pub(crate) async fn count_oauth_client_rows(&self, kind: ClientKind) -> Result<u64, Error> {
        let db = self.connection().await?;
        let row = sql::query_row(
            &db,
            "SELECT COUNT(*) FROM oauth_clients WHERE kind = ?",
            vec![text(kind.as_str())],
        )
        .await?;
        row.map_or(Ok(0), |row| row.u64(0))
    }

    pub(crate) async fn purge_oauth_client_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let db = self.connection().await?;
        sql::execute(
            &db,
            "DELETE FROM oauth_clients WHERE id IN (SELECT id FROM oauth_clients \
             WHERE COALESCE(last_used_at, fetched_at, created_at) < ?1 \
             AND NOT EXISTS (SELECT 1 FROM connections WHERE connections.client_id = oauth_clients.id) \
             LIMIT ?2)",
            vec![ts(before), sql::limit(limit)],
        )
        .await
    }

    pub(crate) async fn put_oauth_code_row(&self, code: &OAuthCode) -> Result<(), StoreError> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, async {
            let taken = sql::query_row(
                db,
                "SELECT 1 FROM oauth_codes WHERE hash = ?",
                vec![digest(code.hash)],
            )
            .await?;
            if taken.is_some() {
                return Err(StoreError::Conflict(format!(
                    "an authorization code with hash {} already exists",
                    code.hash.short()
                )));
            }
            sql::execute(
                db,
                &format!(
                    "INSERT INTO oauth_codes ({CODE_COLUMNS}) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                vec![
                    digest(code.hash),
                    text(code.client_id.as_str()),
                    text(code.user.as_str()),
                    text(code.redirect_uri.as_str()),
                    text(code.code_challenge.as_str()),
                    text(code.resource.as_str()),
                    text(encode_organizations(&code.organizations)),
                    text(encode_labels(&code.labels)),
                    ts(code.expires_at),
                    opt_ts(code.used_at),
                    opt_text(code.connection.clone()),
                ],
            )
            .await?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn oauth_code_row(&self, hash: Digest) -> Result<Option<OAuthCode>, Error> {
        let db = self.connection().await?;
        let row = sql::query_row(
            &db,
            &format!("SELECT {CODE_COLUMNS} FROM oauth_codes WHERE hash = ?"),
            vec![digest(hash)],
        )
        .await?;
        row.map(|row| code_from_row(&row)).transpose()
    }

    pub(crate) async fn redeem_oauth_code_rows(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, StoreError> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, async {
            let row = sql::query_row(
                db,
                &format!("SELECT {CODE_COLUMNS} FROM oauth_codes WHERE hash = ?"),
                vec![digest(hash)],
            )
            .await?;
            let Some(code) = row.map(|row| code_from_row(&row)).transpose()? else {
                return Ok(CodeRedemption::Unknown);
            };
            if code.used_at.is_some() {
                // A replay: its connection is revoked, which this transaction commits.
                if let Some(id) = &code.connection {
                    revoke(db, id, now).await?;
                }
                return Ok(CodeRedemption::Used);
            }
            sql::execute(
                db,
                "UPDATE oauth_codes SET used_at = ? WHERE hash = ?",
                vec![ts(now), digest(hash)],
            )
            .await?;
            let kept = sql::query_row(
                db,
                &format!(
                    "SELECT {CONNECTION_COLUMNS} FROM connections WHERE user_id = ? \
                     AND client_id = ? AND revoked_at IS NULL ORDER BY created_at, id LIMIT 1"
                ),
                vec![
                    text(new.connection.user.as_str()),
                    text(new.connection.client_id.as_str()),
                ],
            )
            .await?
            .map(|row| connection_from_row(&row))
            .transpose()?;
            let connection = match kept {
                Some(kept) => {
                    sql::execute(
                        db,
                        "UPDATE connections SET expires_at = ? WHERE id = ?",
                        vec![ts(new.connection.expires_at), text(kept.id.as_str())],
                    )
                    .await?;
                    ports::Connection {
                        expires_at: new.connection.expires_at,
                        ..kept
                    }
                }
                None => {
                    if find_connection(db, &new.connection.id).await?.is_some() {
                        return Err(StoreError::Conflict(format!(
                            "a connection {} already exists",
                            new.connection.id
                        )));
                    }
                    let created = &new.connection;
                    sql::execute(
                        db,
                        &format!(
                            "INSERT INTO connections ({CONNECTION_COLUMNS}) \
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                        ),
                        vec![
                            text(created.id.as_str()),
                            text(created.user.as_str()),
                            text(created.client_id.as_str()),
                            text(created.client_name.as_str()),
                            ts(created.created_at),
                            opt_ts(created.last_used_at),
                            opt_ts(created.revoked_at),
                            ts(created.expires_at),
                        ],
                    )
                    .await?;
                    created.clone()
                }
            };
            issue(
                db,
                &connection.id,
                [
                    (&new.access, ConnectionTokenKind::Access),
                    (&new.refresh, ConnectionTokenKind::Refresh),
                ],
                now,
            )
            .await?;
            open(db, &connection, &new.organizations, now).await?;
            sql::execute(
                db,
                "UPDATE oauth_codes SET connection_id = ? WHERE hash = ?",
                vec![text(connection.id.as_str()), digest(hash)],
            )
            .await?;
            sql::execute(
                db,
                "UPDATE oauth_clients SET last_used_at = ? WHERE id = ?",
                vec![ts(now), text(connection.client_id.as_str())],
            )
            .await?;
            Ok(CodeRedemption::Redeemed(connection))
        })
        .await
    }

    pub(crate) async fn purge_oauth_code_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let db = self.connection().await?;
        sql::execute(
            &db,
            "DELETE FROM oauth_codes WHERE hash IN \
             (SELECT hash FROM oauth_codes WHERE expires_at < ?1 LIMIT ?2)",
            vec![ts(before), sql::limit(limit)],
        )
        .await
    }

    pub(crate) async fn connection_row(
        &self,
        id: &str,
    ) -> Result<Option<ports::Connection>, Error> {
        let db = self.connection().await?;
        find_connection(&db, id).await
    }

    pub(crate) async fn connections_of_user(
        &self,
        user: &UserId,
    ) -> Result<Vec<ports::Connection>, Error> {
        let db = self.connection().await?;
        let rows = sql::query(
            &db,
            &format!(
                "SELECT {CONNECTION_COLUMNS} FROM connections WHERE user_id = ? \
                 AND revoked_at IS NULL ORDER BY created_at, id"
            ),
            vec![text(user.as_str())],
        )
        .await?;
        rows.iter().map(connection_from_row).collect()
    }

    pub(crate) async fn connection_grant_rows(&self, id: &str) -> Result<Vec<Grant>, Error> {
        let db = self.connection().await?;
        let rows = sql::query(
            &db,
            &format!("SELECT {GRANT_COLUMNS} FROM grants WHERE principal = ? ORDER BY tenant"),
            vec![text(Principal::Connection(id.to_owned()).to_string())],
        )
        .await?;
        rows.iter().map(grant_from_row).collect()
    }

    pub(crate) async fn set_connection_organization_rows(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, Error> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, async {
            let Some(connection) = find_connection(db, id)
                .await?
                .filter(|connection| connection.revoked_at.is_none())
            else {
                return Ok(false);
            };
            open(db, &connection, organizations, now).await?;
            Ok(true)
        })
        .await
    }

    pub(crate) async fn touch_connection_row(&self, id: &str, now: Timestamp) -> Result<(), Error> {
        let db = self.connection().await?;
        sql::execute(
            &db,
            "UPDATE connections SET last_used_at = ? WHERE id = ?",
            vec![ts(now), text(id)],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn revoke_connection_row(
        &self,
        id: &str,
        now: Timestamp,
    ) -> Result<bool, Error> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, revoke(db, id, now)).await
    }

    pub(crate) async fn purge_connection_rows(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, async {
            let rows = sql::query(
                db,
                "SELECT id FROM connections WHERE expires_at < ?1 OR revoked_at < ?2 LIMIT ?3",
                vec![ts(lapsed_before), ts(revoked_before), sql::limit(limit)],
            )
            .await?;
            let ids: Vec<String> = rows
                .iter()
                .map(|row| row.text(0))
                .collect::<Result<_, _>>()?;
            for id in &ids {
                forget(db, id).await?;
                sql::execute(
                    db,
                    "DELETE FROM connections WHERE id = ?",
                    vec![text(id.as_str())],
                )
                .await?;
            }
            Ok(ids.len() as u64)
        })
        .await
    }

    pub(crate) async fn connection_token_row(
        &self,
        hash: Digest,
    ) -> Result<Option<ConnectionToken>, Error> {
        let db = self.connection().await?;
        let row = sql::query_row(
            &db,
            &format!("SELECT {TOKEN_COLUMNS} FROM connection_tokens WHERE hash = ?"),
            vec![digest(hash)],
        )
        .await?;
        row.map(|row| token_from_row(&row)).transpose()
    }

    pub(crate) async fn rotate_refresh_token_rows(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, StoreError> {
        let db = self.connection().await?;
        let db = &*db;
        transaction(db, async {
            let row = sql::query_row(
                db,
                &format!(
                    "SELECT {TOKEN_COLUMNS} FROM connection_tokens WHERE hash = ? AND kind = ?"
                ),
                vec![digest(hash), text(ConnectionTokenKind::Refresh.as_str())],
            )
            .await?;
            let Some(token) = row.map(|row| token_from_row(&row)).transpose()? else {
                return Ok(Rotation::Unknown);
            };
            let Some(connection) = find_connection(db, &token.connection)
                .await?
                .filter(|connection| connection.revoked_at.is_none())
            else {
                return Ok(Rotation::Unknown);
            };
            if token.used_at.is_some() {
                revoke(db, &connection.id, now).await?;
                return Ok(Rotation::Used);
            }
            if token.expires_at <= now {
                return Ok(Rotation::Expired);
            }
            issue(
                db,
                &connection.id,
                [
                    (access, ConnectionTokenKind::Access),
                    (refresh, ConnectionTokenKind::Refresh),
                ],
                now,
            )
            .await?;
            sql::execute(
                db,
                "UPDATE connection_tokens SET used_at = ? WHERE hash = ?",
                vec![ts(now), digest(hash)],
            )
            .await?;
            sql::execute(
                db,
                "UPDATE connections SET expires_at = ? WHERE id = ?",
                vec![ts(refresh.expires_at), text(connection.id.as_str())],
            )
            .await?;
            Ok(Rotation::Rotated(ports::Connection {
                expires_at: refresh.expires_at,
                ..connection
            }))
        })
        .await
    }

    pub(crate) async fn purge_connection_token_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let db = self.connection().await?;
        sql::execute(
            &db,
            "DELETE FROM connection_tokens WHERE hash IN \
             (SELECT hash FROM connection_tokens WHERE expires_at < ?1 LIMIT ?2)",
            vec![ts(before), sql::limit(limit)],
        )
        .await
    }
}
