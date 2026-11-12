use package_manager_core::SourceError;

/// Messages are written for end users.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("No network connection. Showing cached results.")]
    Offline,
    #[error("{0} was not found on GitHub, or it has no releases yet.")]
    NotFound(String),
    #[error("GitHub rate limit reached. Try again in {retry_after_secs} seconds.")]
    RateLimited { retry_after_secs: u64 },
    #[error("GitHub returned an error (HTTP {0}).")]
    Http(u16),
    #[error("GitHub sent a response this app could not understand: {0}")]
    InvalidResponse(String),
    #[error("Refusing to use a download that is not a GitHub HTTPS address: {0}")]
    UnsafeUrl(String),
    #[error("{repo} {tag} has no download matching {pattern}.")]
    NoMatchingAsset {
        repo: String,
        tag: String,
        pattern: String,
    },
    #[error("{0}")]
    BadRepoInput(String),
    #[error("{repo} {tag} has no .deb package for this computer. {hint}")]
    NoInstallablePackage {
        repo: String,
        tag: String,
        hint: String,
    },
    #[error("{0}")]
    CannotLink(String),
    #[error("The downloads in {0}'s latest release belong to a different repository (it may have been renamed or moved), so they were not used. Link it again from its new address.")]
    ForeignAssets(String),
    #[error("The download failed: {0}")]
    DownloadFailed(String),
    #[error("The download is {actual} bytes but {expected} were expected. It was discarded.")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("The download does not match the checksum the maintainer published. It may be corrupted or tampered with, so it was deleted and nothing was installed.")]
    ChecksumMismatch,
    #[error("The release publishes a checksum file, but it does not list this download, so it cannot be verified. Nothing was installed.")]
    ChecksumUnverifiable,
    #[error("The download is not a Debian package. It was deleted and nothing was installed.")]
    NotDebianPackage,
    #[error(
        "No checksum provided by maintainer. Confirm in the app to install this update anyway."
    )]
    NeedsConfirmation,
    #[error("Only .deb updates can be installed for now ({0}).")]
    UnsupportedAsset(String),
    #[error("A newer release appeared since the last check. Check for updates again.")]
    ReleaseChanged,
    #[error("Installing GitHub updates is not set up: {0}")]
    NotConfigured(&'static str),
    #[error("The installer reported an error: {0}")]
    InstallFailed(String),
    #[error("The update installed, but the app is still at version {0}.")]
    VerifyFailed(String),
}

impl From<GithubError> for SourceError {
    fn from(e: GithubError) -> Self {
        match e {
            GithubError::Offline => Self::Offline,
            GithubError::RateLimited { retry_after_secs } => Self::RateLimited { retry_after_secs },
            other => Self::Other(other.to_string()),
        }
    }
}
