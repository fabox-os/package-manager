use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::domain::{InstalledApp, TrackedApp, UpdateCandidate};

/// One enum per source crate wraps into this at the trait boundary.
/// Messages are written for end users.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0} is not installed on this system.")]
    Unavailable(String),
    #[error("APT is locked by another process. Try again in a moment.")]
    Locked,
    #[error("Update cancelled: administrator authentication was not completed.")]
    Cancelled,
    #[error("No network connection. Showing cached results.")]
    Offline,
    #[error("Rate limited. Try again in {retry_after_secs} seconds.")]
    RateLimited { retry_after_secs: u64 },
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Started { app_id: String },
    Percent { app_id: String, percent: u8 },
    Finished { app_id: String },
    Failed { app_id: String, reason: String },
}

pub type ProgressSender = mpsc::UnboundedSender<ProgressEvent>;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Where linked GitHub apps are persisted. Implemented outside `core` (SQLite).
#[async_trait]
pub trait TrackedStore: Send + Sync {
    async fn list(&self) -> Result<Vec<TrackedApp>, SourceError>;

    /// Links (or re-links) a package to a repository.
    async fn save(&self, app: &TrackedApp) -> Result<(), SourceError>;
}

#[async_trait]
pub trait UpdateSource: Send + Sync {
    /// Short stable name: `"apt"`, `"flatpak"`, `"github"`.
    fn kind(&self) -> &'static str;
    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError>;
    async fn check_updates(
        &self,
        apps: &[InstalledApp],
    ) -> Result<Vec<UpdateCandidate>, SourceError>;
    async fn apply(
        &self,
        candidates: &[UpdateCandidate],
        progress: ProgressSender,
    ) -> Result<ApplyReport, SourceError>;
}
