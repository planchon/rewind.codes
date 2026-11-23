//! People's connections on Postgres (`AccessStore`, `archi/06-storage-and-scale.md`,
//! "Connections"): the OAuth clients and codes, the connections, their tokens and their grants.
//! The in-memory `MemoryAccess` of `rewind-engine` is the reference of every behavior here.
//!
//! Several servers share the database. Exchanging a code locks the code's row, and rotating a
//! refresh token the token's (`FOR UPDATE`): of two exchanges at once, the second finds it
//! used, and revokes the connection. An exchange also takes an advisory lock on its person and
//! client, so that two codes exchanged at once keep one connection per person and client
//! (`connections_live` refuses a second one anyway). A connection's grants carry the built-in
//! roles `reader` and `harness`, never a tenant's role, and a connection is never an owner: they
//! take no lock of the tenant's grants.
//!
//! A write of a connection, of its tokens or of its grants sends, in its transaction, the
//! connection's change and the change of each grant it wrote or deleted (`crate::changes`).

use std::collections::BTreeMap;

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    self, Access, ClientAuthMethod, ClientKind, CodeRedemption, ConnectionToken,
    ConnectionTokenKind, ControlPlaneChange, Grant, NewConnection, NewToken, OAuthClient,
    OAuthCode, OpenedOrganization, Principal, Rotation, StoreError, TenantId, UserId,
};
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::access::grant_changed;
use crate::changes::notify;
use crate::codec::{decode_labels, encode_labels};
use crate::sql::{Columns, Failure, bytes, opt_ts, retrying, tenant, ts};
use crate::{Error, PostgresStore};

type Fail = Failure<StoreError>;

fn connection_changed(id: &str) -> ControlPlaneChange {
    ControlPlaneChange::Connection { id: id.to_owned() }
}

/// Deletes the grants of a connection's principal, adding the change of each to `changes`.
async fn delete_grants(
    conn: &mut PgConnection,
    principal: &Principal,
    changes: &mut Vec<ControlPlaneChange>,
) -> Result<(), Error> {
    let tenants: Vec<String> =
        sqlx::query_scalar("DELETE FROM grants WHERE principal = $1 RETURNING tenant")
            .bind(principal.to_string())
            .fetch_all(&mut *conn)
            .await?;
    for tenant_id in tenants {
        let tenant_id = TenantId::new(tenant_id).map_err(|err| Error::Corrupt(err.to_string()))?;
        changes.push(grant_changed(&tenant_id, principal));
    }
    Ok(())
}

const CLIENT_COLUMNS: &str = "id, kind, name, uri, logo_uri, redirect_uris, auth_method, \
     secret_hash, fetched_at, expires_at, created_at, last_used_at";

/// The columns of an [`OAuthCode`], as written.
const CODE_FIELDS: &str = "hash, client_id, user_id, redirect_uri, code_challenge, resource, \
     organizations, labels, expires_at, used_at, connection_id";

/// The columns of an [`OAuthCode`], read by [`code_from_row`].
const CODE_COLUMNS: &str = "hash, client_id, user_id, redirect_uri, code_challenge, resource, \
     organizations::text, labels::text, expires_at, used_at, connection_id";

const CONNECTION_COLUMNS: &str =
    "id, user_id, client_id, client_name, created_at, last_used_at, revoked_at, expires_at";

const TOKEN_COLUMNS: &str = "hash, connection_id, kind, expires_at, created_at, used_at";

const GRANT_COLUMNS: &str = "tenant, principal, role, allowed_labels::text, granted_by, granted_at";

fn encode_organizations(organizations: &BTreeMap<TenantId, Access>) -> String {
    let object: serde_json::Map<String, Value> = organizations
        .iter()
        .map(|(tenant, access)| (tenant.to_string(), Value::String(access.as_str().into())))
        .collect();
    Value::Object(object).to_string()
}

fn decode_organizations(json: &str) -> Result<BTreeMap<TenantId, Access>, Error> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| Error::Corrupt(format!("organizations {json:?}: {error}")))?;
    let Value::Object(object) = value else {
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

fn principal(text: &str) -> Result<Principal, Error> {
    Principal::parse(text).map_err(|error| Error::Corrupt(error.to_string()))
}

fn client_from_row(row: &PgRow) -> Result<OAuthClient, Error> {
    let kind = row.text(1)?;
    let auth_method = row.text(6)?;
    let redirect_uris: Vec<String> = row.try_get(5)?;
    Ok(OAuthClient {
        id: row.text(0)?,
        kind: ClientKind::parse(&kind)
            .ok_or_else(|| Error::Corrupt(format!("client kind {kind:?}")))?,
        name: row.text(2)?,
        uri: row.opt_text(3)?,
        logo_uri: row.opt_text(4)?,
        redirect_uris,
        auth_method: ClientAuthMethod::parse(&auth_method)
            .ok_or_else(|| Error::Corrupt(format!("authentication method {auth_method:?}")))?,
        secret_hash: row.opt_digest(7)?,
        fetched_at: row.opt_ts(8)?,
        expires_at: row.opt_ts(9)?,
        created_at: row.ts(10)?,
        last_used_at: row.opt_ts(11)?,
    })
}

fn code_from_row(row: &PgRow) -> Result<OAuthCode, Error> {
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

fn connection_from_row(row: &PgRow) -> Result<ports::Connection, Error> {
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

fn token_from_row(row: &PgRow) -> Result<ConnectionToken, Error> {
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

fn grant_from_row(row: &PgRow) -> Result<Grant, Error> {
    Ok(Grant {
        tenant: row.tenant(0)?,
        principal: principal(&row.text(1)?)?,
        role: row.text(2)?,
        allowed_labels: decode_labels(&row.text(3)?)?,
        granted_by: row.opt_text(4)?.map(|by| principal(&by)).transpose()?,
        granted_at: row.ts(5)?,
    })
}

async fn find_connection(
    conn: &mut PgConnection,
    id: &str,
    lock: bool,
) -> Result<Option<ports::Connection>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {CONNECTION_COLUMNS} FROM connections WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| connection_from_row(&row)).transpose()
}

/// Replaces the grants of a connection with one per organization, given by its person.
async fn open(
    conn: &mut PgConnection,
    connection: &ports::Connection,
    organizations: &[OpenedOrganization],
    now: Timestamp,
    changes: &mut Vec<ControlPlaneChange>,
) -> Result<(), Error> {
    let grantee = connection.principal();
    delete_grants(conn, &grantee, changes).await?;
    changes.push(connection_changed(&connection.id));
    let principal = grantee.to_string();
    for opened in organizations {
        changes.push(grant_changed(&opened.tenant, &grantee));
        sqlx::query(
            "INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6)",
        )
        .bind(tenant(&opened.tenant))
        .bind(principal.as_str())
        .bind(opened.access.role())
        .bind(encode_labels(&opened.allowed_labels))
        .bind(Principal::from(&connection.user).to_string())
        .bind(ts(now))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Deletes the grants and the tokens of a connection.
async fn forget(
    conn: &mut PgConnection,
    id: &str,
    changes: &mut Vec<ControlPlaneChange>,
) -> Result<(), Error> {
    changes.push(connection_changed(id));
    delete_grants(conn, &Principal::Connection(id.to_owned()), changes).await?;
    sqlx::query("DELETE FROM connection_tokens WHERE connection_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Revokes a live connection: its grants and tokens go. `false` when it is unknown or revoked.
async fn revoke(
    conn: &mut PgConnection,
    id: &str,
    now: Timestamp,
    changes: &mut Vec<ControlPlaneChange>,
) -> Result<bool, Error> {
    let revoked =
        sqlx::query("UPDATE connections SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .bind(ts(now))
            .execute(&mut *conn)
            .await?;
    if revoked.rows_affected() == 0 {
        return Ok(false);
    }
    forget(conn, id, changes).await?;
    Ok(true)
}

/// Stores new tokens of a connection; a hash already stored is a `Conflict`.
async fn issue(
    conn: &mut PgConnection,
    connection: &str,
    tokens: [(&NewToken, ConnectionTokenKind); 2],
    now: Timestamp,
) -> Result<(), Fail> {
    for (token, kind) in tokens {
        let inserted = sqlx::query(
            "INSERT INTO connection_tokens (hash, connection_id, kind, expires_at, created_at, \
             used_at) VALUES ($1, $2, $3, $4, $5, NULL) ON CONFLICT (hash) DO NOTHING",
        )
        .bind(bytes(token.hash))
        .bind(connection)
        .bind(kind.as_str())
        .bind(ts(token.expires_at))
        .bind(ts(now))
        .execute(&mut *conn)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(Failure::Port(StoreError::Conflict(
                "a connection's token with this hash already exists".into(),
            )));
        }
    }
    Ok(())
}

impl PostgresStore {
    pub(crate) async fn put_oauth_client_row(&self, client: &OAuthClient) -> Result<(), Error> {
        sqlx::query(&format!(
            "INSERT INTO oauth_clients ({CLIENT_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (id) DO UPDATE SET kind = EXCLUDED.kind, name = EXCLUDED.name, \
             uri = EXCLUDED.uri, logo_uri = EXCLUDED.logo_uri, \
             redirect_uris = EXCLUDED.redirect_uris, auth_method = EXCLUDED.auth_method, \
             secret_hash = EXCLUDED.secret_hash, fetched_at = EXCLUDED.fetched_at, \
             expires_at = EXCLUDED.expires_at"
        ))
        .bind(client.id.as_str())
        .bind(client.kind.as_str())
        .bind(client.name.as_str())
        .bind(client.uri.as_deref())
        .bind(client.logo_uri.as_deref())
        .bind(&client.redirect_uris)
        .bind(client.auth_method.as_str())
        .bind(client.secret_hash.map(bytes))
        .bind(opt_ts(client.fetched_at))
        .bind(opt_ts(client.expires_at))
        .bind(ts(client.created_at))
        .bind(opt_ts(client.last_used_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn oauth_client_row(&self, id: &str) -> Result<Option<OAuthClient>, Error> {
        let row = sqlx::query(&format!(
            "SELECT {CLIENT_COLUMNS} FROM oauth_clients WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| client_from_row(&row)).transpose()
    }

    pub(crate) async fn count_oauth_client_rows(&self, kind: ClientKind) -> Result<u64, Error> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_clients WHERE kind = $1")
            .bind(kind.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub(crate) async fn purge_oauth_client_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let purged = sqlx::query(
            "DELETE FROM oauth_clients WHERE id IN (\
                 SELECT id FROM oauth_clients \
                 WHERE COALESCE(last_used_at, fetched_at, created_at) < $1 \
                 AND NOT EXISTS (SELECT 1 FROM connections \
                     WHERE connections.client_id = oauth_clients.id) \
                 LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(ts(before))
        .bind(crate::sql::limit(limit))
        .execute(&self.pool)
        .await?;
        Ok(purged.rows_affected())
    }

    pub(crate) async fn put_oauth_code_row(&self, code: &OAuthCode) -> Result<(), StoreError> {
        let inserted = sqlx::query(&format!(
            "INSERT INTO oauth_codes ({CODE_FIELDS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8::jsonb, $9, $10, $11) \
             ON CONFLICT (hash) DO NOTHING"
        ))
        .bind(bytes(code.hash))
        .bind(code.client_id.as_str())
        .bind(code.user.as_str())
        .bind(code.redirect_uri.as_str())
        .bind(code.code_challenge.as_str())
        .bind(code.resource.as_str())
        .bind(encode_organizations(&code.organizations))
        .bind(encode_labels(&code.labels))
        .bind(ts(code.expires_at))
        .bind(opt_ts(code.used_at))
        .bind(code.connection.as_deref())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!(
                "an authorization code with hash {} already exists",
                code.hash.short()
            )));
        }
        Ok(())
    }

    pub(crate) async fn oauth_code_row(&self, hash: Digest) -> Result<Option<OAuthCode>, Error> {
        let row = sqlx::query(&format!(
            "SELECT {CODE_COLUMNS} FROM oauth_codes WHERE hash = $1"
        ))
        .bind(bytes(hash))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| code_from_row(&row)).transpose()
    }

    async fn redeem_once(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, Fail> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!(
            "SELECT {CODE_COLUMNS} FROM oauth_codes WHERE hash = $1 FOR UPDATE"
        ))
        .bind(bytes(hash))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(code) = row.map(|row| code_from_row(&row)).transpose()? else {
            return Ok(CodeRedemption::Unknown);
        };
        let mut changes = Vec::new();
        if code.used_at.is_some() {
            // A replay: its connection is revoked, and that is committed.
            if let Some(id) = &code.connection {
                revoke(&mut tx, id, now, &mut changes).await?;
            }
            notify(&mut tx, &changes).await?;
            tx.commit().await?;
            return Ok(CodeRedemption::Used);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "rewind connection\u{1f}{}\u{1f}{}",
                new.connection.user, new.connection.client_id
            ))
            .execute(&mut *tx)
            .await?;
        let kept = sqlx::query(&format!(
            "SELECT {CONNECTION_COLUMNS} FROM connections WHERE user_id = $1 AND client_id = $2 \
             AND revoked_at IS NULL FOR UPDATE"
        ))
        .bind(new.connection.user.as_str())
        .bind(new.connection.client_id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| connection_from_row(&row))
        .transpose()?;
        let connection = match kept {
            Some(kept) => {
                sqlx::query("UPDATE connections SET expires_at = $2 WHERE id = $1")
                    .bind(kept.id.as_str())
                    .bind(ts(new.connection.expires_at))
                    .execute(&mut *tx)
                    .await?;
                ports::Connection {
                    expires_at: new.connection.expires_at,
                    ..kept
                }
            }
            None => {
                let created = &new.connection;
                let inserted = sqlx::query(&format!(
                    "INSERT INTO connections ({CONNECTION_COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING"
                ))
                .bind(created.id.as_str())
                .bind(created.user.as_str())
                .bind(created.client_id.as_str())
                .bind(created.client_name.as_str())
                .bind(ts(created.created_at))
                .bind(opt_ts(created.last_used_at))
                .bind(opt_ts(created.revoked_at))
                .bind(ts(created.expires_at))
                .execute(&mut *tx)
                .await?;
                if inserted.rows_affected() == 0 {
                    return Err(Failure::Port(StoreError::Conflict(format!(
                        "a connection {} already exists",
                        created.id
                    ))));
                }
                created.clone()
            }
        };
        issue(
            &mut tx,
            &connection.id,
            [
                (&new.access, ConnectionTokenKind::Access),
                (&new.refresh, ConnectionTokenKind::Refresh),
            ],
            now,
        )
        .await?;
        open(&mut tx, &connection, &new.organizations, now, &mut changes).await?;
        sqlx::query("UPDATE oauth_codes SET used_at = $2, connection_id = $3 WHERE hash = $1")
            .bind(bytes(hash))
            .bind(ts(now))
            .bind(connection.id.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE oauth_clients SET last_used_at = $2 WHERE id = $1")
            .bind(connection.client_id.as_str())
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(CodeRedemption::Redeemed(connection))
    }

    pub(crate) async fn redeem_oauth_code_rows(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, StoreError> {
        Ok(retrying(|| self.redeem_once(hash, new, now)).await?)
    }

    pub(crate) async fn purge_oauth_code_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let purged = sqlx::query(
            "DELETE FROM oauth_codes WHERE hash IN (\
                 SELECT hash FROM oauth_codes WHERE expires_at < $1 \
                 LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(ts(before))
        .bind(crate::sql::limit(limit))
        .execute(&self.pool)
        .await?;
        Ok(purged.rows_affected())
    }

    pub(crate) async fn connection_row(
        &self,
        id: &str,
    ) -> Result<Option<ports::Connection>, Error> {
        let mut conn = self.pool.acquire().await?;
        find_connection(&mut conn, id, false).await
    }

    pub(crate) async fn connections_of_user(
        &self,
        user: &UserId,
    ) -> Result<Vec<ports::Connection>, Error> {
        let rows = sqlx::query(&format!(
            "SELECT {CONNECTION_COLUMNS} FROM connections WHERE user_id = $1 \
             AND revoked_at IS NULL ORDER BY created_at, id"
        ))
        .bind(user.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(connection_from_row).collect()
    }

    pub(crate) async fn connection_grant_rows(&self, id: &str) -> Result<Vec<Grant>, Error> {
        let rows = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM grants WHERE principal = $1 ORDER BY tenant"
        ))
        .bind(Principal::Connection(id.to_owned()).to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(grant_from_row).collect()
    }

    async fn set_organizations_once(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(connection) = find_connection(&mut tx, id, true)
            .await?
            .filter(|connection| connection.revoked_at.is_none())
        else {
            return Ok(false);
        };
        let mut changes = Vec::new();
        open(&mut tx, &connection, organizations, now, &mut changes).await?;
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn set_connection_organization_rows(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        Ok(retrying(|| self.set_organizations_once(id, organizations, now)).await?)
    }

    pub(crate) async fn touch_connection_row(&self, id: &str, now: Timestamp) -> Result<(), Error> {
        sqlx::query("UPDATE connections SET last_used_at = $2 WHERE id = $1")
            .bind(id)
            .bind(ts(now))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke_once(&self, id: &str, now: Timestamp) -> Result<bool, Fail> {
        let mut tx = self.pool.begin().await?;
        let mut changes = Vec::new();
        let revoked = revoke(&mut tx, id, now, &mut changes).await?;
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(revoked)
    }

    pub(crate) async fn revoke_connection_row(
        &self,
        id: &str,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        Ok(retrying(|| self.revoke_once(id, now)).await?)
    }

    async fn purge_connections_once(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, Fail> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM connections WHERE expires_at < $1 OR revoked_at < $2 \
             LIMIT $3 FOR UPDATE SKIP LOCKED",
        )
        .bind(ts(lapsed_before))
        .bind(ts(revoked_before))
        .bind(crate::sql::limit(limit))
        .fetch_all(&mut *tx)
        .await?;
        let mut changes = Vec::new();
        for id in &ids {
            forget(&mut tx, id, &mut changes).await?;
            sqlx::query("DELETE FROM connections WHERE id = $1")
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?;
        }
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(ids.len() as u64)
    }

    pub(crate) async fn purge_connection_rows(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        Ok(retrying(|| self.purge_connections_once(lapsed_before, revoked_before, limit)).await?)
    }

    pub(crate) async fn connection_token_row(
        &self,
        hash: Digest,
    ) -> Result<Option<ConnectionToken>, Error> {
        let row = sqlx::query(&format!(
            "SELECT {TOKEN_COLUMNS} FROM connection_tokens WHERE hash = $1"
        ))
        .bind(bytes(hash))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| token_from_row(&row)).transpose()
    }

    async fn rotate_once(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, Fail> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!(
            "SELECT {TOKEN_COLUMNS} FROM connection_tokens WHERE hash = $1 AND kind = $2 \
             FOR UPDATE"
        ))
        .bind(bytes(hash))
        .bind(ConnectionTokenKind::Refresh.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(token) = row.map(|row| token_from_row(&row)).transpose()? else {
            return Ok(Rotation::Unknown);
        };
        let Some(connection) = find_connection(&mut tx, &token.connection, true)
            .await?
            .filter(|connection| connection.revoked_at.is_none())
        else {
            return Ok(Rotation::Unknown);
        };
        if token.used_at.is_some() {
            let mut changes = Vec::new();
            revoke(&mut tx, &connection.id, now, &mut changes).await?;
            notify(&mut tx, &changes).await?;
            tx.commit().await?;
            return Ok(Rotation::Used);
        }
        if token.expires_at <= now {
            return Ok(Rotation::Expired);
        }
        issue(
            &mut tx,
            &connection.id,
            [
                (access, ConnectionTokenKind::Access),
                (refresh, ConnectionTokenKind::Refresh),
            ],
            now,
        )
        .await?;
        sqlx::query("UPDATE connection_tokens SET used_at = $2 WHERE hash = $1")
            .bind(bytes(hash))
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE connections SET expires_at = $2 WHERE id = $1")
            .bind(connection.id.as_str())
            .bind(ts(refresh.expires_at))
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, &[connection_changed(&connection.id)]).await?;
        tx.commit().await?;
        Ok(Rotation::Rotated(ports::Connection {
            expires_at: refresh.expires_at,
            ..connection
        }))
    }

    pub(crate) async fn rotate_refresh_token_rows(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, StoreError> {
        Ok(retrying(|| self.rotate_once(hash, access, refresh, now)).await?)
    }

    pub(crate) async fn purge_connection_token_rows(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, Error> {
        let mut tx = self.pool.begin().await?;
        let purged: Vec<String> = sqlx::query_scalar(
            "DELETE FROM connection_tokens WHERE hash IN (\
                 SELECT hash FROM connection_tokens WHERE expires_at < $1 \
                 LIMIT $2 FOR UPDATE SKIP LOCKED) RETURNING connection_id",
        )
        .bind(ts(before))
        .bind(crate::sql::limit(limit))
        .fetch_all(&mut *tx)
        .await?;
        let connections: std::collections::BTreeSet<&str> =
            purged.iter().map(String::as_str).collect();
        let changes: Vec<ControlPlaneChange> =
            connections.into_iter().map(connection_changed).collect();
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(purged.len() as u64)
    }
}
