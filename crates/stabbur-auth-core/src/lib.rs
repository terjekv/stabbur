//! Authentication values, password and token policy, roles, and permission decisions.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{
        PasswordHash as ParsedPasswordHash, PasswordHasher, PasswordVerifier, SaltString,
    },
};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

/// Authentication and authorization errors safe to map at an application boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// A principal identity is invalid.
    #[error("principal ID must be a UUIDv7")]
    InvalidPrincipalId,
    /// A password fails the local policy.
    #[error("password must contain 12-1024 characters")]
    WeakPassword,
    /// A password hash cannot be generated.
    #[error("password hashing failed")]
    PasswordHashing,
    /// Supplied credentials do not match.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// A serialized token digest is invalid.
    #[error("token digest is invalid")]
    InvalidTokenHash,
    /// A role name is invalid.
    #[error("role name must contain lowercase letters, digits, or hyphens")]
    InvalidRoleName,
}

/// Identity of a human, service, or worker principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrincipalId(Uuid);

impl PrincipalId {
    /// Creates a `UUIDv7` principal identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Validates an existing identity.
    pub fn from_uuid(value: Uuid) -> Result<Self, AuthError> {
        if value.get_version_num() == 7 {
            Ok(Self(value))
        } else {
            Err(AuthError::InvalidPrincipalId)
        }
    }

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PrincipalId {
    type Err = AuthError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value)
            .ok()
            .and_then(|uuid| Self::from_uuid(uuid).ok())
            .ok_or(AuthError::InvalidPrincipalId)
    }
}

impl Serialize for PrincipalId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PrincipalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// A validated custom role name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoleName(String);

impl RoleName {
    /// Validates a role name.
    pub fn new(value: impl Into<String>) -> Result<Self, AuthError> {
        let value = value.into();
        let valid = (1..=63).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric);
        if valid {
            Ok(Self(value))
        } else {
            Err(AuthError::InvalidRoleName)
        }
    }

    /// Returns the normalized name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RoleName {
    type Error = AuthError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RoleName> for String {
    fn from(value: RoleName) -> Self {
        value.0
    }
}

impl fmt::Display for RoleName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A permission checked by Stabbur application services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Read software, releases, variants, and channels.
    #[serde(rename = "software:read")]
    SoftwareRead,
    /// Create and edit software metadata.
    #[serde(rename = "software:write")]
    SoftwareWrite,
    /// Read recipe definitions and revisions.
    #[serde(rename = "recipe:read")]
    RecipeRead,
    /// Create immutable recipe revisions.
    #[serde(rename = "recipe:write")]
    RecipeWrite,
    /// Execute a recipe.
    #[serde(rename = "recipe:execute")]
    RecipeExecute,
    /// Promote a release between channels.
    #[serde(rename = "release:promote")]
    ReleasePromote,
    /// Reject a release and unbind channels.
    #[serde(rename = "release:reject")]
    ReleaseReject,
    /// Read artifacts and metadata.
    #[serde(rename = "artifact:read")]
    ArtifactRead,
    /// Upload immutable artifacts.
    #[serde(rename = "artifact:write")]
    ArtifactWrite,
    /// Configure and test stores.
    #[serde(rename = "storage:manage")]
    StorageManage,
    /// Inspect workers and jobs.
    #[serde(rename = "worker:read")]
    WorkerRead,
    /// Manage workers and jobs.
    #[serde(rename = "worker:manage")]
    WorkerManage,
    /// Read append-only audit history.
    #[serde(rename = "audit:read")]
    AuditRead,
    /// Manage principals, roles, and tokens.
    #[serde(rename = "auth:manage")]
    AuthManage,
}

/// One named collection of permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// Stable name.
    pub name: RoleName,
    /// Granted permissions.
    pub permissions: BTreeSet<Permission>,
    /// Whether this built-in role is immutable.
    pub built_in: bool,
}

impl Role {
    /// Returns the five immutable built-in roles.
    #[must_use]
    pub fn built_ins() -> BTreeMap<RoleName, Self> {
        use Permission as P;
        [
            (
                "reader",
                [P::SoftwareRead, P::RecipeRead, P::ArtifactRead].as_slice(),
            ),
            (
                "operator",
                [
                    P::SoftwareRead,
                    P::SoftwareWrite,
                    P::RecipeRead,
                    P::RecipeExecute,
                    P::ArtifactRead,
                    P::WorkerRead,
                ]
                .as_slice(),
            ),
            (
                "publisher",
                [
                    P::SoftwareRead,
                    P::RecipeRead,
                    P::ArtifactRead,
                    P::ReleasePromote,
                    P::ReleaseReject,
                ]
                .as_slice(),
            ),
            (
                "storage-admin",
                [P::SoftwareRead, P::ArtifactRead, P::StorageManage].as_slice(),
            ),
            (
                "admin",
                [
                    P::SoftwareRead,
                    P::SoftwareWrite,
                    P::RecipeRead,
                    P::RecipeWrite,
                    P::RecipeExecute,
                    P::ReleasePromote,
                    P::ReleaseReject,
                    P::ArtifactRead,
                    P::ArtifactWrite,
                    P::StorageManage,
                    P::WorkerRead,
                    P::WorkerManage,
                    P::AuditRead,
                    P::AuthManage,
                ]
                .as_slice(),
            ),
        ]
        .into_iter()
        .map(|(name, permissions)| {
            let name = RoleName::new(name).expect("built-in role names are valid");
            (
                name.clone(),
                Self {
                    name,
                    permissions: permissions.iter().copied().collect(),
                    built_in: true,
                },
            )
        })
        .collect()
    }
}

/// The kind and authentication scope of a principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// An interactive local person.
    Human,
    /// A non-interactive API integration.
    Service,
    /// An outbound-only build worker.
    Worker,
}

/// An authenticated Stabbur principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Domain identity.
    pub id: PrincipalId,
    /// Unique display/login name.
    pub name: String,
    /// Principal kind.
    pub kind: PrincipalKind,
    /// Assigned role names.
    pub roles: BTreeSet<RoleName>,
    /// Whether authentication is currently permitted.
    pub enabled: bool,
}

/// Stateless role-based authorization policy.
#[derive(Debug, Clone, Default)]
pub struct Authorizer;

impl Authorizer {
    /// Returns whether a principal is allowed to perform an operation.
    #[must_use]
    pub fn allows(
        principal: &Principal,
        permission: Permission,
        roles: &BTreeMap<RoleName, Role>,
    ) -> bool {
        principal.enabled
            && principal.roles.iter().any(|role_name| {
                roles
                    .get(role_name)
                    .is_some_and(|role| role.permissions.contains(&permission))
            })
    }
}

/// An encoded Argon2id password hash. Its debug output is always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Parses an encoded hash without exposing the Argon2 implementation type.
    pub fn parse(value: impl Into<String>) -> Result<Self, AuthError> {
        let value = value.into();
        ParsedPasswordHash::new(&value).map_err(|_| AuthError::PasswordHashing)?;
        Ok(Self(value))
    }

    /// Returns the encoded value for persistence only.
    #[must_use]
    pub fn expose_for_persistence(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PasswordHash([REDACTED])")
    }
}

/// Local Argon2id password hashing and verification policy.
#[derive(Debug, Clone)]
pub struct PasswordPolicy {
    argon2: Argon2<'static>,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        // 19 MiB, two iterations, one lane; encoded parameters permit future upgrades.
        let params =
            Params::new(19 * 1024, 2, 1, None).expect("static Argon2 parameters are valid");
        Self {
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        }
    }
}

impl PasswordPolicy {
    /// Hashes a policy-compliant password with a fresh random salt.
    pub fn hash(&self, password: &str) -> Result<PasswordHash, AuthError> {
        if !(12..=1024).contains(&password.chars().count()) {
            return Err(AuthError::WeakPassword);
        }
        let mut salt_bytes = [0_u8; 16];
        rand::rng().fill_bytes(&mut salt_bytes);
        let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| AuthError::PasswordHashing)?;
        self.argon2
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| PasswordHash(hash.to_string()))
            .map_err(|_| AuthError::PasswordHashing)
    }

    /// Verifies a candidate password without revealing the failure mechanism.
    pub fn verify(&self, password: &str, expected: &PasswordHash) -> Result<(), AuthError> {
        let parsed =
            ParsedPasswordHash::new(&expected.0).map_err(|_| AuthError::InvalidCredentials)?;
        self.argon2
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AuthError::InvalidCredentials)
    }
}

/// Raw bearer-token material. Debug and display output are always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// Exposes the token only at the credential transport boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

impl fmt::Display for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// A one-way SHA-256 bearer-token digest suitable for persistence.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hashes raw token material.
    #[must_use]
    pub fn from_secret(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    /// Parses a persisted lowercase hexadecimal digest.
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        let bytes = hex::decode(value).map_err(|_| AuthError::InvalidTokenHash)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| AuthError::InvalidTokenHash)?;
        Ok(Self(bytes))
    }

    /// Returns a lowercase hexadecimal value for persistence.
    #[must_use]
    pub fn expose_for_persistence(&self) -> String {
        hex::encode(self.0)
    }

    /// Compares a candidate using constant-time equality.
    #[must_use]
    pub fn verifies(&self, candidate: &str) -> bool {
        let candidate = Self::from_secret(candidate);
        bool::from(self.0.ct_eq(&candidate.0))
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenHash([REDACTED])")
    }
}

/// Generates cryptographically random bearer tokens and their one-way digests.
#[must_use]
pub fn generate_token() -> (SecretToken, TokenHash) {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let secret = SecretToken(format!("stb_{}", hex::encode(bytes)));
    let hash = TokenHash::from_secret(secret.expose_secret());
    (secret, hash)
}

/// Metadata common to short-lived sessions and named long-lived tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMetadata {
    /// Owning principal.
    pub principal_id: PrincipalId,
    /// Optional operator-supplied token name.
    pub name: Option<String>,
    /// Credential creation time.
    pub created_at: DateTime<Utc>,
    /// Expiration time, absent only for explicitly long-lived service tokens.
    pub expires_at: Option<DateTime<Utc>>,
    /// Revocation time.
    pub revoked_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_roles_have_expected_boundaries() {
        let roles = Role::built_ins();
        let reader = roles.get(&RoleName::new("reader").unwrap()).unwrap();
        assert!(reader.permissions.contains(&Permission::SoftwareRead));
        assert!(!reader.permissions.contains(&Permission::SoftwareWrite));
        let admin = roles.get(&RoleName::new("admin").unwrap()).unwrap();
        assert_eq!(admin.permissions.len(), 14);
    }

    #[test]
    fn authorizer_requires_enabled_principal_and_permission() {
        let roles = Role::built_ins();
        let mut principal = Principal {
            id: PrincipalId::new(),
            name: "alice".into(),
            kind: PrincipalKind::Human,
            roles: [RoleName::new("publisher").unwrap()].into(),
            enabled: true,
        };
        assert!(Authorizer::allows(
            &principal,
            Permission::ReleasePromote,
            &roles
        ));
        principal.enabled = false;
        assert!(!Authorizer::allows(
            &principal,
            Permission::ReleasePromote,
            &roles
        ));
    }

    #[test]
    fn passwords_use_argon2id_and_are_verifiable() {
        let policy = PasswordPolicy::default();
        let hash = policy.hash("correct horse battery staple").unwrap();
        assert!(hash.expose_for_persistence().starts_with("$argon2id$"));
        assert!(policy.verify("correct horse battery staple", &hash).is_ok());
        assert_eq!(
            policy.verify("incorrect password", &hash),
            Err(AuthError::InvalidCredentials)
        );
        assert!(!format!("{hash:?}").contains("argon2"));
    }

    #[test]
    fn bearer_tokens_are_random_hashed_and_redacted() {
        let (secret, hash) = generate_token();
        assert!(secret.expose_secret().starts_with("stb_"));
        assert!(hash.verifies(secret.expose_secret()));
        assert!(!hash.verifies("stb_wrong"));
        assert_eq!(secret.to_string(), "[REDACTED]");
        assert_eq!(hash.expose_for_persistence().len(), 64);
    }
}
