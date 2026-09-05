//! SQLite implementation of the identity ports.
use super::{
    ApiTokenRecord, AuditActor, AuditEvent, AuditEventId, BTreeMap, BTreeSet, BootstrapPreparation,
    BootstrapStorage, CredentialStorage, DateTime, HumanCredential, IdentityAdminStorage,
    PasswordHash, Permission, Principal, PrincipalId, PrincipalKind, PrincipalRecord, Role,
    RoleName, RoleRecord, Row, SqliteStorage, StorageError, TokenHash, Utc, async_trait, backend,
    insert_audit, insert_bootstrap_admin, invalid_data, load_principal, load_principal_record,
    map_write, parse_value, principal_kind_name, query, query_scalar, token_from_row,
    validate_bootstrap_username,
};

#[async_trait]
impl BootstrapStorage for SqliteStorage {
    async fn prepare_bootstrap(
        &self,
        secret_hash: &TokenHash,
    ) -> Result<BootstrapPreparation, StorageError> {
        let mut connection = self.acquire_write().await?;
        let disabled: Option<String> =
            query_scalar("SELECT value FROM settings WHERE key = 'bootstrap_disabled'")
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking bootstrap status", &error))?;
        let principal_count: i64 = query_scalar("SELECT COUNT(*) FROM principals")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("counting principals", &error))?;
        let existing: Option<String> =
            query_scalar("SELECT value FROM settings WHERE key = 'bootstrap_secret_hash'")
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking bootstrap secret", &error))?;
        let result = if disabled.is_some() || principal_count > 0 {
            BootstrapPreparation::Disabled
        } else if existing.is_some() {
            BootstrapPreparation::Pending
        } else {
            query("INSERT INTO settings (key, value) VALUES ('bootstrap_secret_hash', ?)")
                .bind(secret_hash.expose_for_persistence())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
            BootstrapPreparation::Created
        };
        connection
            .commit("committing bootstrap preparation")
            .await?;
        Ok(result)
    }

    async fn bootstrap_admin(
        &self,
        secret: &str,
        username: &str,
        password_hash: &PasswordHash,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError> {
        validate_bootstrap_username(username)?;
        let mut connection = self.acquire_write().await?;
        let persisted_hash: Option<String> =
            query_scalar("SELECT value FROM settings WHERE key = 'bootstrap_secret_hash'")
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("loading bootstrap secret hash", &error))?;
        let hash = persisted_hash
            .as_deref()
            .ok_or(StorageError::BootstrapUnavailable)
            .and_then(|value| {
                TokenHash::parse(value).map_err(|_| invalid_data("invalid bootstrap secret hash"))
            })?;
        let principal_count: i64 = query_scalar("SELECT COUNT(*) FROM principals")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("counting bootstrap principals", &error))?;
        if principal_count != 0 || !hash.verifies(secret) {
            return Err(StorageError::BootstrapUnavailable);
        }

        let principal = insert_bootstrap_admin(
            &mut connection,
            username,
            password_hash,
            AuditActor::Bootstrap,
            now,
        )
        .await?;
        connection
            .commit("committing administrator bootstrap")
            .await?;
        Ok(principal)
    }

    async fn bootstrap_admin_local(
        &self,
        username: &str,
        password_hash: &PasswordHash,
        now: DateTime<Utc>,
    ) -> Result<Principal, StorageError> {
        validate_bootstrap_username(username)?;
        let mut connection = self.acquire_write().await?;
        let disabled: Option<String> =
            query_scalar("SELECT value FROM settings WHERE key = 'bootstrap_disabled'")
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("checking bootstrap status", &error))?;
        let principal_count: i64 = query_scalar("SELECT COUNT(*) FROM principals")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("counting bootstrap principals", &error))?;
        if disabled.is_some() || principal_count != 0 {
            return Err(StorageError::BootstrapUnavailable);
        }

        let principal = insert_bootstrap_admin(
            &mut connection,
            username,
            password_hash,
            AuditActor::LocalBreakGlass,
            now,
        )
        .await?;
        connection
            .commit("committing local administrator bootstrap")
            .await?;
        Ok(principal)
    }
}

#[async_trait]
impl CredentialStorage for SqliteStorage {
    async fn human_credential(
        &self,
        username: &str,
    ) -> Result<Option<HumanCredential>, StorageError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| backend("acquiring SQLite connection", &error))?;
        let row = query(
            "SELECT id, password_hash FROM principals
             WHERE name = ? COLLATE NOCASE AND kind = 'human' AND enabled = 1",
        )
        .bind(username)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading human credential", &error))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id_text: String = row
            .try_get("id")
            .map_err(|error| backend("decoding principal ID", &error))?;
        let id = parse_value(&id_text, "principal ID")?;
        let hash_text: String = row
            .try_get("password_hash")
            .map_err(|error| backend("decoding password hash", &error))?;
        let password_hash = PasswordHash::parse(hash_text)
            .map_err(|_| invalid_data("invalid persisted password hash"))?;
        let principal = load_principal(&mut connection, id)
            .await?
            .ok_or_else(|| invalid_data("credential references a missing principal"))?;
        Ok(Some(HumanCredential {
            principal,
            password_hash,
        }))
    }

    async fn create_credential(
        &self,
        principal_id: PrincipalId,
        name: Option<&str>,
        kind: &str,
        token_hash: &TokenHash,
        expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        if !matches!(kind, "session" | "api" | "worker") {
            return Err(invalid_data("invalid credential kind"));
        }
        let mut connection = self.acquire_write().await?;
        if kind == "session" {
            query(
                "DELETE FROM credentials
                 WHERE kind = 'session' AND (expires_at <= ? OR revoked_at IS NOT NULL)",
            )
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        query(
            "INSERT INTO credentials
             (id, principal_id, name, kind, token_hash, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(principal_id.to_string())
        .bind(name)
        .bind(kind)
        .bind(token_hash.expose_for_persistence())
        .bind(now)
        .bind(expires_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        connection.commit("committing credential creation").await?;
        Ok(())
    }

    async fn principal_by_token(
        &self,
        token_hash: &TokenHash,
        now: DateTime<Utc>,
    ) -> Result<Option<Principal>, StorageError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| backend("acquiring SQLite connection", &error))?;
        let id: Option<String> = query_scalar(
            "SELECT p.id FROM credentials c
             JOIN principals p ON p.id = c.principal_id
             WHERE c.token_hash = ? AND c.revoked_at IS NULL
               AND (c.expires_at IS NULL OR c.expires_at > ?) AND p.enabled = 1",
        )
        .bind(token_hash.expose_for_persistence())
        .bind(now)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("authenticating bearer token", &error))?;
        let Some(id) = id else {
            return Ok(None);
        };
        load_principal(&mut connection, parse_value(&id, "principal ID")?).await
    }

    async fn revoke_credentials(
        &self,
        username: &str,
        actor: AuditActor,
        now: DateTime<Utc>,
    ) -> Result<u64, StorageError> {
        let mut connection = self.acquire_write().await?;
        let id: Option<String> = query_scalar(
            "SELECT id FROM principals WHERE name = ? COLLATE NOCASE AND kind = 'human'",
        )
        .bind(username)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading principal for revocation", &error))?;
        let id = id.ok_or(StorageError::NotFound)?;
        let changed = query(
            "UPDATE credentials SET revoked_at = ?
             WHERE principal_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(&id)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        insert_audit(
            &mut connection,
            &AuditEvent {
                id: AuditEventId::new(),
                actor,
                action: "auth.sessions.revoke".into(),
                resource_kind: "principal".into(),
                resource_id: Some(id),
                details: serde_json::json!({"credentials_revoked": changed}),
                request_id: None,
                occurred_at: now,
            },
        )
        .await?;
        connection
            .commit("committing credential revocation")
            .await?;
        Ok(changed)
    }

    async fn reset_password(
        &self,
        username: &str,
        password_hash: &PasswordHash,
        actor: AuditActor,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let mut connection = self.acquire_write().await?;
        let id: Option<String> = query_scalar(
            "SELECT id FROM principals WHERE name = ? COLLATE NOCASE AND kind = 'human'",
        )
        .bind(username)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading principal for password reset", &error))?;
        let id = id.ok_or(StorageError::NotFound)?;
        query("UPDATE principals SET password_hash = ? WHERE id = ?")
            .bind(password_hash.expose_for_persistence())
            .bind(&id)
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        query(
            "UPDATE credentials SET revoked_at = ? WHERE principal_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(&id)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(
            &mut connection,
            &AuditEvent {
                id: AuditEventId::new(),
                actor,
                action: "auth.password.reset".into(),
                resource_kind: "principal".into(),
                resource_id: Some(id),
                details: serde_json::json!({}),
                request_id: None,
                occurred_at: now,
            },
        )
        .await?;
        connection.commit("committing password reset").await?;
        Ok(())
    }
}

#[async_trait]
impl IdentityAdminStorage for SqliteStorage {
    async fn list_principals(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PrincipalRecord>, StorageError> {
        if !(1..=200).contains(&limit) {
            return Err(invalid_data(
                "principal page limit must be between 1 and 200",
            ));
        }
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| backend("acquiring principal list connection", &error))?;
        let ids: Vec<String> =
            query_scalar("SELECT id FROM principals WHERE id > ? ORDER BY id LIMIT ?")
                .bind(after.unwrap_or(""))
                .bind(i64::from(limit))
                .fetch_all(&mut *connection)
                .await
                .map_err(|error| backend("listing principals", &error))?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            records.push(
                load_principal_record(&mut connection, &id)
                    .await?
                    .ok_or_else(|| invalid_data("listed principal disappeared"))?,
            );
        }
        Ok(records)
    }

    async fn principal_record(
        &self,
        identity: &str,
    ) -> Result<Option<PrincipalRecord>, StorageError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| backend("acquiring principal connection", &error))?;
        load_principal_record(&mut connection, identity).await
    }

    async fn create_principal(
        &self,
        principal: &Principal,
        password_hash: Option<&PasswordHash>,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<PrincipalRecord, StorageError> {
        if principal.name.trim() != principal.name
            || !(1..=128).contains(&principal.name.len())
            || principal.roles.is_empty()
            || !matches!(
                principal.kind,
                PrincipalKind::Human | PrincipalKind::Service
            )
            || (principal.kind == PrincipalKind::Human) != password_hash.is_some()
        {
            return Err(invalid_data("principal creation input is invalid"));
        }
        let mut connection = self.acquire_write().await?;
        for role in &principal.roles {
            let exists: Option<i64> = query_scalar("SELECT 1 FROM roles WHERE name = ?")
                .bind(role.as_str())
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("validating principal role", &error))?;
            if exists.is_none() {
                return Err(invalid_data("principal references an unknown role"));
            }
        }
        query(
            "INSERT INTO principals
             (id, name, kind, password_hash, enabled, created_at, revision)
             VALUES (?, ?, ?, ?, ?, ?, 1)",
        )
        .bind(principal.id.to_string())
        .bind(&principal.name)
        .bind(principal_kind_name(principal.kind))
        .bind(password_hash.map(PasswordHash::expose_for_persistence))
        .bind(i64::from(principal.enabled))
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        for role in &principal.roles {
            query("INSERT INTO principal_roles (principal_id, role_name) VALUES (?, ?)")
                .bind(principal.id.to_string())
                .bind(role.as_str())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
        }
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing principal creation").await?;
        Ok(PrincipalRecord {
            principal: principal.clone(),
            created_at: now,
            revision: 1,
        })
    }

    async fn assign_roles(
        &self,
        principal_id: PrincipalId,
        roles: &[RoleName],
        expected_revision: u64,
        audit: &AuditEvent,
    ) -> Result<PrincipalRecord, StorageError> {
        if roles.is_empty() {
            return Err(invalid_data("a principal requires at least one role"));
        }
        let mut unique = std::collections::BTreeSet::new();
        if roles.iter().any(|role| !unique.insert(role.as_str())) {
            return Err(invalid_data("principal roles must be unique"));
        }
        let mut connection = self.acquire_write().await?;
        let current = load_principal_record(&mut connection, &principal_id.to_string())
            .await?
            .ok_or(StorageError::NotFound)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        for role in roles {
            let exists: Option<i64> = query_scalar("SELECT 1 FROM roles WHERE name = ?")
                .bind(role.as_str())
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| backend("validating assigned role", &error))?;
            if exists.is_none() {
                return Err(invalid_data("assignment references an unknown role"));
            }
        }
        let removes_admin = current
            .principal
            .roles
            .iter()
            .any(|role| role.as_str() == "admin")
            && !roles.iter().any(|role| role.as_str() == "admin");
        if current.principal.enabled && removes_admin {
            let enabled_admins: i64 = query_scalar(
                "SELECT COUNT(*) FROM principals p JOIN principal_roles r ON r.principal_id = p.id
                 WHERE p.enabled = 1 AND r.role_name = 'admin'",
            )
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("counting enabled administrators", &error))?;
            if enabled_admins <= 1 {
                return Err(StorageError::Conflict);
            }
        }
        query("DELETE FROM principal_roles WHERE principal_id = ?")
            .bind(principal_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        for role in roles {
            query("INSERT INTO principal_roles (principal_id, role_name) VALUES (?, ?)")
                .bind(principal_id.to_string())
                .bind(role.as_str())
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
        }
        let changed =
            query("UPDATE principals SET revision = revision + 1 WHERE id = ? AND revision = ?")
                .bind(principal_id.to_string())
                .bind(
                    i64::try_from(expected_revision)
                        .map_err(|_| invalid_data("principal revision exceeds SQLite range"))?,
                )
                .execute(&mut *connection)
                .await
                .map_err(map_write)?
                .rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        insert_audit(&mut connection, audit).await?;
        let record = load_principal_record(&mut connection, &principal_id.to_string())
            .await?
            .ok_or_else(|| invalid_data("updated principal disappeared"))?;
        connection
            .commit("committing principal role assignment")
            .await?;
        Ok(record)
    }

    async fn set_principal_enabled(
        &self,
        principal_id: PrincipalId,
        enabled: bool,
        expected_revision: u64,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<PrincipalRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        let current = load_principal_record(&mut connection, &principal_id.to_string())
            .await?
            .ok_or(StorageError::NotFound)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision);
        }
        if !enabled
            && current.principal.enabled
            && current
                .principal
                .roles
                .iter()
                .any(|role| role.as_str() == "admin")
        {
            let enabled_admins: i64 = query_scalar(
                "SELECT COUNT(*) FROM principals p JOIN principal_roles r ON r.principal_id = p.id
                 WHERE p.enabled = 1 AND r.role_name = 'admin'",
            )
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| backend("counting enabled administrators", &error))?;
            if enabled_admins <= 1 {
                return Err(StorageError::Conflict);
            }
        }
        let changed = query(
            "UPDATE principals SET enabled = ?, revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(i64::from(enabled))
        .bind(principal_id.to_string())
        .bind(
            i64::try_from(expected_revision)
                .map_err(|_| invalid_data("principal revision exceeds SQLite range"))?,
        )
        .execute(&mut *connection)
        .await
        .map_err(map_write)?
        .rows_affected();
        if changed != 1 {
            return Err(StorageError::StaleRevision);
        }
        if !enabled {
            query(
                "UPDATE credentials SET revoked_at = ?
                 WHERE principal_id = ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(principal_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        }
        insert_audit(&mut connection, audit).await?;
        let record = load_principal_record(&mut connection, &principal_id.to_string())
            .await?
            .ok_or_else(|| invalid_data("updated principal disappeared"))?;
        connection
            .commit("committing principal status change")
            .await?;
        Ok(record)
    }

    async fn api_tokens(
        &self,
        principal_id: PrincipalId,
    ) -> Result<Vec<ApiTokenRecord>, StorageError> {
        let rows = query(
            "SELECT id, principal_id, name, created_at, expires_at, revoked_at
             FROM credentials WHERE principal_id = ? AND kind = 'api' ORDER BY id",
        )
        .bind(principal_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing API tokens", &error))?;
        rows.iter().map(token_from_row).collect()
    }

    async fn create_api_token(
        &self,
        token: &ApiTokenRecord,
        token_hash: &TokenHash,
        audit: &AuditEvent,
    ) -> Result<(), StorageError> {
        if token.name.trim() != token.name
            || !(1..=128).contains(&token.name.len())
            || token.revoked_at.is_some()
            || uuid::Uuid::parse_str(&token.id)
                .ok()
                .is_none_or(|id| id.get_version_num() != 7)
        {
            return Err(invalid_data("API token metadata is invalid"));
        }
        let mut connection = self.acquire_write().await?;
        let enabled: Option<i64> = query_scalar("SELECT enabled FROM principals WHERE id = ?")
            .bind(token.principal_id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| backend("validating API token principal", &error))?;
        if enabled != Some(1) {
            return Err(StorageError::Conflict);
        }
        query(
            "INSERT INTO credentials
             (id, principal_id, name, kind, token_hash, created_at, expires_at, revoked_at)
             VALUES (?, ?, ?, 'api', ?, ?, ?, NULL)",
        )
        .bind(&token.id)
        .bind(token.principal_id.to_string())
        .bind(&token.name)
        .bind(token_hash.expose_for_persistence())
        .bind(token.created_at)
        .bind(token.expires_at)
        .execute(&mut *connection)
        .await
        .map_err(map_write)?;
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing API token creation").await?;
        Ok(())
    }

    async fn revoke_api_token(
        &self,
        token_id: &str,
        audit: &AuditEvent,
        now: DateTime<Utc>,
    ) -> Result<ApiTokenRecord, StorageError> {
        let mut connection = self.acquire_write().await?;
        let row = query(
            "SELECT id, principal_id, name, created_at, expires_at, revoked_at
             FROM credentials WHERE id = ? AND kind = 'api'",
        )
        .bind(token_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| backend("loading revoked API token", &error))?
        .ok_or(StorageError::NotFound)?;
        let mut token = token_from_row(&row)?;
        if token.revoked_at.is_none() {
            query("UPDATE credentials SET revoked_at = ? WHERE id = ?")
                .bind(now)
                .bind(token_id)
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
            insert_audit(&mut connection, audit).await?;
            token.revoked_at = Some(now);
        }
        connection.commit("committing API token revocation").await?;
        Ok(token)
    }

    async fn roles(&self) -> Result<Vec<RoleRecord>, StorageError> {
        let rows = query(
            "SELECT r.name, r.built_in, r.revision, rp.permission
             FROM roles r
             LEFT JOIN role_permissions rp ON rp.role_name = r.name
             ORDER BY r.name, rp.permission",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| backend("listing roles", &error))?;
        let mut grouped = BTreeMap::<RoleName, (bool, u64, BTreeSet<Permission>)>::new();
        for row in rows {
            let name_text: String = row
                .try_get("name")
                .map_err(|error| backend("decoding role name", &error))?;
            let built_in: i64 = row
                .try_get("built_in")
                .map_err(|error| backend("decoding built-in role marker", &error))?;
            let revision: i64 = row
                .try_get("revision")
                .map_err(|error| backend("decoding role revision", &error))?;
            let name = RoleName::new(name_text)
                .map_err(|_| invalid_data("invalid persisted role name"))?;
            let revision =
                u64::try_from(revision).map_err(|_| invalid_data("negative role revision"))?;
            let entry = grouped
                .entry(name)
                .or_insert_with(|| (built_in == 1, revision, BTreeSet::new()));
            if entry.0 != (built_in == 1) || entry.1 != revision {
                return Err(invalid_data("inconsistent persisted role metadata"));
            }
            if let Some(permission) = row
                .try_get::<Option<String>, _>("permission")
                .map_err(|error| backend("decoding role permission", &error))?
            {
                entry.2.insert(
                    serde_json::from_value(serde_json::Value::String(permission)).map_err(
                        |error| invalid_data(format!("invalid role permission: {error}")),
                    )?,
                );
            }
        }
        Ok(grouped
            .into_iter()
            .map(|(name, (built_in, revision, permissions))| RoleRecord {
                role: Role {
                    name,
                    permissions,
                    built_in,
                },
                revision,
            })
            .collect())
    }

    async fn create_role(
        &self,
        name: &RoleName,
        permissions: &[Permission],
        audit: &AuditEvent,
    ) -> Result<RoleRecord, StorageError> {
        let permissions = permissions
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if permissions.is_empty() || permissions.len() > 64 {
            return Err(invalid_data("custom role requires 1-64 unique permissions"));
        }
        let mut connection = self.acquire_write().await?;
        query("INSERT INTO roles (name, built_in, revision) VALUES (?, 0, 1)")
            .bind(name.as_str())
            .execute(&mut *connection)
            .await
            .map_err(map_write)?;
        for permission in &permissions {
            let value = serde_json::to_value(permission)
                .map_err(|error| invalid_data(format!("serializing permission: {error}")))?;
            let permission = value
                .as_str()
                .ok_or_else(|| invalid_data("permission did not serialize as text"))?;
            query("INSERT INTO role_permissions (role_name, permission) VALUES (?, ?)")
                .bind(name.as_str())
                .bind(permission)
                .execute(&mut *connection)
                .await
                .map_err(map_write)?;
        }
        insert_audit(&mut connection, audit).await?;
        connection.commit("committing custom role creation").await?;
        Ok(RoleRecord {
            role: Role {
                name: name.clone(),
                permissions,
                built_in: false,
            },
            revision: 1,
        })
    }
}
