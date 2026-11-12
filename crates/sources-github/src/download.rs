//! Fetching a release download safely: HTTPS to GitHub hosts only, resumable, size-bounded,
//! then verified (size, `.deb` format, checksum) before anything is installed.

use std::fs;
use std::io::{Read, SeekFrom};
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest as _, Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use crate::assets::Digest;
use crate::error::GithubError;

/// Largest download accepted, whatever the release claims.
pub const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest checksum file accepted.
pub const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;

#[async_trait]
pub trait Downloader: Send + Sync {
    /// Small file as raw bytes (a checksum list).
    async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, GithubError>;

    /// Small text file (a checksum list).
    async fn fetch_text(&self, url: &str, max_bytes: usize) -> Result<String, GithubError> {
        Ok(String::from_utf8_lossy(&self.fetch_bytes(url, max_bytes).await?).into_owned())
    }

    /// Downloads into `part`, continuing from what is already there. Returns when the file
    /// holds exactly `expected_size` bytes. `progress` gets whole percentages.
    async fn download(
        &self,
        url: &str,
        part: &Path,
        expected_size: u64,
        progress: &(dyn Fn(u8) + Send + Sync),
    ) -> Result<(), GithubError>;
}

/// Release downloads redirect from github.com to GitHub's release CDN. Only those exact hosts
/// are followed (not `raw.githubusercontent.com`, which serves arbitrary user content).
const ALLOWED_HOSTS: [&str; 4] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

fn is_allowed_redirect(scheme: &str, host: Option<&str>) -> bool {
    scheme == "https" && host.is_some_and(|h| ALLOWED_HOSTS.contains(&h))
}

pub struct HttpDownloader {
    client: reqwest::Client,
}

impl HttpDownloader {
    pub fn new() -> Result<Self, GithubError> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if is_allowed_redirect(url.scheme(), url.host_str()) && attempt.previous().len() < 5 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent("package-manager")
            .connect_timeout(Duration::from_secs(10))
            // No total timeout (downloads are large) but a stalled connection is dropped.
            .read_timeout(Duration::from_secs(30))
            .redirect(policy)
            .build()
            .map_err(|e| GithubError::DownloadFailed(e.to_string()))?;
        Ok(Self { client })
    }
}

fn net(e: &reqwest::Error) -> GithubError {
    tracing::debug!("download request failed: {e}");
    GithubError::DownloadFailed("the connection was interrupted. Try again; it will resume.".into())
}

#[async_trait]
impl Downloader for HttpDownloader {
    async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, GithubError> {
        let mut resp = self.client.get(url).send().await.map_err(|e| net(&e))?;
        if !resp.status().is_success() {
            return Err(GithubError::Http(resp.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| net(&e))? {
            if body.len() + chunk.len() > max_bytes {
                return Err(GithubError::InvalidResponse("the file is too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn download(
        &self,
        url: &str,
        part: &Path,
        expected_size: u64,
        progress: &(dyn Fn(u8) + Send + Sync),
    ) -> Result<(), GithubError> {
        if expected_size == 0 || expected_size > MAX_DOWNLOAD_BYTES {
            return Err(GithubError::DownloadFailed(
                "the release reports an invalid file size".into(),
            ));
        }
        // One retry: a stale partial file can make the server answer 416.
        for attempt in 0..2 {
            let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
            if have > expected_size {
                let _ = fs::remove_file(part);
                continue;
            }
            if have == expected_size {
                progress(100);
                return Ok(());
            }
            let mut req = self.client.get(url);
            if have > 0 {
                req = req.header("Range", format!("bytes={have}-"));
            }
            let mut resp = req.send().await.map_err(|e| net(&e))?;
            let status = resp.status().as_u16();
            let resume_from = match status {
                206 => {
                    let ok = resp
                        .headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
                    if !ok {
                        return Err(GithubError::DownloadFailed(
                            "the server sent an unexpected range".into(),
                        ));
                    }
                    have
                }
                200 => 0, // server ignored the range: start over
                416 if attempt == 0 => {
                    let _ = fs::remove_file(part);
                    continue;
                }
                other => return Err(GithubError::Http(other)),
            };

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(resume_from == 0)
                .open(part)
                .await
                .map_err(|e| {
                    GithubError::DownloadFailed(format!("could not write the download: {e}"))
                })?;
            if resume_from > 0 {
                use tokio::io::AsyncSeekExt;
                file.seek(SeekFrom::Start(resume_from))
                    .await
                    .map_err(|e| GithubError::DownloadFailed(e.to_string()))?;
            }
            let (mut written, mut last_percent) = (resume_from, 0u8);
            while let Some(chunk) = resp.chunk().await.map_err(|e| net(&e))? {
                written += chunk.len() as u64;
                if written > expected_size {
                    drop(file);
                    let _ = fs::remove_file(part);
                    return Err(GithubError::SizeMismatch {
                        expected: expected_size,
                        actual: written,
                    });
                }
                file.write_all(&chunk).await.map_err(|e| {
                    GithubError::DownloadFailed(format!("could not write the download: {e}"))
                })?;
                let percent = (written * 100 / expected_size) as u8;
                if percent != last_percent {
                    last_percent = percent;
                    progress(percent);
                }
            }
            file.flush()
                .await
                .map_err(|e| GithubError::DownloadFailed(e.to_string()))?;
            file.sync_all()
                .await
                .map_err(|e| GithubError::DownloadFailed(e.to_string()))?;
            if written != expected_size {
                // Keep the partial file: the next attempt resumes from it.
                return Err(GithubError::DownloadFailed(
                    "the connection closed early. Try again; it will resume.".into(),
                ));
            }
            return Ok(());
        }
        Err(GithubError::DownloadFailed(
            "the server could not resume the download".into(),
        ))
    }
}

// ------------------------------------------------------------------- verification

/// Blocking: hashes the whole file. Call from `spawn_blocking`.
pub fn file_digest(path: &Path, like: &Digest) -> std::io::Result<Digest> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    match like {
        Digest::Sha256(_) => {
            let mut h = Sha256::new();
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(Digest::Sha256(h.finalize().into()))
        }
        Digest::Sha512(_) => {
            let mut h = Sha512::new();
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(Digest::Sha512(h.finalize().into()))
        }
    }
}

/// SHA-256 of a file, used to pin what the user confirmed when no checksum is published.
pub fn sha256_file(path: &Path) -> std::io::Result<Digest> {
    file_digest(path, &Digest::Sha256([0; 32]))
}

/// A `.deb` is an `ar` archive whose first member is `debian-binary`.
pub fn has_deb_magic(path: &Path) -> std::io::Result<bool> {
    let mut head = [0u8; 21];
    let mut f = fs::File::open(path)?;
    Ok(f.read_exact(&mut head).is_ok()
        && &head[..8] == b"!<arch>\n"
        && &head[8..21] == b"debian-binary")
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `(algorithm, lowercase hex)` as the helper expects.
pub fn digest_args(d: &Digest) -> (&'static str, String) {
    match d {
        Digest::Sha256(b) => ("sha256", to_hex(b)),
        Digest::Sha512(b) => ("sha512", to_hex(b)),
    }
}

/// A single path component safe to use in the downloads folder.
pub fn safe_component(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && s.len() <= 120
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~' | '-'));
    ok.then_some(s)
}

/// Keeps the `keep` most recently modified version folders in `package_dir` (and always
/// `current`), deleting older ones. Never touches anything outside `package_dir`.
pub fn prune_old_versions(package_dir: &Path, current: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(package_dir) else {
        return;
    };
    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, path) in dirs.into_iter().skip(keep) {
        if path != current {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("um-dl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir");
        d
    }

    /// A tiny HTTP server for one body. Honours `Range`. `cut_first` drops the first response
    /// after that many body bytes, simulating a lost connection.
    async fn serve(
        body: Vec<u8>,
        cut_first: Option<usize>,
        ignore_range: bool,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/file.deb", listener.local_addr().expect("addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(k) => req.extend_from_slice(&buf[..k]),
                        }
                    }
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let start = text
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                        .filter(|_| !ignore_range)
                        .unwrap_or(0);
                    let (status, extra) = if start > 0 {
                        (
                            "206 Partial Content",
                            format!(
                                "Content-Range: bytes {start}-{}/{}\r\n",
                                body.len() - 1,
                                body.len()
                            ),
                        )
                    } else {
                        ("200 OK", String::new())
                    };
                    let payload = &body[start..];
                    let head = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let cut = if n == 0 { cut_first } else { None };
                    let send = cut.map_or(payload.len(), |c| c.min(payload.len()));
                    let _ = sock.write_all(&payload[..send]).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (url, hits)
    }

    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn downloads_a_file_and_reports_progress() {
        let dir = tmp("plain");
        let data = body(300_000);
        let (url, _) = serve(data.clone(), None, false).await;
        let seen = std::sync::Mutex::new(Vec::new());
        HttpDownloader::new()
            .expect("client")
            .download(&url, &dir.join("a.part"), data.len() as u64, &|p| {
                seen.lock().expect("l").push(p)
            })
            .await
            .expect("download");
        assert_eq!(fs::read(dir.join("a.part")).expect("read"), data);
        let seen = seen.into_inner().expect("l");
        assert_eq!(seen.last(), Some(&100));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
    }

    #[tokio::test]
    async fn an_interrupted_download_resumes_from_the_partial_file() {
        let dir = tmp("resume");
        let data = body(500_000);
        let (url, hits) = serve(data.clone(), Some(120_000), false).await;
        let dl = HttpDownloader::new().expect("client");
        let part = dir.join("a.part");
        let first = dl.download(&url, &part, data.len() as u64, &|_| {}).await;
        assert!(first.is_err(), "the cut connection must be reported");
        let partial = fs::metadata(&part).expect("partial kept").len();
        assert!(
            partial > 0 && partial < data.len() as u64,
            "partial = {partial}"
        );
        dl.download(&url, &part, data.len() as u64, &|_| {})
            .await
            .expect("resume");
        assert_eq!(
            fs::read(&part).expect("read"),
            data,
            "resumed file must be byte-identical"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_server_that_ignores_ranges_restarts_cleanly() {
        let dir = tmp("norange");
        let data = body(200_000);
        let (url, _) = serve(data.clone(), None, true).await;
        let part = dir.join("a.part");
        fs::write(&part, &data[..50_000]).expect("seed partial");
        HttpDownloader::new()
            .expect("c")
            .download(&url, &part, data.len() as u64, &|_| {})
            .await
            .expect("download");
        assert_eq!(fs::read(&part).expect("read"), data);
    }

    #[tokio::test]
    async fn more_data_than_announced_is_rejected_and_deleted() {
        let dir = tmp("toobig");
        let data = body(100_000);
        let (url, _) = serve(data, None, false).await;
        let part = dir.join("a.part");
        let err = HttpDownloader::new()
            .expect("c")
            .download(&url, &part, 10_000, &|_| {})
            .await
            .expect_err("too big");
        assert!(matches!(err, GithubError::SizeMismatch { .. }), "{err}");
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn a_complete_partial_file_is_not_downloaded_again() {
        let dir = tmp("complete");
        let data = body(10_000);
        let (url, hits) = serve(data.clone(), None, false).await;
        let part = dir.join("a.part");
        fs::write(&part, &data).expect("seed");
        HttpDownloader::new()
            .expect("c")
            .download(&url, &part, data.len() as u64, &|_| {})
            .await
            .expect("ok");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn absurd_sizes_are_refused_before_any_request() {
        let dl = HttpDownloader::new().expect("c");
        for size in [0, MAX_DOWNLOAD_BYTES + 1] {
            let r = dl
                .download(
                    "http://127.0.0.1:9/x",
                    Path::new("/nonexistent/a.part"),
                    size,
                    &|_| {},
                )
                .await;
            assert!(matches!(r, Err(GithubError::DownloadFailed(_))));
        }
    }

    #[tokio::test]
    async fn checksum_text_is_size_limited() {
        let (url, _) = serve(vec![b'a'; 5_000], None, false).await;
        let dl = HttpDownloader::new().expect("c");
        assert_eq!(
            dl.fetch_text(&url, 10_000).await.expect("small").len(),
            5_000
        );
        assert!(dl.fetch_text(&url, 1_000).await.is_err());
    }

    #[test]
    fn redirects_are_limited_to_github_over_https() {
        assert!(is_allowed_redirect("https", Some("github.com")));
        assert!(is_allowed_redirect(
            "https",
            Some("objects.githubusercontent.com")
        ));
        assert!(is_allowed_redirect(
            "https",
            Some("release-assets.githubusercontent.com")
        ));
        assert!(!is_allowed_redirect(
            "https",
            Some("raw.githubusercontent.com")
        ));
        assert!(!is_allowed_redirect(
            "https",
            Some("gist.githubusercontent.com")
        ));
        for (scheme, host) in [
            ("http", Some("github.com")),
            ("https", Some("evil.test")),
            ("https", Some("githubusercontent.com.evil.test")),
            ("https", Some("notgithub.com")),
            ("https", None),
        ] {
            assert!(!is_allowed_redirect(scheme, host), "{scheme} {host:?}");
        }
    }

    #[test]
    fn digests_match_known_vectors_for_both_algorithms() {
        let dir = tmp("digest");
        fs::write(dir.join("abc"), b"abc").expect("w");
        let Digest::Sha256(d) = sha256_file(&dir.join("abc")).expect("hash") else {
            panic!("algo")
        };
        assert_eq!(
            to_hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let Digest::Sha512(d) =
            file_digest(&dir.join("abc"), &Digest::Sha512([0; 64])).expect("hash")
        else {
            panic!("algo")
        };
        assert!(to_hex(&d).starts_with("ddaf35a193617aba"));
        assert_eq!(digest_args(&Digest::Sha256([0xab; 32])).0, "sha256");
        assert_eq!(digest_args(&Digest::Sha512([0xab; 64])).1.len(), 128);
    }

    #[test]
    fn deb_magic_detection() {
        let dir = tmp("magic");
        let mut deb = b"!<arch>\ndebian-binary   ".to_vec();
        deb.extend_from_slice(&[0; 40]);
        fs::write(dir.join("ok.deb"), &deb).expect("w");
        fs::write(dir.join("bad.deb"), b"MZ this is a windows exe....").expect("w");
        fs::write(dir.join("short.deb"), b"!<arch>").expect("w");
        assert!(has_deb_magic(&dir.join("ok.deb")).expect("ok"));
        assert!(!has_deb_magic(&dir.join("bad.deb")).expect("bad"));
        assert!(!has_deb_magic(&dir.join("short.deb")).expect("short"));
        assert!(has_deb_magic(&dir.join("missing.deb")).is_err());
    }

    #[test]
    fn only_plain_names_are_allowed_as_folder_components() {
        for ok in [
            "hydralauncher",
            "4.4.0",
            "1.0~rc1+git",
            "hydralauncher_4.4.0_amd64.deb",
        ] {
            assert!(safe_component(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "a b",
            "a\u{0}b",
            &"x".repeat(121),
        ] {
            assert!(safe_component(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn pruning_keeps_the_newest_versions_and_the_current_one() {
        let dir = tmp("prune");
        for (i, v) in ["1.0", "2.0", "3.0", "4.0"].iter().enumerate() {
            fs::create_dir_all(dir.join(v)).expect("mk");
            fs::write(dir.join(v).join("f.deb"), b"x").expect("w");
            let t = std::time::SystemTime::now() - Duration::from_secs(1000 - i as u64 * 100);
            fs::File::open(dir.join(v))
                .expect("o")
                .set_modified(t)
                .expect("mtime");
        }
        fs::write(dir.join("stray-file"), b"keep me").expect("w");
        prune_old_versions(&dir, &dir.join("4.0"), 2);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["3.0", "4.0", "stray-file"]);
    }

    #[test]
    fn pruning_a_missing_folder_is_harmless() {
        prune_old_versions(Path::new("/nonexistent/folder"), Path::new("/x"), 2);
    }
}
