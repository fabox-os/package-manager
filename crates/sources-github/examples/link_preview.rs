//! Read-only: what "Add from GitHub" would show for real repositories.
//! Run: cargo run -p package-manager-sources-github --example link_preview -- <input>...
use std::sync::Arc;

use async_trait::async_trait;
use package_manager_core::exec::SystemRunner;
use package_manager_core::log::LogBroadcaster;
use package_manager_core::{SourceError, TrackedApp, TrackedStore};
use package_manager_sources_github::{ChecksumState, HttpFetcher, Linker};

struct NoStore;

#[async_trait]
impl TrackedStore for NoStore {
    async fn list(&self) -> Result<Vec<TrackedApp>, SourceError> {
        Ok(vec![])
    }
    async fn save(&self, _: &TrackedApp) -> Result<(), SourceError> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let linker = Linker::new(
        Arc::new(HttpFetcher::new().expect("client")),
        None,
        Arc::new(NoStore),
        Box::new(SystemRunner::new(LogBroadcaster::new(), "GitHub")),
    );
    for input in std::env::args().skip(1) {
        println!("\n> {input}");
        match linker.preview(&input).await {
            Err(e) => println!("  ERROR: {e}"),
            Ok(p) => {
                println!(
                    "  {} {} (preset: {:?})",
                    p.repo.full_name(),
                    p.tag,
                    p.preset_name
                );
                println!(
                    "  package guess: {:?}, installed now: {:?}",
                    p.suggested_package,
                    linker.installed_version(&p.suggested_package).await
                );
                for c in &p.choices {
                    let sum = match c.checksum {
                        ChecksumState::Published(k) => format!("{k:?}"),
                        other => format!("{other:?}"),
                    };
                    println!(
                        "  - {} ({:.1} MB) pattern {:?} checksum {sum}",
                        c.name,
                        c.size as f64 / 1e6,
                        c.pattern
                    );
                }
                println!("  trust: {:?}", p.provenance(0).trust);
            }
        }
    }
}
