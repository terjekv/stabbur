//! Builder-neutral worker capabilities, jobs, attempts, and lease policy.

use std::{collections::BTreeSet, fmt};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use stabbur_domain::{AttemptId, JobId, RecipeCatalogScanId, RunId, WorkerId};
use thiserror::Error;

/// A normalized worker capability such as `builder.autopkg` or `os.macos`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Capability(String);

impl Capability {
    /// Validates a capability name.
    pub fn new(value: impl Into<String>) -> Result<Self, JobError> {
        let value = value.into();
        let valid = (1..=127).contains(&value.len())
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-' | b'_')
            })
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
            Err(JobError::InvalidCapability)
        }
    }

    /// Returns the normalized capability.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Capability {
    type Error = JobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Capability> for String {
    fn from(value: Capability) -> Self {
        value.0
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A set of capabilities advertised by one worker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// Creates a set from validated values.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = Capability>) -> Self {
        Self(values.into_iter().collect())
    }

    /// Returns whether all required capabilities are present.
    #[must_use]
    pub fn satisfies(&self, required: &Self) -> bool {
        required.0.is_subset(&self.0)
    }

    /// Iterates through sorted capabilities.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }
}

/// Durable job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Ready for a compatible worker.
    Queued,
    /// Held by an unexpired attempt lease.
    Leased,
    /// Completed transactionally and idempotently.
    Succeeded,
    /// Exhausted or explicitly failed.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

/// Durable builder-neutral job data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSubject {
    /// A build run whose terminal state follows the job.
    BuildRun {
        /// Durable build run identity.
        run_id: RunId,
    },
    /// A recipe catalog scan whose terminal state follows the job.
    RecipeCatalogScan {
        /// Durable catalog scan identity.
        scan_id: RecipeCatalogScanId,
    },
}

impl JobSubject {
    /// Returns the build run identity when this is a build job.
    #[must_use]
    pub const fn build_run_id(self) -> Option<RunId> {
        match self {
            Self::BuildRun { run_id } => Some(run_id),
            Self::RecipeCatalogScan { .. } => None,
        }
    }

    /// Returns the catalog scan identity when this is a catalog job.
    #[must_use]
    pub const fn recipe_catalog_scan_id(self) -> Option<RecipeCatalogScanId> {
        match self {
            Self::BuildRun { .. } => None,
            Self::RecipeCatalogScan { scan_id } => Some(scan_id),
        }
    }
}

/// Durable builder-neutral job data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// Job identity.
    pub id: JobId,
    /// Domain work item whose terminal result this job controls.
    pub subject: JobSubject,
    /// Required worker capabilities.
    pub required_capabilities: CapabilitySet,
    /// Builder-neutral serialized request.
    pub payload: serde_json::Value,
    /// Current durable state.
    pub state: JobState,
    /// Maximum execution attempts.
    pub maximum_attempts: u32,
    /// Number of attempts already created.
    pub attempt_count: u32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// One exclusive, expiring attempt to execute a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Attempt identity used for heartbeats and result submission.
    pub attempt_id: AttemptId,
    /// Leased job.
    pub job_id: JobId,
    /// Owning worker.
    pub worker_id: WorkerId,
    /// Last accepted heartbeat.
    pub heartbeat_at: DateTime<Utc>,
    /// Exclusive lease deadline.
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    /// Returns whether the lease has expired at a database-supplied time.
    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }

    /// Extends an active lease from a database-supplied time.
    pub fn heartbeat(
        &mut self,
        worker: WorkerId,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<(), JobError> {
        if worker != self.worker_id {
            return Err(JobError::LeaseOwnerMismatch);
        }
        if self.is_expired_at(now) {
            return Err(JobError::LeaseExpired);
        }
        if duration <= Duration::zero() {
            return Err(JobError::InvalidLeaseDuration);
        }
        self.heartbeat_at = now;
        self.expires_at = now + duration;
        Ok(())
    }
}

/// Job and lease policy failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobError {
    /// A capability name is malformed.
    #[error("capability name is invalid")]
    InvalidCapability,
    /// A worker cannot execute this job.
    #[error("worker does not satisfy the required capabilities")]
    IncompatibleWorker,
    /// The job is not claimable.
    #[error("job is not claimable")]
    NotClaimable,
    /// The lease belongs to another worker.
    #[error("lease belongs to another worker")]
    LeaseOwnerMismatch,
    /// The lease deadline has passed.
    #[error("lease has expired")]
    LeaseExpired,
    /// A lease duration must be positive.
    #[error("lease duration must be positive")]
    InvalidLeaseDuration,
    /// No additional attempts are allowed.
    #[error("job has exhausted its attempts")]
    AttemptsExhausted,
}

/// Claims a compatible queued job and returns its new lease.
pub fn claim(
    job: &mut Job,
    worker_id: WorkerId,
    capabilities: &CapabilitySet,
    now: DateTime<Utc>,
    duration: Duration,
) -> Result<Lease, JobError> {
    if job.state != JobState::Queued {
        return Err(JobError::NotClaimable);
    }
    if !capabilities.satisfies(&job.required_capabilities) {
        return Err(JobError::IncompatibleWorker);
    }
    if job.attempt_count >= job.maximum_attempts {
        return Err(JobError::AttemptsExhausted);
    }
    if duration <= Duration::zero() {
        return Err(JobError::InvalidLeaseDuration);
    }
    job.attempt_count += 1;
    job.state = JobState::Leased;
    Ok(Lease {
        attempt_id: AttemptId::new(),
        job_id: job.id,
        worker_id,
        heartbeat_at: now,
        expires_at: now + duration,
    })
}

/// Makes a job claimable after an expired lease, or permanently fails it.
pub fn recover_expired(job: &mut Job, lease: &Lease, now: DateTime<Utc>) -> Result<(), JobError> {
    if job.id != lease.job_id || job.state != JobState::Leased || !lease.is_expired_at(now) {
        return Err(JobError::NotClaimable);
    }
    job.state = if job.attempt_count >= job.maximum_attempts {
        JobState::Failed
    } else {
        JobState::Queued
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            id: JobId::new(),
            subject: JobSubject::BuildRun {
                run_id: RunId::new(),
            },
            required_capabilities: CapabilitySet::new([
                Capability::new("builder.autopkg").unwrap(),
                Capability::new("os.macos").unwrap(),
            ]),
            payload: serde_json::json!({}),
            state: JobState::Queued,
            maximum_attempts: 2,
            attempt_count: 0,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn only_compatible_workers_claim_jobs() {
        let mut job = job();
        let linux = CapabilitySet::new([Capability::new("os.linux").unwrap()]);
        assert_eq!(
            claim(
                &mut job,
                WorkerId::new(),
                &linux,
                Utc::now(),
                Duration::seconds(30)
            ),
            Err(JobError::IncompatibleWorker)
        );
        let mac = CapabilitySet::new([
            Capability::new("os.macos").unwrap(),
            Capability::new("builder.autopkg").unwrap(),
        ]);
        assert!(
            claim(
                &mut job,
                WorkerId::new(),
                &mac,
                Utc::now(),
                Duration::seconds(30)
            )
            .is_ok()
        );
        assert_eq!(job.state, JobState::Leased);
    }

    #[test]
    fn expired_jobs_are_requeued_until_attempts_exhausted() {
        let now = Utc::now();
        let capabilities = job().required_capabilities;
        let mut job = job();
        let lease = claim(
            &mut job,
            WorkerId::new(),
            &capabilities,
            now,
            Duration::seconds(1),
        )
        .unwrap();
        recover_expired(&mut job, &lease, now + Duration::seconds(2)).unwrap();
        assert_eq!(job.state, JobState::Queued);
        let lease = claim(
            &mut job,
            WorkerId::new(),
            &capabilities,
            now + Duration::seconds(2),
            Duration::seconds(1),
        )
        .unwrap();
        recover_expired(&mut job, &lease, now + Duration::seconds(4)).unwrap();
        assert_eq!(job.state, JobState::Failed);
    }

    #[test]
    fn heartbeats_require_owner_and_active_lease() {
        let now = Utc::now();
        let mut job = job();
        let worker = WorkerId::new();
        let capabilities = job.required_capabilities.clone();
        let mut lease = claim(&mut job, worker, &capabilities, now, Duration::seconds(10)).unwrap();
        assert_eq!(
            lease.heartbeat(WorkerId::new(), now, Duration::seconds(10)),
            Err(JobError::LeaseOwnerMismatch)
        );
        assert!(
            lease
                .heartbeat(worker, now + Duration::seconds(2), Duration::seconds(10))
                .is_ok()
        );
    }
}
