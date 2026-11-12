//! Read-only check against the real system: scans and checks for updates, never installs.
//! Run: `cargo run -p package-manager-runtime --example smoke`

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let backend = package_manager_runtime::build(
        package_manager_runtime::Enabled {
            apt: true,
            flatpak: true,
            github: false,
        },
        package_manager_runtime::Services::default(),
    )
    .orchestrator;
    let scanned = backend.scan_all().await;
    println!(
        "{} apps, {} scan errors",
        scanned.items.len(),
        scanned.errors.len()
    );
    for (kind, msg) in &scanned.errors {
        println!("  scan error [{kind}]: {msg}");
    }
    for a in scanned.items.iter().take(8) {
        println!(
            "  {:<9} {:<28} {:<22} {:?} from {}",
            a.source.kind().badge(),
            a.name,
            a.version,
            a.provenance.trust,
            a.provenance.display_name
        );
    }
    let checked = backend.check_all(&scanned.items).await;
    println!(
        "{} updates, {} check errors",
        checked.items.len(),
        checked.errors.len()
    );
    for u in &checked.items {
        println!(
            "  {} {} -> {}{}",
            u.source_kind.badge(),
            u.app.name,
            u.latest_version,
            if u.is_security { " (security)" } else { "" }
        );
    }
    for (kind, msg) in &checked.errors {
        println!("  check error [{kind}]: {msg}");
    }
}
