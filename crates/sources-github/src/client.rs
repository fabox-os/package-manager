use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use package_manager_core::GithubRepo;

use crate::error::GithubError;
use crate::release::{parse_release, Release, MAX_RESPONSE_BYTES};

pub const API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The cached copy (matching the ETag we sent) is still current.
    NotModified,
    Fresh {
        release: Release,
        etag: Option<String>,
    },
}

/// Where release data comes from; a fake in tests, GitHub in production.
#[async_trait]
pub trait ReleaseFetcher: Send + Sync {
    async fn latest(&self, repo: &GithubRepo, etag: Option<&str>) -> Result<Fetched, GithubError>;
}

pub struct HttpFetcher {
    client: reqwest::Client,
    base: String,
}

impl HttpFetcher {
    pub fn new() -> Result<Self, GithubError> {
        Self::with_base(API_BASE)
    }

    /// `base` must be HTTPS: release data and (later) downloads are never fetched in clear text.
    pub fn with_base(base: &str) -> Result<Self, GithubError> {
        if !base.starts_with("https://") {
            return Err(GithubError::UnsafeUrl(base.to_owned()));
        }
        // Follow redirects only within the API host (e.g. a renamed repository).
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if url.scheme() == "https"
                && url.host_str() == Some("api.github.com")
                && attempt.previous().len() < 5
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::Client::builder()
            // A generic client name: no version, user or machine details are sent.
            .user_agent("package-manager")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(policy)
            .build()
            .map_err(|e| GithubError::InvalidResponse(e.to_string()))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        })
    }
}

/// Turns a non-success response into the error the user should see.
/// `remaining` / `reset` come from `x-ratelimit-*`, `retry_after` from `Retry-After`.
pub fn classify_failure(
    status: u16,
    repo: &str,
    remaining: Option<u64>,
    reset_epoch: Option<u64>,
    retry_after: Option<u64>,
    now_epoch: u64,
) -> GithubError {
    match status {
        404 => GithubError::NotFound(repo.to_owned()),
        403 | 429 => {
            let limited = remaining == Some(0) || retry_after.is_some() || status == 429;
            if !limited {
                return GithubError::Http(status);
            }
            let secs = retry_after
                .or_else(|| reset_epoch.map(|r| r.saturating_sub(now_epoch)))
                .unwrap_or(60)
                .max(1);
            GithubError::RateLimited {
                retry_after_secs: secs,
            }
        }
        other => GithubError::Http(other),
    }
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[async_trait]
impl ReleaseFetcher for HttpFetcher {
    async fn latest(&self, repo: &GithubRepo, etag: Option<&str>) -> Result<Fetched, GithubError> {
        // `GithubRepo::parse` guarantees owner and repo contain only [A-Za-z0-9._-].
        let url = format!(
            "{}/repos/{}/{}/releases/latest",
            self.base, repo.owner, repo.repo
        );
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(tag) = etag.filter(|t| t.chars().all(|c| c.is_ascii_graphic() || c == ' ')) {
            req = req.header("If-None-Match", tag);
        }
        let resp = req.send().await.map_err(|e| {
            tracing::debug!("GitHub request failed: {e}");
            GithubError::Offline
        })?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        if status == 304 {
            return Ok(Fetched::NotModified);
        }
        if status != 200 {
            return Err(classify_failure(
                status,
                &repo.full_name(),
                header_u64(&headers, "x-ratelimit-remaining"),
                header_u64(&headers, "x-ratelimit-reset"),
                header_u64(&headers, "retry-after"),
                now_epoch(),
            ));
        }
        if resp
            .content_length()
            .is_some_and(|n| n as usize > MAX_RESPONSE_BYTES)
        {
            return Err(GithubError::InvalidResponse(
                "the response is too large".to_owned(),
            ));
        }
        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // Read in pieces so a response with no Content-Length cannot fill memory.
        let mut resp = resp;
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| GithubError::Offline)? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(GithubError::InvalidResponse(
                    "the response is too large".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Fetched::Fresh {
            release: parse_release(&body)?,
            etag,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_base_is_refused() {
        assert!(matches!(
            HttpFetcher::with_base("http://api.github.com"),
            Err(GithubError::UnsafeUrl(_))
        ));
        assert!(HttpFetcher::new().is_ok());
    }

    #[test]
    fn not_found_names_the_repo() {
        let e = classify_failure(404, "a/b", None, None, None, 0);
        assert!(matches!(e, GithubError::NotFound(ref r) if r == "a/b"));
        assert!(e.to_string().contains("a/b"));
    }

    #[test]
    fn exhausted_rate_limit_counts_down_to_the_reset_time() {
        let e = classify_failure(403, "a/b", Some(0), Some(1_000_300), None, 1_000_000);
        assert!(matches!(
            e,
            GithubError::RateLimited {
                retry_after_secs: 300
            }
        ));
    }

    #[test]
    fn retry_after_wins_and_429_is_always_a_rate_limit() {
        let e = classify_failure(429, "a/b", None, None, Some(42), 0);
        assert!(matches!(
            e,
            GithubError::RateLimited {
                retry_after_secs: 42
            }
        ));
        let e = classify_failure(429, "a/b", None, None, None, 0);
        assert!(matches!(
            e,
            GithubError::RateLimited {
                retry_after_secs: 60
            }
        ));
    }

    #[test]
    fn a_403_with_quota_left_is_a_plain_error_not_a_rate_limit() {
        assert!(matches!(
            classify_failure(403, "a/b", Some(12), Some(5), None, 0),
            GithubError::Http(403)
        ));
    }

    #[test]
    fn reset_time_in_the_past_still_waits_at_least_one_second() {
        let e = classify_failure(403, "a/b", Some(0), Some(5), None, 1000);
        assert!(matches!(
            e,
            GithubError::RateLimited {
                retry_after_secs: 1
            }
        ));
    }

    #[test]
    fn server_errors_are_reported_with_their_code() {
        assert!(matches!(
            classify_failure(502, "a/b", None, None, None, 0),
            GithubError::Http(502)
        ));
    }
}
