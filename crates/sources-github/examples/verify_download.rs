//! Downloads a real release asset and verifies it against the maintainer's checksum.
//! Never installs anything. Usage: verify_download <owner/repo> <pattern> <dir>
use std::path::PathBuf;

use package_manager_core::GithubRepo;
use package_manager_sources_github::*;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (repo, pattern, dir) = (
        GithubRepo::parse(&args[1]).expect("repo"),
        args[2].clone(),
        PathBuf::from(&args[3]),
    );
    let fetcher = HttpFetcher::new().expect("client");
    let Ok(Fetched::Fresh { release, .. }) = fetcher.latest(&repo, None).await else {
        return println!("no release");
    };
    let asset =
        select_asset(&release.assets, &pattern, release.version(), host_arch()).expect("asset");
    println!("asset {} ({} bytes)", asset.name, asset.size);
    let dl = HttpDownloader::new().expect("downloader");
    let src = find_checksum_source(&release.assets, &asset.name).expect("checksum source");
    let text = dl
        .fetch_text(&src.asset.browser_download_url, MAX_CHECKSUM_BYTES)
        .await
        .expect("checksum text");
    let want = parse_checksum(src.kind, &text, &asset.name).expect("checksum for asset");
    println!("published checksum: {:?} via {}", src.kind, src.asset.name);

    std::fs::create_dir_all(&dir).expect("dir");
    let part = dir.join(format!("{}.part", asset.name));
    let before = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    println!("starting with {before} bytes already on disk");
    let last = std::sync::atomic::AtomicU8::new(0);
    let result = dl
        .download(&asset.browser_download_url, &part, asset.size, &|p| {
            let prev = last.swap(p, std::sync::atomic::Ordering::Relaxed);
            if p / 20 != prev / 20 {
                println!("  {p}%");
            }
        })
        .await;
    if let Err(e) = result {
        return println!(
            "download stopped: {e} ({} bytes kept)",
            std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
        );
    }
    println!(
        "size ok: {}",
        std::fs::metadata(&part).expect("meta").len() == asset.size
    );
    println!("deb magic ok: {}", has_deb_magic(&part).expect("magic"));
    let got = file_digest(&part, &want).expect("hash");
    println!("CHECKSUM MATCHES PUBLISHED: {}", got == want);
}
