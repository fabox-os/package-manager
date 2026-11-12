//! Read-only: reports how many real apps get a display name and an icon file.
//! Run: `cargo run -p package-manager-runtime --example icons`
use package_manager_core::IconRef;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let backend = package_manager_runtime::build(
        package_manager_runtime::Enabled {
            apt: true,
            flatpak: true,
            github: false,
        },
        package_manager_runtime::Services::default(),
    );
    let scanned = backend.orchestrator.scan_all().await;
    let before: Vec<String> = scanned.items.iter().map(|a| a.name.clone()).collect();
    let started = std::time::Instant::now();
    let apps = package_manager_runtime::Enricher::system_default().enrich(scanned.items);
    println!("enriched {} apps in {:?}", apps.len(), started.elapsed());
    let renamed = apps
        .iter()
        .zip(&before)
        .filter(|(a, b)| &a.name != *b)
        .count();
    let with_icon = apps
        .iter()
        .filter(|a| matches!(a.icon, IconRef::File(_)))
        .count();
    let with_desktop = apps.iter().filter(|a| a.desktop_file.is_some()).count();
    println!(
        "{with_desktop} have a desktop file, {renamed} renamed, {with_icon} have an icon file"
    );
    for a in apps.iter().filter(|a| a.desktop_file.is_some()).take(12) {
        let icon = match &a.icon {
            IconRef::File(p) => p.display().to_string(),
            other => format!("{other:?}"),
        };
        println!("  {:<10} {:<34} {}", a.source.kind().badge(), a.name, icon);
    }
    let missing: Vec<_> = apps
        .iter()
        .filter(|a| a.desktop_file.is_some() && !matches!(a.icon, IconRef::File(_)))
        .map(|a| a.name.as_str())
        .collect();
    println!("desktop entries without a resolved icon: {missing:?}");
}
