//! Choosing the right download from a release, and finding its checksum.

use base64::Engine;

use crate::release::{is_github_https_url, Asset};

// ------------------------------------------------------------------ pattern matching

/// Case-insensitive glob where `*` matches any run of characters and `{version}` is the
/// release version. The version is inserted literally, so a hostile tag such as `v*` cannot
/// widen the match.
pub fn matches_pattern(pattern: &str, name: &str, version: &str) -> bool {
    let name = name.to_lowercase();
    let version = version.to_lowercase();
    let pieces: Vec<String> = pattern
        .to_lowercase()
        .split('*')
        .map(|p| p.replace("{version}", &version))
        .collect();
    let (first, rest) = match pieces.split_first() {
        Some(x) => x,
        None => return name.is_empty(),
    };
    if rest.is_empty() {
        return name == *first; // no wildcard: exact match
    }
    let Some(mut remaining) = name.strip_prefix(first.as_str()) else {
        return false;
    };
    let last = &rest[rest.len() - 1];
    for middle in &rest[..rest.len() - 1] {
        match remaining.find(middle.as_str()) {
            Some(i) => remaining = &remaining[i + middle.len()..],
            None => return false,
        }
    }
    remaining.ends_with(last.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssetKind {
    Deb,
    AppImage,
    TarGz,
}

pub fn asset_kind(name: &str) -> Option<AssetKind> {
    let n = name.to_lowercase();
    if n.ends_with(".deb") {
        Some(AssetKind::Deb)
    } else if n.ends_with(".appimage") {
        Some(AssetKind::AppImage)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(AssetKind::TarGz)
    } else {
        None
    }
}

/// The CPU architecture in Debian naming.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// File-name tokens that name a CPU architecture, normalized to Debian names.
/// Matching is per token (`gh_2.1_linux_386.deb` -> `386`), never by substring, so a version
/// like `1.2386.0` cannot be mistaken for an architecture.
fn arch_tokens_in(name: &str) -> Vec<String> {
    let n = name
        .to_lowercase()
        .replace("x86_64", "amd64")
        .replace("x86-64", "amd64")
        .replace("aarch64", "arm64");
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| if t == "x64" { "amd64" } else { t })
        .filter(|t| {
            matches!(
                *t,
                "amd64"
                    | "arm64"
                    | "i386"
                    | "i686"
                    | "386"
                    | "armhf"
                    | "armv5"
                    | "armv6"
                    | "armv7"
                    | "arm"
                    | "ppc64"
                    | "ppc64le"
                    | "s390x"
                    | "riscv64"
                    | "mips"
                    | "mipsel"
                    | "mips64"
                    | "loong64"
            )
        })
        .map(str::to_owned)
        .collect()
}

/// True when the file name names a CPU architecture other than `host`.
fn is_for_other_arch(name: &str, host: &str) -> bool {
    let tokens = arch_tokens_in(name);
    !tokens.is_empty() && !tokens.iter().any(|t| t == host)
}

fn mentions_arch(name: &str, host: &str) -> bool {
    arch_tokens_in(name).iter().any(|t| t == host)
}

/// Best first: `.deb` before AppImage before tarball, files that name this CPU before
/// arch-less ones, and the regular build before a `-musl` variant (a different package).
fn rank<'a>(kind: AssetKind, a: &'a Asset, arch: &str) -> (AssetKind, bool, bool, &'a str) {
    (
        kind,
        !mentions_arch(&a.name, arch),
        a.name.to_lowercase().contains("musl"),
        a.name.as_str(),
    )
}

/// Picks the download for `pattern`: only `.deb`, AppImage or tarball files on github.com
/// over HTTPS, for this CPU, preferring `.deb`. `None` if nothing qualifies.
pub fn select_asset<'a>(
    assets: &'a [Asset],
    pattern: &str,
    version: &str,
    arch: &str,
) -> Option<&'a Asset> {
    assets
        .iter()
        .filter(|a| is_github_https_url(&a.browser_download_url))
        .filter(|a| matches_pattern(pattern, &a.name, version))
        .filter(|a| !is_for_other_arch(&a.name, arch))
        .filter_map(|a| Some((asset_kind(&a.name)?, a)))
        .min_by_key(|(k, a)| rank(*k, a, arch))
        .map(|(_, a)| a)
}

/// Downloads worth offering when the user links a repo by hand: installable files for this
/// CPU, best first.
pub fn suggest_assets<'a>(assets: &'a [Asset], arch: &str) -> Vec<&'a Asset> {
    let mut found: Vec<_> = assets
        .iter()
        .filter(|a| is_github_https_url(&a.browser_download_url))
        .filter(|a| !is_for_other_arch(&a.name, arch))
        .filter_map(|a| Some((asset_kind(&a.name)?, a)))
        .collect();
    found.sort_by_key(|(k, a)| rank(*k, a, arch));
    found.into_iter().map(|(_, a)| a).collect()
}

/// Turns a release asset name into a reusable pattern by replacing the version:
/// `hydralauncher_4.4.0_amd64.deb` + `4.4.0` -> `hydralauncher_{version}_amd64.deb`.
pub fn pattern_from_asset(name: &str, version: &str) -> String {
    if version.is_empty() {
        name.to_owned()
    } else {
        name.replacen(version, "{version}", 1)
    }
}

// ------------------------------------------------------------------------ checksums

/// Where a maintainer publishes a checksum, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumKind {
    /// `<file>.sha256` or `<file>.sha256sum` next to the download.
    Sidecar,
    /// `SHA256SUMS` / `checksums.txt` listing many files.
    SumsFile,
    /// electron-builder's `latest-linux.yml` (base64 SHA-512 per file).
    ElectronYml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChecksumSource<'a> {
    pub kind: ChecksumKind,
    pub asset: &'a Asset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    Sha256([u8; 32]),
    Sha512([u8; 64]),
}

/// The checksum file published for `target`, if any. Only github.com HTTPS assets count.
pub fn find_checksum_source<'a>(assets: &'a [Asset], target: &str) -> Option<ChecksumSource<'a>> {
    let usable = |a: &&Asset| is_github_https_url(&a.browser_download_url);
    let find = |pred: &dyn Fn(&str) -> bool| {
        assets
            .iter()
            .filter(usable)
            .find(|a| pred(&a.name.to_lowercase()))
    };
    let t = target.to_lowercase();
    if let Some(asset) = find(&|n| n == format!("{t}.sha256") || n == format!("{t}.sha256sum")) {
        return Some(ChecksumSource {
            kind: ChecksumKind::Sidecar,
            asset,
        });
    }
    // Whole-release lists: `SHA256SUMS`, `checksums.txt`, `gh_2.1.0_checksums.txt`, ...
    let is_list = |n: &str| {
        [
            "sha256sums",
            "sha256sums.txt",
            "checksums.txt",
            "checksums.sha256",
            "checksums.sha256sum",
        ]
        .iter()
        .any(|suffix| n.ends_with(suffix))
    };
    if let Some(asset) = find(&is_list) {
        return Some(ChecksumSource {
            kind: ChecksumKind::SumsFile,
            asset,
        });
    }
    find(&|n| n == "latest-linux.yml").map(|asset| ChecksumSource {
        kind: ChecksumKind::ElectronYml,
        asset,
    })
}

fn hex_to_bytes<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn clean_name(name: &str) -> &str {
    name.trim().trim_start_matches('*').trim_start_matches("./")
}

/// Extracts the digest for `target` from a checksum file's text. `None` means the file does
/// not vouch for `target`, which callers must treat as "not verified".
pub fn parse_checksum(kind: ChecksumKind, text: &str, target: &str) -> Option<Digest> {
    match kind {
        ChecksumKind::Sidecar => {
            let mut parts = text.split_whitespace();
            let hash = parts.next()?;
            // A `<hash>  <name>` sidecar must name the file it was made for.
            if let Some(name) = parts.next() {
                if clean_name(name) != target {
                    return None;
                }
            }
            hex_to_bytes::<32>(hash).map(Digest::Sha256)
        }
        ChecksumKind::SumsFile => text.lines().find_map(|line| {
            let (hash, name) = line.trim().split_once(char::is_whitespace)?;
            (clean_name(name) == target).then(|| hex_to_bytes::<32>(hash).map(Digest::Sha256))?
        }),
        ChecksumKind::ElectronYml => parse_electron_yml(text, target),
    }
}

/// Reads the `files:` list of `latest-linux.yml`:
/// `- url: <name>` followed by `sha512: <base64>`.
fn parse_electron_yml(text: &str, target: &str) -> Option<Digest> {
    let mut current: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(url) = t.strip_prefix("- url:") {
            current = Some(url.trim().trim_matches(['\'', '"']));
        } else if let Some(b64) = t.strip_prefix("sha512:") {
            if current.is_some_and(|u| clean_name(u) == target) {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim().trim_matches(['\'', '"']))
                    .ok()?;
                return <[u8; 64]>::try_from(bytes.as_slice())
                    .ok()
                    .map(Digest::Sha512);
            }
        } else if !t.is_empty() && !t.starts_with('-') && !line.starts_with(' ') {
            current = None; // left the files list
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::parse_release;

    fn hydra_assets() -> Vec<Asset> {
        parse_release(include_bytes!("../tests/fixtures/hydra-release.json"))
            .expect("fixture")
            .assets
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            size: 1,
            browser_download_url: format!("https://github.com/o/r/releases/download/v1/{name}"),
        }
    }

    #[test]
    fn version_placeholder_and_wildcards() {
        assert!(matches_pattern(
            "hydralauncher_{version}_amd64.deb",
            "hydralauncher_4.4.0_amd64.deb",
            "4.4.0"
        ));
        assert!(!matches_pattern(
            "hydralauncher_{version}_amd64.deb",
            "hydralauncher_4.3.0_amd64.deb",
            "4.4.0"
        ));
        assert!(matches_pattern("*_amd64.deb", "tool_1.0_amd64.deb", "1.0"));
        assert!(matches_pattern("*.AppImage", "Tool-1.0.appimage", "1.0"));
        assert!(matches_pattern(
            "tool-*-linux-*.deb",
            "tool-1.0-linux-amd64.deb",
            "1.0"
        ));
        assert!(!matches_pattern(
            "tool-*-linux-*.deb",
            "tool-1.0-win-amd64.deb",
            "1.0"
        ));
        assert!(matches_pattern("exact.deb", "EXACT.deb", "1"));
        assert!(!matches_pattern("exact.deb", "exact.deb.sig", "1"));
    }

    #[test]
    fn hostile_version_cannot_widen_the_match() {
        // A tag like `v*` is inserted literally, not as a wildcard.
        assert!(!matches_pattern(
            "tool_{version}_amd64.deb",
            "tool_1.0_amd64.deb",
            "*"
        ));
        assert!(matches_pattern(
            "tool_{version}_amd64.deb",
            "tool_*_amd64.deb",
            "*"
        ));
    }

    #[test]
    fn selects_the_real_hydra_deb_over_every_other_file() {
        let assets = hydra_assets();
        let pick = select_asset(
            &assets,
            "hydralauncher_{version}_amd64.deb",
            "4.4.0",
            "amd64",
        )
        .expect("deb");
        assert_eq!(pick.name, "hydralauncher_4.4.0_amd64.deb");
        // The wrong pattern from the original design doc matches nothing.
        assert!(select_asset(&assets, "hydra_{version}_amd64.deb", "4.4.0", "amd64").is_none());
    }

    #[test]
    fn prefers_deb_then_appimage_and_never_picks_checksums_or_installers() {
        let assets = vec![
            asset("t-1.0.AppImage"),
            asset("t_1.0_amd64.deb"),
            asset("t-1.0.exe"),
            asset("t_1.0_amd64.deb.sha256"),
        ];
        assert_eq!(
            select_asset(&assets, "*", "1.0", "amd64")
                .expect("pick")
                .name,
            "t_1.0_amd64.deb"
        );
        let only_appimage = vec![asset("t-1.0.AppImage"), asset("t-1.0.exe")];
        assert_eq!(
            select_asset(&only_appimage, "*", "1.0", "amd64")
                .expect("pick")
                .name,
            "t-1.0.AppImage"
        );
        assert!(select_asset(
            &[asset("t-1.0.exe"), asset("t.sha256")],
            "*",
            "1.0",
            "amd64"
        )
        .is_none());
    }

    #[test]
    fn skips_downloads_for_other_cpus() {
        let assets = vec![
            asset("t_1.0_arm64.deb"),
            asset("t_1.0_amd64.deb"),
            asset("t_1.0_armhf.deb"),
        ];
        assert_eq!(
            select_asset(&assets, "*.deb", "1.0", "amd64")
                .expect("pick")
                .name,
            "t_1.0_amd64.deb"
        );
        assert_eq!(
            select_asset(&assets, "*.deb", "1.0", "arm64")
                .expect("pick")
                .name,
            "t_1.0_arm64.deb"
        );
        assert!(select_asset(&[asset("t_1.0_arm64.deb")], "*.deb", "1.0", "amd64").is_none());
        // x86_64 naming counts as amd64.
        assert!(select_asset(&[asset("t-1.0.x86_64.AppImage")], "*", "1.0", "amd64").is_some());
    }

    #[test]
    fn refuses_assets_not_hosted_on_github_https() {
        let mut bad = asset("t_1.0_amd64.deb");
        bad.browser_download_url = "http://github.com/o/r/t_1.0_amd64.deb".into();
        let mut other = asset("t_1.0_amd64.deb");
        other.browser_download_url = "https://evil.test/t_1.0_amd64.deb".into();
        assert!(select_asset(&[bad, other], "*", "1.0", "amd64").is_none());
    }

    #[test]
    fn suggestions_and_pattern_generation_for_the_link_flow() {
        let assets = hydra_assets();
        let names: Vec<_> = suggest_assets(&assets, "amd64")
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names[0], "hydralauncher_4.4.0_amd64.deb");
        assert!(names.contains(&"hydralauncher-4.4.0.AppImage"));
        assert!(!names
            .iter()
            .any(|n| n.ends_with(".exe") || n.ends_with(".rpm") || n.ends_with(".snap")));
        assert_eq!(
            pattern_from_asset("hydralauncher_4.4.0_amd64.deb", "4.4.0"),
            "hydralauncher_{version}_amd64.deb"
        );
        assert_eq!(pattern_from_asset("tool.deb", "9.9"), "tool.deb");
    }

    #[test]
    fn finds_checksum_sources_in_order_of_preference() {
        let a = vec![asset("t.deb"), asset("SHA256SUMS"), asset("t.deb.sha256")];
        assert_eq!(
            find_checksum_source(&a, "t.deb").expect("c").kind,
            ChecksumKind::Sidecar
        );
        let b = vec![
            asset("t.deb"),
            asset("checksums.txt"),
            asset("latest-linux.yml"),
        ];
        assert_eq!(
            find_checksum_source(&b, "t.deb").expect("c").kind,
            ChecksumKind::SumsFile
        );
        // Hydra: only the electron-builder file.
        let hydra = hydra_assets();
        let c = find_checksum_source(&hydra, "hydralauncher_4.4.0_amd64.deb").expect("c");
        assert_eq!(
            (c.kind, c.asset.name.as_str()),
            (ChecksumKind::ElectronYml, "latest-linux.yml")
        );
        assert!(find_checksum_source(&[asset("t.deb")], "t.deb").is_none());
    }

    #[test]
    fn parses_sums_files_in_both_notations() {
        let text = include_str!("../tests/fixtures/SHA256SUMS");
        let d = parse_checksum(ChecksumKind::SumsFile, text, "tool_1.2.0_amd64.deb")
            .expect("text mode");
        assert!(matches!(d, Digest::Sha256(b) if b[0] == 0xe3 && b[31] == 0x55));
        assert!(parse_checksum(ChecksumKind::SumsFile, text, "tool_1.2.0_arm64.deb").is_some()); // `*name` binary mode
        assert!(parse_checksum(ChecksumKind::SumsFile, text, "other.deb").is_none());
    }

    #[test]
    fn sidecar_must_match_its_file_and_be_a_real_hash() {
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(parse_checksum(ChecksumKind::Sidecar, h, "t.deb").is_some());
        assert!(parse_checksum(ChecksumKind::Sidecar, &format!("{h}  t.deb\n"), "t.deb").is_some());
        assert!(
            parse_checksum(ChecksumKind::Sidecar, &format!("{h}  other.deb\n"), "t.deb").is_none()
        );
        for bad in ["", "zz", &h[..63], &format!("{h}0"), &h.replace('e', "g")] {
            assert!(
                parse_checksum(ChecksumKind::Sidecar, bad, "t.deb").is_none(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parses_the_real_hydra_latest_linux_yml() {
        let text = include_str!("../tests/fixtures/hydra-latest-linux.yml");
        let d = parse_checksum(
            ChecksumKind::ElectronYml,
            text,
            "hydralauncher_4.4.0_amd64.deb",
        )
        .expect("deb");
        // First bytes of base64 `12Auts+k…` decoded.
        assert!(matches!(d, Digest::Sha512(b) if b[0] == 0xd7 && b[1] == 0x60));
        assert!(parse_checksum(
            ChecksumKind::ElectronYml,
            text,
            "hydralauncher-4.4.0.AppImage"
        )
        .is_some());
        assert!(parse_checksum(ChecksumKind::ElectronYml, text, "missing.deb").is_none());
    }

    #[test]
    fn checksum_parsers_never_panic_on_garbage() {
        for kind in [
            ChecksumKind::Sidecar,
            ChecksumKind::SumsFile,
            ChecksumKind::ElectronYml,
        ] {
            for text in [
                "",
                "\u{0}",
                "- url:",
                "sha512:",
                "- url: a\n  sha512: !!!",
                "é é é",
                &"x".repeat(10_000),
            ] {
                let _ = parse_checksum(kind, text, "t.deb");
            }
        }
    }

    #[test]
    fn real_world_cpu_names_are_filtered_by_whole_tokens() {
        // Names seen on the GitHub CLI release.
        for other in [
            "gh_2.103.3_linux_386.deb",
            "gh_2.103.3_linux_armv6.deb",
            "gh_2.1_linux_arm64.deb",
            "t-1.0.i686.deb",
            "t_1.0_ppc64le.deb",
            "t_1.0_riscv64.deb",
        ] {
            assert!(is_for_other_arch(other, "amd64"), "{other}");
        }
        for mine in [
            "gh_2.103.3_linux_amd64.deb",
            "t-1.0.x86_64.deb",
            "t_x64_1.0.deb",
            "tool.deb",
            "tool_1.0.deb",
        ] {
            assert!(!is_for_other_arch(mine, "amd64"), "{mine}");
        }
        // A version that merely contains `386` or `arm` is not an architecture.
        assert!(!is_for_other_arch("tool_1.2386.0_amd64.deb", "amd64"));
        assert!(!is_for_other_arch("alarm-clock_1.0.deb", "amd64"));
        // On an ARM machine it is the other way round.
        assert!(is_for_other_arch("tool_1.0_amd64.deb", "arm64"));
        assert!(!is_for_other_arch("tool_1.0_aarch64.deb", "arm64"));
    }

    #[test]
    fn suggestions_prefer_the_regular_build_over_musl_and_name_the_cpu() {
        let a = vec![
            asset("bat-musl_0.26.1_musl-linux-amd64.deb"),
            asset("bat_0.26.1_amd64.deb"),
            asset("gh_2.1_linux_386.deb"),
            asset("gh_2.1_linux_amd64.deb"),
            asset("gh_2.1_linux_armv6.deb"),
        ];
        let names: Vec<_> = suggest_assets(&a, "amd64")
            .iter()
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "bat_0.26.1_amd64.deb",
                "gh_2.1_linux_amd64.deb",
                "bat-musl_0.26.1_musl-linux-amd64.deb"
            ]
        );
        // A file that names the CPU beats one that does not.
        let b = vec![asset("tool.deb"), asset("tool_amd64.deb")];
        assert_eq!(
            select_asset(&b, "*", "1", "amd64").expect("pick").name,
            "tool_amd64.deb"
        );
    }

    #[test]
    fn checksum_lists_with_a_version_prefix_are_recognized() {
        let a = vec![
            asset("gh_2.103.3_linux_amd64.deb"),
            asset("gh_2.103.3_checksums.txt"),
        ];
        let c = find_checksum_source(&a, "gh_2.103.3_linux_amd64.deb").expect("checksums");
        assert_eq!(
            (c.kind, c.asset.name.as_str()),
            (ChecksumKind::SumsFile, "gh_2.103.3_checksums.txt")
        );
        let b = vec![asset("t.deb"), asset("t_1.0_SHA256SUMS")];
        assert_eq!(
            find_checksum_source(&b, "t.deb").expect("sums").kind,
            ChecksumKind::SumsFile
        );
        // Unrelated files that merely contain a similar word do not count.
        assert!(
            find_checksum_source(&[asset("t.deb"), asset("checksum-tool.exe")], "t.deb").is_none()
        );
    }
}
