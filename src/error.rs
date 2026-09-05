//! RFC 9457-style problem responses with stable Stabbur error codes.

use actix_web::{HttpResponse, ResponseError, http::StatusCode};
use serde::Serialize;
use stabbur_domain::DomainError;
use stabbur_storage_core::StorageError;
use stabbur_store_core::StoreError;
use thiserror::Error;
use utoipa::ToSchema;

/// One field-level validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ValidationError {
    /// Request field or parameter.
    pub field: String,
    /// Stable validation code.
    pub code: String,
    /// Human-readable diagnostic.
    pub message: String,
}

/// Stable `application/problem+json` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Problem {
    /// Stable machine-readable Stabbur code.
    pub code: String,
    /// HTTP status.
    pub status: u16,
    /// Human-readable detail safe for untrusted clients.
    pub detail: String,
    /// Field-level validation failures.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<ValidationError>,
    /// Request correlation identity.
    pub request_id: String,
}

/// Application error carrying a complete safe problem response.
#[derive(Debug, Error)]
#[error("{problem_code}: {detail}", problem_code = .problem.code, detail = .problem.detail)]
pub struct ApiError {
    status: StatusCode,
    problem: Problem,
}

impl ApiError {
    /// Creates an error with no field-level validation failures.
    #[must_use]
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        detail: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            status,
            problem: Problem {
                code: code.into(),
                status: status.as_u16(),
                detail: detail.into(),
                validation_errors: vec![],
                request_id: request_id.into(),
            },
        }
    }

    /// Creates a validation response.
    #[must_use]
    pub fn validation(
        detail: impl Into<String>,
        errors: Vec<ValidationError>,
        request_id: impl Into<String>,
    ) -> Self {
        let mut result = Self::new(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            detail,
            request_id,
        );
        result.problem.validation_errors = errors;
        result
    }

    /// Maps a storage error while withholding backend details from clients.
    #[must_use]
    pub fn storage(error: StorageError, request_id: &str) -> Self {
        match error {
            StorageError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "The requested resource does not exist.",
                request_id,
            ),
            StorageError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "conflict",
                "The request conflicts with an existing resource.",
                request_id,
            ),
            StorageError::StaleRevision => Self::new(
                StatusCode::PRECONDITION_FAILED,
                "stale_revision",
                "The resource changed since it was read.",
                request_id,
            ),
            StorageError::BootstrapUnavailable => Self::new(
                StatusCode::GONE,
                "bootstrap_unavailable",
                "One-time bootstrap is unavailable.",
                request_id,
            ),
            StorageError::InvalidCredentials => Self::unauthorized(request_id),
            StorageError::InvalidLease => Self::new(
                StatusCode::CONFLICT,
                "invalid_lease",
                "The job lease is no longer active.",
                request_id,
            ),
            StorageError::InvalidData { message } => {
                Self::new(StatusCode::BAD_REQUEST, "invalid_data", message, request_id)
            }
            StorageError::Backend { .. } => Self::internal(request_id),
        }
    }

    /// Maps an artifact-store error.
    #[must_use]
    pub fn store(error: StoreError, request_id: &str) -> Self {
        match error {
            StoreError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "artifact_not_found",
                "Artifact content is not present.",
                request_id,
            ),
            StoreError::RangeNotSatisfiable { .. } => Self::new(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                "The requested single byte range is not satisfiable.",
                request_id,
            ),
            StoreError::UnsupportedCapability { .. } => Self::new(
                StatusCode::NOT_IMPLEMENTED,
                "store_capability_unsupported",
                "The selected store does not support this operation.",
                request_id,
            ),
            StoreError::DigestMismatch { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "digest_mismatch",
                "Uploaded content does not match the declared SHA-256 digest.",
                request_id,
            ),
            StoreError::SizeMismatch { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "size_mismatch",
                "Uploaded content does not match the declared size.",
                request_id,
            ),
            StoreError::Backend { .. } => Self::internal(request_id),
        }
    }

    /// Maps a domain error.
    #[must_use]
    pub fn domain(error: DomainError, request_id: &str) -> Self {
        match error {
            DomainError::NoCompatibleVariant => Self::new(
                StatusCode::NOT_FOUND,
                "no_compatible_variant",
                error.to_string(),
                request_id,
            ),
            DomainError::AmbiguousVariant => Self::new(
                StatusCode::CONFLICT,
                "ambiguous_variant",
                error.to_string(),
                request_id,
            ),
            _ => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_domain_value",
                error.to_string(),
                request_id,
            ),
        }
    }

    /// Authentication is missing or invalid.
    #[must_use]
    pub fn unauthorized(request_id: &str) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Valid authentication is required.",
            request_id,
        )
    }

    /// Authentication succeeded but permission is absent.
    #[must_use]
    pub fn forbidden(request_id: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The authenticated principal is not permitted to perform this operation.",
            request_id,
        )
    }

    /// An unexpected internal failure.
    #[must_use]
    pub fn internal(request_id: &str) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The server could not complete the request.",
            request_id,
        )
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status)
            .content_type("application/problem+json")
            .json(&self.problem)
    }
}
