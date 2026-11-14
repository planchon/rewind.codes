//! `AccessStore` on Turso: API keys, the tenants' own roles, grants and default roles. The
//! in-memory `MemoryAccess` of `rewind-engine` is the reference of every behavior here; both pass
//! the same conformance suite.
//!
//! Every call holds the one connection for its whole duration, and the calls that read before
//! they write (the kept role's last holder, a role in use, a claim) are one transaction: the
//! check and the write are atomic.

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, Principal, Role,
    RoleDeletion, StoreError, TenantId,
};
use turso::{Connection, Row};

use crate::codec::{
    decode_labels, decode_permissions, encode_labels, encode_permissions, parse_principal,
};
use crate::sql::{self, Columns, digest, opt_text, opt_ts, tenant, text, transaction, ts};
use crate::{Error, TursoStore};

/// The columns of an [`ApiKey`], read by [`key_from_row`].
const KEY_COLUMNS: &str = "hash, tenant, name, created_by, created_at, revoked_at";

/// The columns of a [`Role`], read by [`role_from_row`].
const ROLE_COLUMNS: &str = "tenant, name, description, permissions, created_at, updated_at";

/// The columns of a [`Grant`], read by [`grant_from_row`].
const GRANT_COLUMNS: &str = "tenant, principal, role, allowed_labels, granted_by, granted_at";

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
            Ok(())
        })
        .await
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
        let revoked = sql::execute(
            &connection,
            "UPDATE api_keys SET revoked_at = ? WHERE hash = ? AND revoked_at IS NULL",
            vec![ts(now), digest(hash)],
        )
        .await?;
        Ok(revoked > 0)
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
        transaction(connection, async {
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
        .await
    }

    async fn put_grant(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            if let Some(keep) = keep
                && grant.role != keep
                && holds_last(connection, &grant.tenant, &grant.principal, keep).await?
            {
                return Ok(GrantWrite::LastHolder);
            }
            write_grant(connection, grant).await?;
            Ok::<_, StoreError>(GrantWrite::Written)
        })
        .await
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
        transaction(connection, async {
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
        .await
    }

    async fn claim_grant(
        &self,
        tenant_id: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let pending = find_grant(connection, tenant_id, invitation).await?;
            if pending.is_some() {
                remove_grant(connection, tenant_id, invitation).await?;
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
            Ok::<_, StoreError>(GrantClaim::Claimed(claimed))
        })
        .await
    }

    async fn set_default_role(&self, tenant_id: &TenantId, role: &str) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO access_settings (tenant, default_role) VALUES (?, ?)",
            vec![tenant(tenant_id), text(role)],
        )
        .await?;
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
}
