//! `AccessStore` on Turso: API keys, the tenants' own roles, grants and default roles, end
//! users' tokens and the keys that sign them; people's connections (`crate::connections`). The
//! in-memory `MemoryAccess` of `rewind-engine` is the reference of every behavior here; both pass
//! the same conformance suite.
//!
//! Every call holds the one connection for its whole duration, and the calls that read before
//! they write (the kept role's last holder, a role in use, a claim) are one transaction: the
//! check and the write are atomic.
//!
//! Every write sends what it changed once it committed, while it still holds the connection
//! (`TursoStore::changes`): a key, a token or token key, a role, a grant, a connection, and the
//! tenant for its default role. What only records a use (a connection used, an OAuth client or
//! code) sends nothing: no change names it.

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    AccessStore, ApiKey, ClientKind, CodeRedemption, Connection as PersonConnection,
    ConnectionToken, ControlPlaneChange, Grant, GrantClaim, GrantDeletion, GrantWrite,
    NamespaceName, NewConnection, NewToken, OAuthClient, OAuthCode, OpenedOrganization, Principal,
    Role, RoleDeletion, Rotation, StoreError, TenantId, Token, TokenKey, UserId,
};

use turso::{Connection, Row};

use crate::codec::{
    decode_labels, decode_permissions, encode_labels, encode_permissions, parse_principal,
};
use crate::sql::{self, Columns, digest, opt_text, opt_ts, tenant, text, transaction, ts};
use crate::{Error, TursoStore};

impl From<Error> for StoreError {
    fn from(err: Error) -> Self {
        StoreError::Backend(err.to_string())
    }
}

/// The columns of an [`ApiKey`], read by [`key_from_row`].
const KEY_COLUMNS: &str = "hash, tenant, name, created_by, created_at, revoked_at";

/// The columns of a [`Role`], read by [`role_from_row`].
const ROLE_COLUMNS: &str = "tenant, name, description, permissions, created_at, updated_at";

/// The columns of a [`Grant`], read by [`grant_from_row`].
const GRANT_COLUMNS: &str = "tenant, principal, role, allowed_labels, granted_by, granted_at";

/// The columns of a [`Token`], read by [`token_from_row`].
const TOKEN_COLUMNS: &str = "hash, tenant, namespace, permissions, labels, expires_at, created_by, \
     created_at, revoked_at";

/// The columns of a [`TokenKey`], read by [`token_key_from_row`].
const TOKEN_KEY_COLUMNS: &str = "kid, tenant, name, jwk, permissions, created_by, created_at";

fn token_from_row(row: &Row) -> Result<Token, Error> {
    let namespace = row.text(2)?;
    Ok(Token {
        hash: row.digest(0)?,
        tenant: row.tenant(1)?,
        namespace: NamespaceName::new(namespace.clone())
            .map_err(|_| Error::Corrupt(format!("invalid namespace {namespace:?}")))?,
        permissions: decode_permissions(&row.text(3)?)?,
        labels: decode_labels(&row.text(4)?)?,
        expires_at: row.ts(5)?,
        created_by: parse_principal(&row.text(6)?)?,
        created_at: row.ts(7)?,
        revoked_at: row.opt_ts(8)?,
    })
}

fn token_key_from_row(row: &Row) -> Result<TokenKey, Error> {
    Ok(TokenKey {
        kid: row.text(0)?,
        tenant: row.tenant(1)?,
        name: row.text(2)?,
        jwk: row.text(3)?,
        permissions: decode_permissions(&row.text(4)?)?,
        created_by: parse_principal(&row.text(5)?)?,
        created_at: row.ts(6)?,
    })
}

fn key_from_row(row: &Row) -> Result<ApiKey, Error> {
    Ok(ApiKey {
        hash: row.digest(0)?,
        tenant: row.tenant(1)?,
        name: row.text(2)?,
        created_by: row
            .opt_text(3)?
            .map(|by| parse_principal(&by))
            .transpose()?,
        created_at: row.ts(4)?,
        revoked_at: row.opt_ts(5)?,
    })
}

fn role_from_row(row: &Row) -> Result<Role, Error> {
    Ok(Role {
        tenant: row.tenant(0)?,
        name: row.text(1)?,
        description: row.text(2)?,
        permissions: decode_permissions(&row.text(3)?)?,
        created_at: row.ts(4)?,
        updated_at: row.ts(5)?,
    })
}

fn grant_from_row(row: &Row) -> Result<Grant, Error> {
    Ok(Grant {
        tenant: row.tenant(0)?,
        principal: parse_principal(&row.text(1)?)?,
        role: row.text(2)?,
        allowed_labels: decode_labels(&row.text(3)?)?,
        granted_by: row
            .opt_text(4)?
            .map(|by| parse_principal(&by))
            .transpose()?,
        granted_at: row.ts(5)?,
    })
}

async fn find_grant(
    connection: &Connection,
    tenant_id: &TenantId,
    principal: &Principal,
) -> Result<Option<Grant>, Error> {
    let row = sql::query_row(
        connection,
        &format!("SELECT {GRANT_COLUMNS} FROM grants WHERE tenant = ? AND principal = ?"),
        vec![tenant(tenant_id), text(principal.to_string())],
    )
    .await?;
    row.map(|row| grant_from_row(&row)).transpose()
}

async fn write_grant(connection: &Connection, grant: &Grant) -> Result<(), Error> {
    sql::execute(
        connection,
        "INSERT OR REPLACE INTO grants (tenant, principal, role, allowed_labels, granted_by, \
         granted_at) VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            tenant(&grant.tenant),
            text(grant.principal.to_string()),
            text(grant.role.as_str()),
            text(encode_labels(&grant.allowed_labels)),
            opt_text(grant.granted_by.as_ref().map(ToString::to_string)),
            ts(grant.granted_at),
        ],
    )
    .await?;
    Ok(())
}

async fn remove_grant(
    connection: &Connection,
    tenant_id: &TenantId,
    principal: &Principal,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "DELETE FROM grants WHERE tenant = ? AND principal = ?",
        vec![tenant(tenant_id), text(principal.to_string())],
    )
    .await?;
    Ok(())
}

pub(crate) fn grant_changed(tenant_id: &TenantId, principal: &Principal) -> ControlPlaneChange {
    ControlPlaneChange::Grant {
        tenant: tenant_id.clone(),
        principal: principal.clone(),
    }
}

fn role_changed(tenant_id: &TenantId, name: &str) -> ControlPlaneChange {
    ControlPlaneChange::Role {
        tenant: tenant_id.clone(),
        name: name.to_owned(),
    }
}

fn token_changed(tenant_id: &TenantId, hash: Digest) -> ControlPlaneChange {
    ControlPlaneChange::Token {
        tenant: tenant_id.clone(),
        hash,
    }
}

/// The tokens a statement selected as `tenant, hash`.
fn tokens_of(rows: &[Row]) -> Result<Vec<(TenantId, Digest)>, Error> {
    rows.iter()
        .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
        .collect()
}

/// Whether taking `principal`'s grant away from the role `keep` would leave the tenant without a
/// member holding it. Keys and invitations never count.
async fn holds_last(
    connection: &Connection,
    tenant_id: &TenantId,
    principal: &Principal,
    keep: &str,
) -> Result<bool, Error> {
    if !matches!(principal, Principal::User(_)) {
        return Ok(false);
    }
    let held = find_grant(connection, tenant_id, principal)
        .await?
        .is_some_and(|grant| grant.role == keep);
    if !held {
        return Ok(false);
    }
    let other = sql::query_row(
        connection,
        "SELECT 1 FROM grants WHERE tenant = ? AND role = ? AND principal LIKE 'user:%' \
         AND principal <> ? LIMIT 1",
        vec![tenant(tenant_id), text(keep), text(principal.to_string())],
    )
    .await?;
    Ok(other.is_none())
}

impl AccessStore for TursoStore {
    async fn put_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let exists = sql::query_row(
                connection,
                "SELECT 1 FROM api_keys WHERE hash = ?",
                vec![digest(key.hash)],
            )
            .await?;
            if exists.is_some() {
                return Err(StoreError::Conflict(format!(
                    "an API key with hash {} already exists",
                    key.hash.short()
                )));
            }
            sql::execute(
                connection,
                "INSERT INTO api_keys (hash, tenant, name, created_by, created_at, revoked_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                vec![
                    digest(key.hash),
                    tenant(&key.tenant),
                    text(key.name.as_str()),
                    opt_text(key.created_by.as_ref().map(ToString::to_string)),
                    ts(key.created_at),
                    opt_ts(key.revoked_at),
                ],
            )
            .await?;
            Ok::<_, StoreError>(())
        })
        .await?;
        self.send([ControlPlaneChange::Key {
            tenant: key.tenant.clone(),
            hash: key.hash,
        }]);
        Ok(())
    }

    async fn find_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            &format!("SELECT {KEY_COLUMNS} FROM api_keys WHERE hash = ?"),
            vec![digest(hash)],
        )
        .await?;
        Ok(row.map(|row| key_from_row(&row)).transpose()?)
    }

    async fn list_keys(&self, tenant_id: &TenantId) -> Result<Vec<ApiKey>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            &format!(
                "SELECT {KEY_COLUMNS} FROM api_keys WHERE tenant = ? ORDER BY created_at, hash"
            ),
            vec![tenant(tenant_id)],
        )
        .await?;
        Ok(rows
            .iter()
            .map(key_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn revoke_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let revoked = transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT tenant FROM api_keys WHERE hash = ? AND revoked_at IS NULL",
                vec![digest(hash)],
            )
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            sql::execute(
                connection,
                "UPDATE api_keys SET revoked_at = ? WHERE hash = ? AND revoked_at IS NULL",
                vec![ts(now), digest(hash)],
            )
            .await?;
            Ok::<_, Error>(Some(row.tenant(0)?))
        })
        .await?;
        let Some(tenant_id) = revoked else {
            return Ok(false);
        };
        self.send([ControlPlaneChange::Key {
            tenant: tenant_id,
            hash,
        }]);
        Ok(true)
    }

    // End users' tokens and token keys

    async fn put_token(&self, token: &Token) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let taken = sql::query_row(
                connection,
                "SELECT 1 FROM tokens WHERE hash = ?",
                vec![digest(token.hash)],
            )
            .await?;
            if taken.is_some() {
                return Err(StoreError::Conflict(format!(
                    "a token with hash {} already exists",
                    token.hash.short()
                )));
            }
            sql::execute(
                connection,
                "INSERT INTO tokens (hash, tenant, namespace, permissions, labels, expires_at, \
                 created_by, created_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    digest(token.hash),
                    tenant(&token.tenant),
                    text(token.namespace.as_str()),
                    text(encode_permissions(&token.permissions)),
                    text(encode_labels(&token.labels)),
                    ts(token.expires_at),
                    text(token.created_by.to_string()),
                    ts(token.created_at),
                    opt_ts(token.revoked_at),
                ],
            )
            .await?;
            Ok::<_, StoreError>(())
        })
        .await?;
        self.send([token_changed(&token.tenant, token.hash)]);
        Ok(())
    }

    async fn find_token(&self, hash: Digest) -> Result<Option<Token>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            &format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE hash = ?"),
            vec![digest(hash)],
        )
        .await?;
        Ok(row.map(|row| token_from_row(&row)).transpose()?)
    }

    async fn revoke_token(
        &self,
        tenant_id: &TenantId,
        hash: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let revoked = sql::execute(
            &connection,
            "UPDATE tokens SET revoked_at = ? WHERE hash = ? AND tenant = ? AND revoked_at IS NULL",
            vec![ts(now), digest(hash), tenant(tenant_id)],
        )
        .await?;
        if revoked == 0 {
            return Ok(false);
        }
        self.send([token_changed(tenant_id, hash)]);
        Ok(true)
    }

    async fn revoke_tokens(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let revoked = transaction(connection, async {
            let rows = sql::query(
                connection,
                "SELECT tenant, hash FROM tokens \
                 WHERE tenant = ? AND namespace = ? AND revoked_at IS NULL",
                vec![tenant(tenant_id), text(namespace.as_str())],
            )
            .await?;
            sql::execute(
                connection,
                "UPDATE tokens SET revoked_at = ? \
                 WHERE tenant = ? AND namespace = ? AND revoked_at IS NULL",
                vec![ts(now), tenant(tenant_id), text(namespace.as_str())],
            )
            .await?;
            tokens_of(&rows)
        })
        .await?;
        self.send(
            revoked
                .iter()
                .map(|(owner, hash)| token_changed(owner, *hash)),
        );
        Ok(revoked.len() as u64)
    }

    async fn count_tokens(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT COUNT(*) FROM tokens WHERE tenant = ? AND namespace = ? \
             AND revoked_at IS NULL AND expires_at > ?",
            vec![tenant(tenant_id), text(namespace.as_str()), ts(now)],
        )
        .await?;
        Ok(row.map(|row| row.u64(0)).transpose()?.unwrap_or(0))
    }

    async fn purge_tokens(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let purged = transaction(connection, async {
            let rows = sql::query(
                connection,
                "SELECT tenant, hash FROM tokens WHERE expires_at < ?1 OR revoked_at < ?1 \
                 LIMIT ?2",
                vec![ts(before), sql::limit(limit)],
            )
            .await?;
            let purged = tokens_of(&rows)?;
            for (_, hash) in &purged {
                sql::execute(
                    connection,
                    "DELETE FROM tokens WHERE hash = ?",
                    vec![digest(*hash)],
                )
                .await?;
            }
            Ok::<_, Error>(purged)
        })
        .await?;
        self.send(
            purged
                .iter()
                .map(|(owner, hash)| token_changed(owner, *hash)),
        );
        Ok(purged.len() as u64)
    }

    async fn put_token_key(&self, key: &TokenKey) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let taken = sql::query_row(
                connection,
                "SELECT 1 FROM token_keys WHERE kid = ?",
                vec![text(key.kid.as_str())],
            )
            .await?;
            if taken.is_some() {
                return Err(StoreError::Conflict(format!(
                    "a token key {} already exists",
                    key.kid
                )));
            }
            sql::execute(
                connection,
                "INSERT INTO token_keys (kid, tenant, name, jwk, permissions, created_by, \
                 created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(key.kid.as_str()),
                    tenant(&key.tenant),
                    text(key.name.as_str()),
                    text(key.jwk.as_str()),
                    text(encode_permissions(&key.permissions)),
                    text(key.created_by.to_string()),
                    ts(key.created_at),
                ],
            )
            .await?;
            Ok::<_, StoreError>(())
        })
        .await?;
        self.send([ControlPlaneChange::TokenKey {
            tenant: key.tenant.clone(),
            kid: key.kid.clone(),
        }]);
        Ok(())
    }

    async fn find_token_key(&self, kid: &str) -> Result<Option<TokenKey>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            &format!("SELECT {TOKEN_KEY_COLUMNS} FROM token_keys WHERE kid = ?"),
            vec![text(kid)],
        )
        .await?;
        Ok(row.map(|row| token_key_from_row(&row)).transpose()?)
    }

    async fn list_token_keys(&self, tenant_id: &TenantId) -> Result<Vec<TokenKey>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            &format!(
                "SELECT {TOKEN_KEY_COLUMNS} FROM token_keys WHERE tenant = ? \
                 ORDER BY created_at, kid"
            ),
            vec![tenant(tenant_id)],
        )
        .await?;
        Ok(rows
            .iter()
            .map(token_key_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_token_key(&self, tenant_id: &TenantId, kid: &str) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let deleted = sql::execute(
            &connection,
            "DELETE FROM token_keys WHERE kid = ? AND tenant = ?",
            vec![text(kid), tenant(tenant_id)],
        )
        .await?;
        if deleted == 0 {
            return Ok(false);
        }
        self.send([ControlPlaneChange::TokenKey {
            tenant: tenant_id.clone(),
            kid: kid.to_owned(),
        }]);
        Ok(true)
    }

    async fn put_role(&self, role: &Role) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT INTO roles (tenant, name, description, permissions, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (tenant, name) DO UPDATE SET description = excluded.description, \
             permissions = excluded.permissions, updated_at = excluded.updated_at",
            vec![
                tenant(&role.tenant),
                text(role.name.as_str()),
                text(role.description.as_str()),
                text(encode_permissions(&role.permissions)),
                ts(role.created_at),
                ts(role.updated_at),
            ],
        )
        .await?;
        self.send([role_changed(&role.tenant, &role.name)]);
        Ok(())
    }

    async fn get_role(&self, tenant_id: &TenantId, name: &str) -> Result<Option<Role>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            &format!("SELECT {ROLE_COLUMNS} FROM roles WHERE tenant = ? AND name = ?"),
            vec![tenant(tenant_id), text(name)],
        )
        .await?;
        Ok(row.map(|row| role_from_row(&row)).transpose()?)
    }

    async fn list_roles(&self, tenant_id: &TenantId) -> Result<Vec<Role>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            &format!("SELECT {ROLE_COLUMNS} FROM roles WHERE tenant = ? ORDER BY name"),
            vec![tenant(tenant_id)],
        )
        .await?;
        Ok(rows
            .iter()
            .map(role_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_role(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<RoleDeletion, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let deletion = transaction(connection, async {
            let exists = sql::query_row(
                connection,
                "SELECT 1 FROM roles WHERE tenant = ? AND name = ?",
                vec![tenant(tenant_id), text(name)],
            )
            .await?;
            if exists.is_none() {
                return Ok(RoleDeletion::NotFound);
            }
            let in_use = sql::query_row(
                connection,
                "SELECT 1 FROM grants WHERE tenant = ? AND role = ? LIMIT 1",
                vec![tenant(tenant_id), text(name)],
            )
            .await?;
            if in_use.is_some() {
                return Ok(RoleDeletion::InUse);
            }
            sql::execute(
                connection,
                "DELETE FROM roles WHERE tenant = ? AND name = ?",
                vec![tenant(tenant_id), text(name)],
            )
            .await?;
            Ok::<_, StoreError>(RoleDeletion::Deleted)
        })
        .await?;
        if deletion == RoleDeletion::Deleted {
            self.send([role_changed(tenant_id, name)]);
        }
        Ok(deletion)
    }

    async fn put_grant(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let written = transaction(connection, async {
            if let Some(keep) = keep
                && grant.role != keep
                && holds_last(connection, &grant.tenant, &grant.principal, keep).await?
            {
                return Ok(GrantWrite::LastHolder);
            }
            write_grant(connection, grant).await?;
            Ok::<_, StoreError>(GrantWrite::Written)
        })
        .await?;
        if written == GrantWrite::Written {
            self.send([grant_changed(&grant.tenant, &grant.principal)]);
        }
        Ok(written)
    }

    async fn grant(
        &self,
        tenant_id: &TenantId,
        principal: &Principal,
    ) -> Result<Option<Grant>, StoreError> {
        let connection = self.connection().await?;
        Ok(find_grant(&connection, tenant_id, principal).await?)
    }

    async fn list_grants(&self, tenant_id: &TenantId) -> Result<Vec<Grant>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            &format!("SELECT {GRANT_COLUMNS} FROM grants WHERE tenant = ? ORDER BY principal"),
            vec![tenant(tenant_id)],
        )
        .await?;
        Ok(rows
            .iter()
            .map(grant_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_grant(
        &self,
        tenant_id: &TenantId,
        principal: &Principal,
        keep: Option<&str>,
    ) -> Result<GrantDeletion, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let deletion = transaction(connection, async {
            if find_grant(connection, tenant_id, principal)
                .await?
                .is_none()
            {
                return Ok(GrantDeletion::NotFound);
            }
            if let Some(keep) = keep
                && holds_last(connection, tenant_id, principal, keep).await?
            {
                return Ok(GrantDeletion::LastHolder);
            }
            remove_grant(connection, tenant_id, principal).await?;
            Ok::<_, StoreError>(GrantDeletion::Deleted)
        })
        .await?;
        if deletion == GrantDeletion::Deleted {
            self.send([grant_changed(tenant_id, principal)]);
        }
        Ok(deletion)
    }

    async fn claim_grant(
        &self,
        tenant_id: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let mut changes = Vec::new();
        let claim = transaction(connection, async {
            let pending = find_grant(connection, tenant_id, invitation).await?;
            if pending.is_some() {
                remove_grant(connection, tenant_id, invitation).await?;
                changes.push(grant_changed(tenant_id, invitation));
            }
            if let Some(existing) = find_grant(connection, tenant_id, user).await? {
                return Ok(GrantClaim::AlreadyGranted(existing));
            }
            let Some(pending) = pending else {
                return Ok(GrantClaim::NoGrant);
            };
            let claimed = Grant {
                principal: user.clone(),
                ..pending
            };
            write_grant(connection, &claimed).await?;
            changes.push(grant_changed(tenant_id, user));
            Ok::<_, StoreError>(GrantClaim::Claimed(claimed))
        })
        .await?;
        self.send(changes);
        Ok(claim)
    }

    async fn set_default_role(&self, tenant_id: &TenantId, role: &str) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO access_settings (tenant, default_role) VALUES (?, ?)",
            vec![tenant(tenant_id), text(role)],
        )
        .await?;
        // The default role is a setting of the tenant.
        self.send([ControlPlaneChange::Tenant {
            tenant: tenant_id.clone(),
        }]);
        Ok(())
    }

    async fn default_role(&self, tenant_id: &TenantId) -> Result<Option<String>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT default_role FROM access_settings WHERE tenant = ?",
            vec![tenant(tenant_id)],
        )
        .await?;
        Ok(row.map(|row| row.text(0)).transpose()?)
    }
    async fn put_oauth_client(&self, client: &OAuthClient) -> Result<(), StoreError> {
        Ok(self.put_oauth_client_row(client).await?)
    }

    async fn oauth_client(&self, id: &str) -> Result<Option<OAuthClient>, StoreError> {
        Ok(self.oauth_client_row(id).await?)
    }

    async fn count_oauth_clients(&self, kind: ClientKind) -> Result<u64, StoreError> {
        Ok(self.count_oauth_client_rows(kind).await?)
    }

    async fn purge_oauth_clients(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        Ok(self.purge_oauth_client_rows(before, limit).await?)
    }

    async fn put_oauth_code(&self, code: &OAuthCode) -> Result<(), StoreError> {
        self.put_oauth_code_row(code).await
    }

    async fn oauth_code(&self, hash: Digest) -> Result<Option<OAuthCode>, StoreError> {
        Ok(self.oauth_code_row(hash).await?)
    }

    async fn redeem_oauth_code(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, StoreError> {
        self.redeem_oauth_code_rows(hash, new, now).await
    }

    async fn purge_oauth_codes(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        Ok(self.purge_oauth_code_rows(before, limit).await?)
    }

    async fn connection(&self, id: &str) -> Result<Option<PersonConnection>, StoreError> {
        Ok(self.connection_row(id).await?)
    }

    async fn connections_of(&self, user: &UserId) -> Result<Vec<PersonConnection>, StoreError> {
        Ok(self.connections_of_user(user).await?)
    }

    async fn connection_grants(&self, id: &str) -> Result<Vec<Grant>, StoreError> {
        Ok(self.connection_grant_rows(id).await?)
    }

    async fn set_connection_organizations(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        Ok(self
            .set_connection_organization_rows(id, organizations, now)
            .await?)
    }

    async fn touch_connection(&self, id: &str, now: Timestamp) -> Result<(), StoreError> {
        Ok(self.touch_connection_row(id, now).await?)
    }

    async fn revoke_connection(&self, id: &str, now: Timestamp) -> Result<bool, StoreError> {
        Ok(self.revoke_connection_row(id, now).await?)
    }

    async fn purge_connections(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        Ok(self
            .purge_connection_rows(lapsed_before, revoked_before, limit)
            .await?)
    }

    async fn connection_token(&self, hash: Digest) -> Result<Option<ConnectionToken>, StoreError> {
        Ok(self.connection_token_row(hash).await?)
    }

    async fn rotate_refresh_token(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, StoreError> {
        self.rotate_refresh_token_rows(hash, access, refresh, now)
            .await
    }

    async fn purge_connection_tokens(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        Ok(self.purge_connection_token_rows(before, limit).await?)
    }
}
