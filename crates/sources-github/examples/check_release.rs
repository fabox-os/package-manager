//! Read-only check against the real GitHub API (uses 1-2 of the 60 unauthenticated
//! requests per hour). Run: `cargo run -p package-manager-sources-github --example check_release -- hydralauncher/hydra`
use package_manager_core::GithubRepo;
use package_manager_sources_github::{
    find_checksum_source, host_arch, preset_for_package, select_asset, Fetched, HttpFetcher,
    ReleaseFetcher,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hydralauncher/hydra".to_owned());
    let Some(repo) = GithubRepo::parse(&arg) else {
        eprintln!("not a valid owner/repo: {arg:?}");
        return;
    };
    let fetcher = HttpFetcher::new().expect("client");

    println!("1) first request (no ETag)");
    let (release, etag) = match fetcher.latest(&repo, None).await {
        Ok(Fetched::Fresh { release, etag }) => (release, etag),
        Ok(Fetched::NotModified) => return println!("   unexpected 304"),
        Err(e) => return println!("   error: {e}"),
    };
    println!(
        "   tag {} ({} assets), etag {:?}",
        release.tag_name,
        release.assets.len(),
        etag
    );

    let pattern = preset_for_package("hydralauncher")
        .filter(|p| p.repo == arg)
        .map_or("*".to_owned(), |p| p.asset_pattern.clone());
    match select_asset(&release.assets, &pattern, release.version(), host_arch()) {
        Some(a) => {
            println!("   asset {} ({} bytes)", a.name, a.size);
            match find_checksum_source(&release.assets, &a.name) {
                Some(c) => println!("   checksum: {:?} via {}", c.kind, c.asset.name),
                None => println!("   checksum: none published"),
            }
        }
        None => println!("   no asset matches {pattern:?}"),
    }

    println!("2) second request with the ETag");
    match fetcher.latest(&repo, etag.as_deref()).await {
        Ok(Fetched::NotModified) => println!("   304 Not Modified: the cache would be used"),
        Ok(Fetched::Fresh { .. }) => println!("   200 again (ETag was not honoured)"),
        Err(e) => println!("   error: {e}"),
    }

    println!("3) error handling");
    let missing = GithubRepo::parse("hydralauncher/this-repo-does-not-exist-xyz").expect("repo");
    match fetcher.latest(&missing, None).await {
        Err(e) => println!("   missing repo -> {e}"),
        Ok(_) => println!("   unexpectedly found"),
    }
}
