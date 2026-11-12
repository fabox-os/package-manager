use serde::{Deserialize, Serialize};

use crate::error::GithubError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// The tag without a leading `v`: `v4.4.0` -> `4.4.0`.
    pub fn version(&self) -> &str {
        let t = self.tag_name.trim();
        t.strip_prefix(['v', 'V']).unwrap_or(t)
    }
}

/// Largest API response accepted; a release list with hundreds of assets is still far smaller.
pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

pub fn parse_release(bytes: &[u8]) -> Result<Release, GithubError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(GithubError::InvalidResponse(
            "the response is too large".to_owned(),
        ));
    }
    let release: Release =
        serde_json::from_slice(bytes).map_err(|e| GithubError::InvalidResponse(e.to_string()))?;
    if release.tag_name.trim().is_empty() {
        return Err(GithubError::InvalidResponse(
            "the release has no tag".to_owned(),
        ));
    }
    Ok(release)
}

/// `https://github.com/...` exactly: no other scheme, no other host, no credentials, no port.
/// Release pages and asset downloads must be on github.com (downloads then redirect to
/// GitHub's CDN), which is also the domain the app's provenance names.
pub fn is_github_https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host == "github.com" && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYDRA: &[u8] = include_bytes!("../tests/fixtures/hydra-release.json");

    #[test]
    fn parses_the_real_hydra_release() {
        let r = parse_release(HYDRA).expect("parse");
        assert_eq!(r.tag_name, "v4.4.0");
        assert_eq!(r.version(), "4.4.0");
        assert!(!r.draft && !r.prerelease);
        assert_eq!(r.assets.len(), 10);
        let deb = r
            .assets
            .iter()
            .find(|a| a.name.ends_with(".deb"))
            .expect("deb");
        assert_eq!(deb.name, "hydralauncher_4.4.0_amd64.deb");
        assert_eq!(deb.size, 178_442_846);
        assert!(is_github_https_url(&deb.browser_download_url));
        assert!(is_github_https_url(&r.html_url));
    }

    #[test]
    fn rejects_bad_json_and_empty_tags() {
        assert!(parse_release(b"not json").is_err());
        assert!(parse_release(b"{}").is_err());
        assert!(parse_release(br#"{"tag_name":" ","html_url":"https://github.com/a/b"}"#).is_err());
        assert!(parse_release(&vec![b' '; MAX_RESPONSE_BYTES + 1]).is_err());
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let r = parse_release(br#"{"tag_name":"V1.0","html_url":"https://github.com/a/b"}"#)
            .expect("parse");
        assert!(r.assets.is_empty() && r.body.is_none());
        assert_eq!(r.version(), "1.0");
    }

    #[test]
    fn only_plain_github_https_urls_are_accepted() {
        assert!(is_github_https_url(
            "https://github.com/o/r/releases/download/v1/a.deb"
        ));
        for bad in [
            "http://github.com/o/r",
            "https://github.com.evil.test/o/r",
            "https://evil.test/github.com/o/r",
            "https://user@github.com/o/r",
            "https://github.com:444/o/r",
            "https://githubusercontent.com/o",
            "ftp://github.com/o/r",
            "https://github.com/o/r a.deb",
            "//github.com/o/r",
            "",
        ] {
            assert!(!is_github_https_url(bad), "{bad:?}");
        }
    }
}
