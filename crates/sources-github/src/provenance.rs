use package_manager_core::{Provenance, ProvenanceKind, SignatureStatus, TrustLevel};

use crate::assets::ChecksumKind;

/// What is known about a release's checksum when provenance is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumState {
    /// The release could not be inspected (offline, rate limited).
    Unknown,
    /// The maintainer publishes a checksum; it is verified before every install.
    Published(ChecksumKind),
    /// No checksum file: the user is warned and must confirm.
    Missing,
}

/// Trust rules from the design doc:
/// - shipped preset + published checksum -> Verified
/// - repo the user typed in (or a fork) -> Community
/// - no published checksum -> signature Missing (always shown with a warning)
pub fn github_provenance(
    owner: &str,
    repo: &str,
    release_url: &str,
    is_preset: bool,
    checksum: ChecksumState,
) -> Provenance {
    let trust = match (is_preset, checksum) {
        (true, ChecksumState::Published(_)) => TrustLevel::Verified,
        _ => TrustLevel::Community,
    };
    let signature = match checksum {
        ChecksumState::Missing => SignatureStatus::Missing,
        _ => SignatureStatus::NotChecked,
    };
    Provenance {
        kind: ProvenanceKind::Github {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            release_url: release_url.to_owned(),
        },
        display_name: format!("github.com/{owner}/{repo}"),
        detail_url: Some(release_url.to_owned()),
        trust,
        verified_at: None,
        signature,
        also_available_from: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/o/r/releases/tag/v1";

    fn p(preset: bool, c: ChecksumState) -> Provenance {
        github_provenance("o", "r", URL, preset, c)
    }

    #[test]
    fn preset_with_published_checksum_is_verified() {
        let v = p(true, ChecksumState::Published(ChecksumKind::ElectronYml));
        assert_eq!(v.trust, TrustLevel::Verified);
        assert_eq!(v.display_name, "github.com/o/r");
        assert_eq!(v.detail_url.as_deref(), Some(URL));
    }

    #[test]
    fn user_linked_repo_is_community_even_with_a_checksum() {
        assert_eq!(
            p(false, ChecksumState::Published(ChecksumKind::SumsFile)).trust,
            TrustLevel::Community
        );
    }

    #[test]
    fn missing_checksum_is_flagged_and_never_verified() {
        for preset in [true, false] {
            let m = p(preset, ChecksumState::Missing);
            assert_eq!(m.trust, TrustLevel::Community);
            assert_eq!(m.signature, SignatureStatus::Missing);
        }
    }

    #[test]
    fn unknown_state_does_not_claim_verification() {
        let u = p(true, ChecksumState::Unknown);
        assert_eq!(u.trust, TrustLevel::Community);
        assert_eq!(u.signature, SignatureStatus::NotChecked);
    }
}
