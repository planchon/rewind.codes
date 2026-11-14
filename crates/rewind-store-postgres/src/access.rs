//! `AccessStore` on Postgres: API keys, the tenants' own roles, grants and default roles. The
//! in-memory `MemoryAccess` of `rewind-engine` is the reference of every behavior here; both pass
//! the same conformance suite.
//!
//! Several servers share the database. The writes that read before they write (a grant written
//! or deleted with a kept role, a claim, a role deleted unless in use) are one transaction that
//! first takes a transaction-scoped advisory lock on the tenant, as every write of the tenant's
//! grants does: two owners demoting each other at once are decided one after the other, and the
//! second finds itself the last holder. Grant writes are rare, so one lock per tenant costs
//! nothing that matters.

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, NamespaceName, Principal,
    Role, RoleDeletion, StoreError, TenantId, Token, TokenKey,
};

use crate::namespaces_not_kept;
use sqlx::PgConnection;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::codec::{decode_labels, encode_labels};
use crate::sql::{Columns, Failure, bytes, opt_ts, retrying, tenant, ts};
use crate::{Error, PostgresStore};

type Fail = Failure<StoreError>;

/// The columns of an [`ApiKey`], read by [`key_from_row`].
const KEY_COLUMNS: &str = "hash, tenant, name, created_by, created_at, revoked_at";

/// The columns of a [`Role`], read by [`role_from_row`].
const ROLE_COLUMNS: &str = "tenant, name, description, permissions, created_at, updated_at";

/// The columns of a [`Grant`], read by [`grant_from_row`].
const GRANT_COLUMNS: &str = "tenant, principal, role, allowed_labels::text, granted_by, granted_at";

fn parse_principal(text: &str) -> Result<Principal, Error> {
    Principal::parse(text).map_err(|err| Error::Corrupt(err.to_string()))
}

fn opt_principal(text: Option<String>) -> Result<Option<Principal>, Error> {
    text.map(|text| parse_principal(&text)).transpose()
}

fn key_from_row(row: &PgRow) -> Result<ApiKey, Error> {
    Ok(ApiKey {
        hash: row.digest(0)?,
        tenant: row.tenant(1)?,
        name: row.text(2)?,
        created_by: opt_principal(row.opt_text(3)?)?,
        created_at: row.ts(4)?,
        revoked_at: row.opt_ts(5)?,
    })
}

fn role_from_row(row: &PgRow) -> Result<Role, Error> {
    let permissions: Vec<String> = row.try_get(3)?;
    Ok(Role {
        tenant: row.tenant(0)?,
        name: row.text(1)?,
        description: row.text(2)?,
        permissions: permissions.into_iter().collect(),
        created_at: row.ts(4)?,
        updated_at: row.ts(5)?,
    })
}

fn grant_from_row(row: &PgRow) -> Result<Grant, Error> {
    Ok(Grant {
        tenant: row.tenant(0)?,
        principal: parse_principal(&row.text(1)?)?,
        role: row.text(2)?,
        allowed_labels: decode_labels(&row.text(3)?)?,
        granted_by: opt_principal(row.opt_text(4)?)?,
        granted_at: row.ts(5)?,
    })
}

/// Takes the advisory lock of the tenant's grants for the transaction. Parts are separated by
/// U+001F, which Postgres text can hold (unlike NUL) and tenant ids do not.
async fn lock_tenant(conn: &mut PgConnection, tenant_id: &TenantId) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("rewind access\u{1f}{tenant_id}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn find_grant(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    principal: &Principal,
) -> Result<Option<Grant>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {GRANT_COLUMNS} FROM grants WHERE tenant = $1 AND principal = $2"
    ))
    .bind(tenant(tenant_id))
    .bind(principal.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| grant_from_row(&row)).transpose()
}

async fn write_grant(conn: &mut PgConnection, grant: &Grant) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at) \
         VALUES ($1, $2, $3, $4::jsonb, $5, $6) \
         ON CONFLICT (tenant, principal) DO UPDATE SET role = EXCLUDED.role, \
             allowed_labels = EXCLUDED.allowed_labels, granted_by = EXCLUDED.granted_by, \
             granted_at = EXCLUDED.granted_at",
    )
    .bind(tenant(&grant.tenant))
    .bind(grant.principal.to_string())
    .bind(grant.role.as_str())
    .bind(encode_labels(&grant.allowed_labels))
    .bind(grant.granted_by.as_ref().map(ToString::to_string))
    .bind(ts(grant.granted_at))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether taking `principal`'s grant away from the role `keep` would leave the tenant without a
/// member holding it. Keys and invitations never count. Read under the tenant's lock.
async fn holds_last(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    principal: &Principal,
    keep: &str,
) -> Result<bool, Error> {
    if !matches!(principal, Principal::User(_)) {
        return Ok(false);
    }
    let held = find_grant(conn, tenant_id, principal)
        .await?
        .is_some_and(|grant| grant.role == keep);
    if !held {
        return Ok(false);
    }
    let other: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM grants WHERE tenant = $1 AND role = $2 \
         AND principal LIKE 'user:%' AND principal <> $3)",
    )
    .bind(tenant(tenant_id))
    .bind(keep)
    .bind(principal.to_string())
    .fetch_one(&mut *conn)
    .await?;
    Ok(!other)
}

impl PostgresStore {
    async fn delete_role_once(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<RoleDeletion, Fail> {
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant_id).await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM roles WHERE tenant = $1 AND name = $2)",
        )
        .bind(tenant(tenant_id))
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Ok(RoleDeletion::NotFound);
        }
        let in_use: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM grants WHERE tenant = $1 AND role = $2)",
        )
        .bind(tenant(tenant_id))
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        if in_use {
            return Ok(RoleDeletion::InUse);
        }
        sqlx::query("DELETE FROM roles WHERE tenant = $1 AND name = $2")
            .bind(tenant(tenant_id))
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(RoleDeletion::Deleted)
    }

    async fn put_grant_once(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, Fail> {
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, &grant.tenant).await?;
        if let Some(keep) = keep
            && grant.role != keep
            && holds_last(&mut tx, &grant.tenant, &grant.principal, keep).await?
        {
            return Ok(GrantWrite::LastHolder);
        }
        write_grant(&mut tx, grant).await?;
        tx.commit().await?;
        Ok(GrantWrite::Written)
    }

    async fn delete_grant_once(
        &self,
        tenant_id: &TenantId,
        principal: &Principal,
        keep: Option<&str>,
    ) -> Result<GrantDeletion, Fail> {
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant_id).await?;
        if find_grant(&mut tx, tenant_id, principal).await?.is_none() {
            return Ok(GrantDeletion::NotFound);
        }
        if let Some(keep) = keep
            && holds_last(&mut tx, tenant_id, principal, keep).await?
        {
            return Ok(GrantDeletion::LastHolder);
        }
        sqlx::query("DELETE FROM grants WHERE tenant = $1 AND principal = $2")
            .bind(tenant(tenant_id))
            .bind(principal.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(GrantDeletion::Deleted)
    }

    async fn claim_grant_once(
        &self,
        tenant_id: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, Fail> {
        let mut tx = self.pool.begin().await?;
        lock_tenant(&mut tx, tenant_id).await?;
        let pending = sqlx::query(&format!(
            "DELETE FROM grants WHERE tenant = $1 AND principal = $2 RETURNING {GRANT_COLUMNS}"
        ))
        .bind(tenant(tenant_id))
        .bind(invitation.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| grant_from_row(&row))
        .transpose()?;
        let claim = if let Some(existing) = find_grant(&mut tx, tenant_id, user).await? {
            GrantClaim::AlreadyGranted(existing)
        } else if let Some(pending) = pending {
            let claimed = Grant {
                principal: user.clone(),
                ..pending
            };
            write_grant(&mut tx, &claimed).await?;
            GrantClaim::Claimed(claimed)
        } else {
            GrantClaim::NoGrant
        };
        tx.commit().await?;
        Ok(claim)
    }
}

impl AccessStore for PostgresStore {
    async fn put_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        let inserted = sqlx::query(
            "INSERT INTO api_keys (hash, tenant, name, created_by, created_at, revoked_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (hash) DO NOTHING",
        )
        .bind(bytes(key.hash))
        .bind(tenant(&key.tenant))
        .bind(key.name.as_str())
        .bind(key.created_by.as_ref().map(ToString::to_string))
        .bind(ts(key.created_at))
        .bind(opt_ts(key.revoked_at))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!(
                "an API key with hash {} already exists",
                key.hash.short()
            )));
        }
        Ok(())
    }

    async fn find_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM api_keys WHERE hash = $1"
        ))
        .bind(bytes(hash))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| key_from_row(&row)).transpose()?)
    }

    async fn list_keys(&self, tenant_id: &TenantId) -> Result<Vec<ApiKey>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {KEY_COLUMNS} FROM api_keys WHERE tenant = $1 ORDER BY created_at, hash"
        ))
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(key_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn revoke_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        let revoked = sqlx::query(
            "UPDATE api_keys SET revoked_at = $2 WHERE hash = $1 AND revoked_at IS NULL",
        )
        .bind(bytes(hash))
        .bind(ts(now))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(revoked.rows_affected() > 0)
    }

    // End users' tokens and token keys: not kept by this store (see `namespaces_not_kept`).

    async fn put_token(&self, _token: &Token) -> Result<(), StoreError> {
        Err(namespaces_not_kept("put_token"))
    }

    async fn find_token(&self, _hash: Digest) -> Result<Option<Token>, StoreError> {
        Err(namespaces_not_kept("find_token"))
    }

    async fn revoke_token(
        &self,
        _tenant: &TenantId,
        _hash: Digest,
        _now: Timestamp,
    ) -> Result<bool, StoreError> {
        Err(namespaces_not_kept("revoke_token"))
    }

    async fn revoke_tokens(
        &self,
        _tenant: &TenantId,
        _namespace: &NamespaceName,
        _now: Timestamp,
    ) -> Result<u64, StoreError> {
        Err(namespaces_not_kept("revoke_tokens"))
    }

    async fn count_tokens(
        &self,
        _tenant: &TenantId,
        _namespace: &NamespaceName,
        _now: Timestamp,
    ) -> Result<u64, StoreError> {
        Err(namespaces_not_kept("count_tokens"))
    }

    async fn put_token_key(&self, _key: &TokenKey) -> Result<(), StoreError> {
        Err(namespaces_not_kept("put_token_key"))
    }

    async fn find_token_key(&self, _kid: &str) -> Result<Option<TokenKey>, StoreError> {
        Err(namespaces_not_kept("find_token_key"))
    }

    async fn list_token_keys(&self, _tenant: &TenantId) -> Result<Vec<TokenKey>, StoreError> {
        Err(namespaces_not_kept("list_token_keys"))
    }

    async fn delete_token_key(&self, _tenant: &TenantId, _kid: &str) -> Result<bool, StoreError> {
        Err(namespaces_not_kept("delete_token_key"))
    }

    async fn put_role(&self, role: &Role) -> Result<(), StoreError> {
        let permissions: Vec<&str> = role.permissions.iter().map(String::as_str).collect();
        sqlx::query(
            "INSERT INTO roles (tenant, name, description, permissions, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (tenant, name) DO UPDATE SET description = EXCLUDED.description, \
                 permissions = EXCLUDED.permissions, updated_at = EXCLUDED.updated_at",
        )
        .bind(tenant(&role.tenant))
        .bind(role.name.as_str())
        .bind(role.description.as_str())
        .bind(permissions)
        .bind(ts(role.created_at))
        .bind(ts(role.updated_at))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn get_role(&self, tenant_id: &TenantId, name: &str) -> Result<Option<Role>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {ROLE_COLUMNS} FROM roles WHERE tenant = $1 AND name = $2"
        ))
        .bind(tenant(tenant_id))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| role_from_row(&row)).transpose()?)
    }

    async fn list_roles(&self, tenant_id: &TenantId) -> Result<Vec<Role>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {ROLE_COLUMNS} FROM roles WHERE tenant = $1 ORDER BY name"
        ))
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
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
        Ok(retrying(|| self.delete_role_once(tenant_id, name)).await?)
    }

    async fn put_grant(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, StoreError> {
        Ok(retrying(|| self.put_grant_once(grant, keep)).await?)
    }

    async fn grant(
        &self,
        tenant_id: &TenantId,
        principal: &Principal,
    ) -> Result<Option<Grant>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(find_grant(&mut conn, tenant_id, principal).await?)
    }

    async fn list_grants(&self, tenant_id: &TenantId) -> Result<Vec<Grant>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM grants WHERE tenant = $1 ORDER BY principal"
        ))
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
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
        Ok(retrying(|| self.delete_grant_once(tenant_id, principal, keep)).await?)
    }

    async fn claim_grant(
        &self,
        tenant_id: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, StoreError> {
        Ok(retrying(|| self.claim_grant_once(tenant_id, invitation, user)).await?)
    }

    async fn set_default_role(&self, tenant_id: &TenantId, role: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO access_settings (tenant, default_role) VALUES ($1, $2) \
             ON CONFLICT (tenant) DO UPDATE SET default_role = EXCLUDED.default_role",
        )
        .bind(tenant(tenant_id))
        .bind(role)
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn default_role(&self, tenant_id: &TenantId) -> Result<Option<String>, StoreError> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT default_role FROM access_settings WHERE tenant = $1")
                .bind(tenant(tenant_id))
                .fetch_optional(&self.pool)
                .await
                .map_err(Error::from)?;
        Ok(role)
    }
}
