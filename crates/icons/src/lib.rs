//! `.desktop` parsing and icon lookup. Pure std, no UI.
//!
//! Icon paths come from files the user may not control (`Icon=/etc/passwd`), so a path is
//! only returned if it is an image inside a known icon folder.

use std::fs;
use std::path::{Path, PathBuf};

/// The parts of a desktop entry this app cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub icon: Option<String>,
    /// `NoDisplay=true` or `Hidden=true`: not meant to be shown in menus.
    pub hidden: bool,
}

/// Parses the `[Desktop Entry]` group. `None` unless it is a named `Type=Application`.
/// Localized keys (`Name[de]`) are ignored so the result does not depend on the locale.
pub fn parse_desktop(text: &str) -> Option<DesktopEntry> {
    let mut in_entry = false;
    let (mut kind, mut name, mut icon) = (None, None, None);
    let (mut no_display, mut hidden) = (false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Only the main group counts; stop at the next one (e.g. an Action group).
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => kind = Some(value.to_owned()),
            "Name" => name = Some(value.to_owned()),
            "Icon" if !value.is_empty() => icon = Some(value.to_owned()),
            "NoDisplay" => no_display = value == "true",
            "Hidden" => hidden = value == "true",
            _ => {}
        }
    }
    let name = name.filter(|n| !n.is_empty())?;
    (kind.as_deref() == Some("Application")).then_some(DesktopEntry {
        name,
        icon,
        hidden: no_display || hidden,
    })
}

pub fn read_desktop(path: &Path) -> Option<DesktopEntry> {
    // Desktop files are tiny; refuse anything huge rather than read it.
    if fs::metadata(path).ok()?.len() > 256 * 1024 {
        return None;
    }
    parse_desktop(&fs::read_to_string(path).ok()?)
}

/// Folders holding Flatpak-exported desktop files (system and user installations).
pub fn flatpak_desktop_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/var/lib/flatpak/exports/share/applications")];
    if let Some(h) = home {
        dirs.push(h.join(".local/share/flatpak/exports/share/applications"));
    }
    dirs
}

/// Finds `<app_id>.desktop` in the given folders.
pub fn find_desktop_file(app_id: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if !is_plain_name(app_id) {
        return None;
    }
    dirs.iter()
        .map(|d| d.join(format!("{app_id}.desktop")))
        .find(|p| p.is_file())
}

/// A bare file or icon name: no separators, no traversal, no control characters.
fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.contains(['/', '\\'])
        && !s.contains("..")
        && !s.chars().any(char::is_control)
}

const SIZES: [&str; 8] = [
    "48x48", "64x64", "128x128", "256x256", "512x512", "32x32", "96x96", "scalable",
];
/// `hicolor` is the shared fallback every app installs into; the others are the common
/// distro themes (Ubuntu ships its own app icons in Yaru).
const THEMES: [&str; 7] = [
    "hicolor", "Yaru", "Adwaita", "Humanity", "gnome", "breeze", "Papirus",
];
/// Generic icons (`package-x-generic`, `input-keyboard`) live outside `apps`.
const CATEGORIES: [&str; 8] = [
    "apps",
    "mimetypes",
    "categories",
    "devices",
    "places",
    "status",
    "actions",
    "legacy",
];
const EXTENSIONS: [&str; 2] = ["png", "svg"];

/// Looks up icon names and validates icon paths against a fixed set of roots.
#[derive(Debug, Clone)]
pub struct IconResolver {
    roots: Vec<PathBuf>,
}

impl IconResolver {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Standard system, user and Flatpak icon folders.
    pub fn system_default(home: Option<&Path>) -> Self {
        let mut roots = vec![
            PathBuf::from("/usr/share/icons"),
            PathBuf::from("/usr/share/pixmaps"),
            PathBuf::from("/var/lib/flatpak/exports/share/icons"),
        ];
        if let Some(h) = home {
            roots.push(h.join(".local/share/icons"));
            roots.push(h.join(".icons"));
            roots.push(h.join(".local/share/flatpak/exports/share/icons"));
        }
        Self::new(roots)
    }

    /// Resolves an `Icon=` value (a name or an absolute path) to an existing png/svg file.
    pub fn resolve(&self, icon: &str) -> Option<PathBuf> {
        if icon.starts_with('/') {
            self.validate_path(Path::new(icon))
        } else {
            self.lookup_name(icon)
        }
    }

    fn lookup_name(&self, name: &str) -> Option<PathBuf> {
        if !is_plain_name(name) {
            return None;
        }
        // Prefer mid-sized raster icons: sharp at 36 px and cheap to decode. Themes are tried
        // in a fixed order so results are predictable.
        for theme in THEMES {
            for size in SIZES {
                for category in CATEGORIES {
                    for root in &self.roots {
                        for ext in EXTENSIONS {
                            let p = root
                                .join(theme)
                                .join(size)
                                .join(category)
                                .join(format!("{name}.{ext}"));
                            if p.is_file() {
                                return Some(p);
                            }
                        }
                    }
                }
            }
        }
        // Flat folders such as /usr/share/pixmaps.
        for root in &self.roots {
            for ext in EXTENSIONS {
                let p = root.join(format!("{name}.{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// An absolute path is accepted only if, after resolving symlinks, it is a png/svg
    /// file inside one of the roots.
    fn validate_path(&self, path: &Path) -> Option<PathBuf> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if !EXTENSIONS.contains(&ext.as_str()) {
            return None;
        }
        let real = fs::canonicalize(path).ok()?;
        self.roots
            .iter()
            .filter_map(|r| fs::canonicalize(r).ok())
            .any(|r| real.starts_with(r))
            .then_some(real)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "[Desktop Entry]\nVersion=1.0\nName=Firefox Web Browser\nName[de]=Firefox Webbrowser\nIcon=firefox\nType=Application\n\n[Desktop Action new-window]\nName=New Window\nIcon=other\n";

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("um-icons-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir");
        d
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, b"x").expect("write");
    }

    #[test]
    fn parses_name_and_icon_ignoring_locales_and_action_groups() {
        let e = parse_desktop(FIREFOX).expect("entry");
        assert_eq!(e.name, "Firefox Web Browser");
        assert_eq!(e.icon.as_deref(), Some("firefox"));
        assert!(!e.hidden);
    }

    #[test]
    fn no_display_and_hidden_are_flagged() {
        let nd = parse_desktop("[Desktop Entry]\nType=Application\nName=X\nNoDisplay=true\n")
            .expect("e");
        let hd =
            parse_desktop("[Desktop Entry]\nType=Application\nName=X\nHidden=true\n").expect("e");
        assert!(nd.hidden && hd.hidden);
    }

    #[test]
    fn non_applications_and_nameless_entries_are_rejected() {
        assert!(parse_desktop("[Desktop Entry]\nType=Link\nName=X\n").is_none());
        assert!(parse_desktop("[Desktop Entry]\nType=Application\n").is_none());
        assert!(parse_desktop("[Desktop Entry]\nType=Application\nName=\n").is_none());
        assert!(parse_desktop("Name=X\nType=Application\n").is_none());
        assert!(parse_desktop("").is_none());
    }

    #[test]
    fn garbage_never_panics() {
        for s in [
            "[",
            "=",
            "[Desktop Entry]\n=\n==\n",
            "\u{0}\u{0}",
            "[Desktop Entry]\nName",
        ] {
            let _ = parse_desktop(s);
        }
    }

    #[test]
    fn resolves_theme_icons_preferring_mid_sizes() {
        let root = temp_dir("theme");
        touch(&root.join("hicolor/16x16/apps/foo.png"));
        touch(&root.join("hicolor/256x256/apps/foo.png"));
        touch(&root.join("hicolor/48x48/apps/foo.png"));
        let r = IconResolver::new(vec![root.clone()]);
        assert_eq!(
            r.resolve("foo"),
            Some(root.join("hicolor/48x48/apps/foo.png"))
        );
        assert_eq!(r.resolve("missing"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_distro_themes_and_prefers_hicolor() {
        let root = temp_dir("themes");
        touch(&root.join("Yaru/48x48/apps/gdebi.png"));
        touch(&root.join("Yaru/48x48/apps/both.png"));
        touch(&root.join("hicolor/48x48/apps/both.png"));
        touch(&root.join("Adwaita/scalable/apps/papers.svg"));
        let r = IconResolver::new(vec![root.clone()]);
        assert_eq!(
            r.resolve("gdebi"),
            Some(root.join("Yaru/48x48/apps/gdebi.png"))
        );
        assert_eq!(
            r.resolve("both"),
            Some(root.join("hicolor/48x48/apps/both.png"))
        );
        assert_eq!(
            r.resolve("papers"),
            Some(root.join("Adwaita/scalable/apps/papers.svg"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generic_icons_are_found_outside_the_apps_folder() {
        let root = temp_dir("generic");
        touch(&root.join("Yaru/48x48/mimetypes/package-x-generic.png"));
        let r = IconResolver::new(vec![root.clone()]);
        assert_eq!(
            r.resolve("package-x-generic"),
            Some(root.join("Yaru/48x48/mimetypes/package-x-generic.png"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_flat_pixmaps_and_svg() {
        let root = temp_dir("pixmaps");
        touch(&root.join("bar.svg"));
        let r = IconResolver::new(vec![root.clone()]);
        assert_eq!(r.resolve("bar"), Some(root.join("bar.svg")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn absolute_icon_paths_must_stay_inside_the_roots() {
        let root = temp_dir("abs");
        let outside = temp_dir("abs-outside");
        touch(&root.join("ok.png"));
        touch(&outside.join("evil.png"));
        let r = IconResolver::new(vec![root.clone()]);
        assert!(r
            .resolve(root.join("ok.png").to_str().expect("utf8"))
            .is_some());
        assert_eq!(
            r.resolve(outside.join("evil.png").to_str().expect("utf8")),
            None
        );
        assert_eq!(r.resolve("/etc/passwd"), None);
        // Traversal out of the root is caught after resolving the path.
        let sneaky = format!(
            "{}/../{}/evil.png",
            root.display(),
            outside.file_name().and_then(|n| n.to_str()).expect("name")
        );
        assert_eq!(r.resolve(&sneaky), None);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn symlink_escaping_the_root_is_rejected() {
        let root = temp_dir("link");
        let outside = temp_dir("link-outside");
        touch(&outside.join("secret.png"));
        std::os::unix::fs::symlink(outside.join("secret.png"), root.join("link.png"))
            .expect("symlink");
        let r = IconResolver::new(vec![root.clone()]);
        assert_eq!(
            r.resolve(root.join("link.png").to_str().expect("utf8")),
            None
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn unsafe_icon_names_and_extensions_are_refused() {
        let root = temp_dir("names");
        touch(&root.join("a.xpm"));
        let r = IconResolver::new(vec![root.clone()]);
        for bad in ["", "../x", "a/b", "..", "a\u{0}b", "a.xpm"] {
            assert_eq!(r.resolve(bad), None, "{bad:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_flatpak_desktop_files_by_id_only() {
        let dir = temp_dir("flatpak");
        touch(&dir.join("org.gimp.GIMP.desktop"));
        let dirs = vec![dir.clone()];
        assert_eq!(
            find_desktop_file("org.gimp.GIMP", &dirs),
            Some(dir.join("org.gimp.GIMP.desktop"))
        );
        assert_eq!(find_desktop_file("../org.gimp.GIMP", &dirs), None);
        assert_eq!(find_desktop_file("nope", &dirs), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
