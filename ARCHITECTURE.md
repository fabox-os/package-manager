# Package Manager — Architecture

> Pure Rust update manager for Ubuntu, Debian derivatives and Debian-based distros.
> One GUI to update everything: APT, Flatpak, manually installed `.deb` packages, and GitHub-released apps like Hydra Launcher.
> UI language: English. Code, docs and commits: English.

## 1. Problem

On Ubuntu, `sudo apt update && sudo apt upgrade` only covers APT repositories.

It does NOT cover:

- Apps installed manually with `dpkg -i app.deb` / `apt install ./app.deb` where the `.deb` has no APT repo behind it.
- Apps installed from GitHub Releases, e.g. **Hydra Launcher** (`hydralauncher/hydra`).
- Flatpak apps, which live in a separate system.
- AppImages downloaded manually.

Result: the user has to remember to check GitHub, re-download `.deb` files, and run separate commands for each ecosystem. This project fixes that.

## 2. Goals

1. **Single source of truth:** one list, one `Update All` button.
2. **Detect out-of-date apps**, not just list installed apps.
3. **GitHub Bridge:** if an app came from GitHub, check its repo for a newer release and update from there.
4. **Show the app icon** for every entry, so the list is scannable.
5. **Minimalist, modern GUI** in English. No terminal required for daily use.
6. **Safe privilege handling:** GUI runs as user, privileged actions go through a small helper via `pkexec`.
7. **Pure Rust:** no Python, no shell-script core. Shell is only used to invoke system tools (`apt-get`, `flatpak`, `dpkg-query`) via `std::process::Command`.

## 3. Non-Goals (v1)

- Not a full package manager / store. No browsing, no installing arbitrary new apps in v1, only updating what is already installed.
- Not an Arch / Fedora / Nix manager. Debian-family only. Detect and refuse gracefully on non-Debian systems.
- No auto-update without consent by default. User approves every update.
- No Snap in v1. Snap is designed as Phase 5, behind a feature flag.

## 4. Tech Stack

| Layer | Choice | Why |
|---|---|---|
| Language | Rust 1.80+ / Edition 2021 | Single binary, safety, no runtime |
| GUI | `iced` 0.13+ | Pure Rust, declarative MVU, modern theming, good for minimalist UI |
| Async runtime | `tokio` | Concurrent source checks, non-blocking UI |
| HTTP | `reqwest` + `serde_json` | GitHub API |
| GitHub | `octocrab` or raw `reqwest` | Keep it thin, cache ETags |
| Version compare | `semver` + custom fallback | GitHub tags are not always clean semver |
| Local DB | `rusqlite` (SQLite) | Tracked apps, history, icon cache index |
| Config | `toml` + `directories` | `~/.config/package-manager/` |
| Icons | `freedesktop` desktop entry parsing + image cache | Resolve `.desktop` Icon field |
| Privilege | `pkexec` + small helper binary | GUI never runs as root |
| Logging | `tracing` + `tracing-subscriber` | Structured logs |
| Tests | `cargo test` + `mockall` / fake sources | No real `apt` calls in unit tests |

Alternative GUI considered: `egui`. Iced was chosen for a more polished, product-like layout and better theming control. If iteration speed becomes a blocker, `egui` is an acceptable swap, the core stays untouched.

## 5. High-Level Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  GUI (iced) — crates/app                                     │
│  Views: Dashboard, Updates List, App Detail, Settings        │
│  State: Elm / MVU — Model, Message, update(), view()         │
└───────────────────────▲──────────────────────▲───────────────┘
                        │ Messages             │ Progress events
                        │                      │
┌───────────────────────▼──────────────────────▼───────────────┐
│  Core — crates/core                                          │
│  Orchestrator: Scan -> Check -> Plan -> Apply -> Verify      │
│  Domain: InstalledApp, UpdateCandidate, UpdatePlan           │
│  Traits: UpdateSource, IconProvider, PrivilegedExecutor      │
└───┬────────────┬──────────────┬──────────────┬──────────────┘
    │            │              │              │
    ▼            ▼              ▼              ▼
┌────────┐ ┌───────────┐ ┌──────────────┐ ┌──────────────┐
│ APT    │ │ Flatpak   │ │ GitHub       │ │ Manual .deb  │
│ Source │ │ Source    │ │ Bridge       │ │ Tracker      │
└────────┘ └───────────┘ └──────────────┘ └──────────────┘
    │            │              │              │
    ▼            ▼              ▼              ▼
┌────────┐ ┌───────────┐ ┌──────────────┐ ┌──────────────┐
│ dpkg / │ │ flatpak   │ │ GitHub API   │ │ SQLite       │
│ apt-get│ │ CLI       │ │ Releases     │ │ tracked.json │
└────────┘ └───────────┘ └──────────────┘ └──────────────┘

Privileged path:
GUI -> pkexec -> crates/helper (tiny binary) -> apt-get / dpkg -i / flatpak (system)
```

### Crate Layout (Cargo Workspace)

```
package-manager/
  Cargo.toml              # workspace
  crates/
    app/                  # iced GUI binary, only UI + wiring
    core/                 # domain, orchestrator, traits, errors
    sources-apt/         # APT + dpkg
    sources-flatpak/     # Flatpak
    sources-github/       # GitHub Bridge
    icons/                # .desktop parsing, icon lookup, cache
    helper/               # privileged helper, invoked via pkexec
    config/               # config + DB migrations
    runtime/              # composition root: builds the orchestrator from concrete sources
  docs/
    ARCHITECTURE.md
    ROADMAP.md
    AGENTS.md
```

Rule: `app` never calls `apt-get` directly. It calls `core`, with sources wired in by `runtime`. `core` never parses UI state. Sources are swappable for tests.

## 6. Domain Model

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSource {
    Apt { package_name: String },
    Flatpak { app_id: String, remote: String },
    Github { repo: GithubRepo, asset_pattern: String },
    ManualDeb { package_name: String, linked_repo: Option<GithubRepo> },
    AppImage { path: PathBuf, linked_repo: Option<GithubRepo> },
}

#[derive(Debug, Clone)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String, // e.g. "hydralauncher/hydra"
}

#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub id: String,              // stable id: "apt:hydra" / "flatpak:com.example.App" / "github:hydralauncher/hydra"
    pub name: String,            // Display name from .desktop or package
    pub version: String,         // Installed version, raw string
    pub normalized_version: Option<semver::Version>,
    pub source: AppSource,
    pub provenance: Provenance,  // WHERE it came from, always shown in UI, see 12.7
    pub icon: IconRef,          // local cached path or theme name
    pub desktop_file: Option<PathBuf>,
    pub homepage: Option<String>,
    pub installed_at: Option<DateTime<Utc>>,
}

/// Provenance = the real origin of this app. Never guessed, never faked.
/// If we do not know, we say Unknown.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub kind: ProvenanceKind,
    pub display_name: String,   // e.g. "Flathub", "Ubuntu noble/main", "github.com/hydralauncher/hydra"
    pub detail_url: Option<String>, // repo URL, remote URL, or release URL, clickable in UI
    pub trust: TrustLevel,       // Official | Verified | Community | Unknown
    pub verified_at: Option<DateTime<Utc>>,
    pub signature: SignatureStatus, // Verified | NotChecked | Missing | Invalid
    pub also_available_from: Vec<Provenance>, // same app found in other sources
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceKind {
    AptRepo { repo: String, component: String, origin: String }, // e.g. repo="Ubuntu", component="noble/main"
    FlatpakRemote { remote: String, remote_url: String }, // e.g. remote="flathub", url="https://dl.flathub.org/repo/"
    Github { owner: String, repo: String, release_url: String },
    ManualDeb { reason: String }, // "No APT origin found"
    AppImage { path: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustLevel {
    Official,  // from distro repo, Flathub verified, or official GitHub org
    Verified,  // GitHub repo linked by user + checksum verified, or Flathub
    Community, // PPA, third-party Flatpak remote, community GitHub fork
    Unknown,   // manual .deb with no origin, cannot verify
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    Verified,
    NotChecked,
    Missing, // e.g. GitHub release has no checksum
    Invalid,
}

#[derive(Debug, Clone)]
pub struct UpdateCandidate {
    pub app: InstalledApp,
    pub latest_version: String,
    pub changelog_url: Option<String>,
    pub download_url: Option<String>,
    pub size_bytes: Option<u64>,
    pub is_security: bool,
    pub source_kind: UpdateKind, // Apt / Flatpak / Github
}

#[derive(Debug, Clone)]
pub enum IconRef {
    Theme(String),
    File(PathBuf),
    Url(String), // only before download, never rendered directly
    Initial(char),
}
```

## 7. Core Trait: UpdateSource

Every ecosystem implements the same contract. The orchestrator does not care where an update comes from.

```rust
#[async_trait::async_trait]
pub trait UpdateSource: Send + Sync {
    fn kind(&self) -> &'static str; // "apt", "flatpak", "github"
    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError>;
    async fn check_updates(&self, apps: &[InstalledApp]) -> Result<Vec<UpdateCandidate>, SourceError>;
    async fn apply(&self, candidates: &[UpdateCandidate], progress: ProgressSender) -> Result<ApplyReport, SourceError>;
}
```

Orchestrator flow:

1. `scan_all()` — run all sources in parallel.
2. `deduplicate()` — same app can appear in two sources, merge provenance into `also_available_from`, e.g. Hydra as APT and GitHub. Priority: GitHub > ManualDeb > Apt > Flatpak. Keep one canonical entry, merge metadata.
3. `check_all()` — for each app, ask its source for latest version.
4. `plan()` — build an ordered `UpdatePlan`. APT first, then Flatpak, then GitHub `.deb` installs. This avoids `dpkg` lock conflicts.
5. `apply()` — stream progress + log events to GUI via `LogBroadcaster`, verify after each step.
6. `verify()` — re-scan version, compare to expected.

## 8. Source 1 — APT / dpkg

Responsibility: everything APT knows about.

**Scan:**
- `dpkg-query -W -f=${Package}\t${Version}\t${Status}\n` for installed packages.
- `/var/lib/dpkg/status` as fallback if `dpkg-query` is unavailable.
- Join with `.desktop` files in `/usr/share/applications/` to get display name and icon. Only packages with a `.desktop` file get an icon + friendly name. Others are grouped under `System Packages` and collapsed by default.

**Check:**
- Run `apt-get update` (via helper, cached, max once per 30 min unless forced).
- Parse `apt list --upgradable` or `apt-get -s dist-upgrade` for candidates. Prefer machine-readable output, never parse human `apt` progress bars.
- `apt-cache show <pkg>` for changelog / homepage if needed.

**Apply:**
- `pkexec helper apt-upgrade <pkgs>` for both single updates and `Update All` (the full list of planned packages; no blanket `dist-upgrade`).
- Stream `dpkg` progress via stdout parsing in helper, forward as `Progress { app_id, percent, status }`.

**Edge cases:**
- APT lock held by another process -> show friendly error, offer Retry.
- Held packages / phased updates -> surface as `Held` badge, do not force.
- Distro detection: read `/etc/os-release`, require `ID_LIKE` contains `debian` or `ID` in `[debian, ubuntu, linuxmint, pop, zorin, elementary]`. Otherwise show unsupported screen.

## 9. Source 2 — Flatpak

Responsibility: user + system Flatpaks.

**Scan:**
- `flatpak list --app --columns=application,version,origin,ref` for user and system.
- Join with `~/.local/share/flatpak/exports/share/applications/*.desktop` and `/var/lib/flatpak/exports/share/applications/*.desktop` for name + icon.

**Check:**
- `flatpak remote-ls --updates --app` — lists apps with updates.
- Alternative: `flatpak update --appstream` then compare versions.

**Apply:**
- User remotes: `flatpak update -y <app-id>` without root.
- System remotes: via helper if needed.
- Progress: Flatpak has no stable percent API, show indeterminate + log lines.

## 10. Source 3 — GitHub Bridge (The Key Feature)

This is what fixes Hydra and other GitHub-distributed `.deb` apps.

### 10.1 Problem it solves

User installs:
```bash
wget https://github.com/hydralauncher/hydra/releases/download/v2.0.0/hydra_2.0.0_amd64.deb
sudo dpkg -i hydra_2.0.0_amd64.deb
```
APT has no repo for it, so it never updates. The Bridge remembers where it came from.

### 10.2 Tracked App Registry

Stored in SQLite: `tracked_apps` table.

| Column | Example |
|---|---|
| `id` | `github:hydralauncher/hydra` |
| `package_name` | `hydralauncher` |
| `github_repo` | `hydralauncher/hydra` |
| `asset_pattern` | `hydralauncher_{version}_amd64.deb` |
| `installed_version` | `2.0.0` |
| `last_checked_at` | timestamp |
| `auto_track` | `true` |

**How apps get tracked:**
1. **Auto-discovery (v2):** On first scan, if a `.deb` package version matches a GitHub release tag pattern and the package has no APT repo (`apt-cache policy <pkg>` shows no origin), suggest linking it. User confirms repo.
2. **Manual linking (v1):** User opens `Add from GitHub` and pastes `owner/repo` or any github.com releases link (`https://github.com/o/r/releases/tag/v1` works too). The app fetches the latest release, keeps only `.deb` files for this CPU (whole-token architecture matching; the regular build ranks above `-musl`), and shows repo, tag, file, size, checksum status and trust before anything happens. The user then chooses `Install` (link + the normal verified install) or `Link only`. The package name is guessed from the file name (`name_version_arch.deb`), editable, and enforced by the installer, which refuses a `.deb` whose real name differs.
3. **First-class presets (v1):** Ship a small `presets.json` with known apps: Hydra, and a few popular GitHub `.deb` tools. If package name matches, auto-suggest the preset.

### 10.3 Check Flow

```
For each tracked app:
  GET https://api.github.com/repos/{owner}/{repo}/releases/latest
  Headers: Accept: application/vnd.github+json, If-None-Match: <cached ETag>
  If 304 -> no change, use cache
  Else parse: tag_name (v2.1.3), assets[], body (changelog), html_url
  Normalize tag: strip leading 'v', try semver parse, fallback to loose compare
  If latest > installed -> UpdateCandidate
  Pick asset: match asset_pattern against asset.name
    Pattern examples:
      - "hydralauncher_{version}_amd64.deb"
      - "*.AppImage"
      - "*_amd64.deb" (generic)
    Prefer .deb on Debian, then AppImage, then .tar.gz
  Verify: if release provides .sha256 / checksums.txt / latest-linux.yml, download and verify
```

**Rate limits:**
- Unauthenticated: 60 req/hour. Cache aggressively, batch checks, use `ETag` + `If-Modified-Since`.
- Optional `GITHUB_TOKEN` in Settings (stored in keyring via `keyring` crate, never in plain config). With token: 5000 req/hour.
- Show `Rate limit` state in UI if 403, with countdown.

**Version comparison:**
- Try `semver::Version::parse(cleaned)`.
- If fails, fallback: split on `.` / `-`, compare numeric segments, then lexicographic.
- Never mark as update if versions are equal after normalization (`v2.0.0` == `2.0.0`).

### 10.4 Apply Flow

```
1. Re-derive the download from the (cached) release; it must equal what the user was shown
2. Fetch the maintainer's checksum file. If one exists, verification is mandatory
3. Download to ~/.cache/package-manager/downloads/<package>/<version>/<file>.deb
   - HTTPS only, redirects only to github.com / *.githubusercontent.com
   - resumes from a .part file with an HTTP Range request; size must match the release
4. Verify: size, Debian `ar` magic bytes, checksum. Any failure deletes the file
5. Install via helper: pkexec helper install-deb <path> <package> <sha256|sha512> <hex>
6. Verify: dpkg-query -W <pkg> is at least the expected version
7. Cleanup: keep the 2 newest version folders
```

**What the helper does for `install-deb`** (it runs as root and treats its caller as hostile):
1. The path must be absolute, resolve (after symlinks and `..`) to a regular `.deb` inside `<caller's home>/.cache/package-manager/downloads`, and be owned by the caller. The caller is `PKEXEC_UID`, never an argument; their home comes from `/etc/passwd`.
2. The open file is the one that was checked (same device and inode), then it is **copied into a new root-only directory under `/var/lib/package-manager-helper` (on disk, root-writable only; `/run` is RAM) while being hashed**. The hash must equal the one the app verified, so the user cannot swap the file between the checks and the install.
3. The copy must start with the `.deb` magic, and `dpkg-deb -f` must report exactly the approved package name.
4. `dpkg --force-confold --force-confdef -i <copy>`; if that fails, `apt-get install -f -y`.

The package name comes from the user's own link in the database, not from the release or the UI.

**Checksum sources** (first match wins): `<file>.sha256`, `SHA256SUMS` / `checksums.txt`, then electron-builder's `latest-linux.yml` (base64 SHA-512 per file). Hydra publishes only the last kind. Checked against the real Hydra v4.4.0 release.

**Rate limits and caching:** responses are cached on disk with their ETag and revalidated with `If-None-Match` (a 304 costs no quota). After a rate-limit answer no further request is sent until the reset time; the UI shows the countdown. Offline with a cached release still answers.

**Safety:**
- Release pages and downloads must be `https://github.com/...` exactly; anything else is ignored.
- Never install from a repo the user did not explicitly link.
- Show repo, tag, asset name, size, and changelog link before install.
- If checksum file exists, verification is mandatory. If not, show `No checksum provided by maintainer` warning, still allow with explicit confirm.
- Download over HTTPS only.

## 11. Source 4 — Manual .deb Tracker

For `.deb` apps with no GitHub link yet.

- They appear in the list with badge `Manual .deb` and action `Link to GitHub`.
- If user does not link, they are shown as `Up to date (unknown)` — never falsely claim they are up to date.
- Once linked, they move to GitHub Bridge and get real update checks.

## 12. Icon Service

Goal: every row has an icon. No generic package icon if we can avoid it.

Lookup order:

1. `.desktop` file `Icon=` field.
   - If absolute path, use it.
   - If theme name, search: `/usr/share/icons/hicolor/*/apps/`, `/usr/share/pixmaps/`, `~/.local/share/icons/`, Flatpak export dirs.
2. Flatpak: `~/.local/share/flatpak/exports/share/icons/` + system equivalent.
3. GitHub: if app is GitHub-linked, fetch `owner avatar` or repo `social preview` as fallback, cache it. Only as last resort.
4. Fallback: initial letter in a colored rounded square, generated in GUI.

**Cache:** `~/.cache/package-manager/icons/<hash>.png`, max 200 MB, LRU cleanup. Store resolved path in SQLite to avoid re-scanning on every launch.

**Rendering in Iced:** load via `iced::widget::image::Handle::from_path`, async load, show placeholder while loading. Never block UI thread on icon I/O.


## 12.5. Terminal Panel — "Show terminal" Checkbox

A checkbox in the footer / toolbar: `[x] Show terminal`.

When checked, a small embedded terminal panel appears docked under the app list. When unchecked, it collapses. It is not a separate window. It shows, live, what is being updated and what the system is doing, like a normal terminal update.

### What it shows

- The exact command being run, with a `$` prefix, e.g. `$ sudo apt-get update`, `$ flatpak update -y`, `$ dpkg -i hydra_2.1.0_amd64.deb`
- Live stdout / stderr from the underlying tools, streamed line by line, not only at the end.
- Per-app context lines, e.g. `[Hydra] Downloading hydra_2.1.0_amd64.deb (68 MB) 45%`, `[APT] Updating VLC 3.0.20 -> 3.0.21...`, `[Flatpak] Updating Firefox...`
- Status lines: `Checking...`, `Done`, `Failed: APT is locked`, `Verified: Hydra 2.1.0`
- Timestamps are optional and off by default to keep it looking like a terminal.

### What it is NOT

- v1 is a **read-only log viewer styled as a terminal**, not a full interactive PTY. User cannot type commands into it. That keeps it safe and simple.
- v2 (optional, Phase 5) can add a real PTY via `portable-pty` if interactive input is ever needed. Do not block v1 on PTY.

### Data model

```rust
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub at: DateTime<Utc>,
    pub source: LogSource, // Apt | Flatpak | Github | Helper | Core
    pub app_id: Option<String>, // e.g. "github:hydralauncher/hydra"
    pub kind: LogKind, // Command | Stdout | Stderr | Status | Progress | Error
    pub line: String, // already sanitized, no secrets
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Log(LogEvent),
    Progress { app_id: String, percent: Option<u8>, status: String },
}
```

### Streaming architecture

```
apt-get / flatpak / dpkg (child process)
  -> stdout/stderr piped, line-buffered, flushed
  -> helper (if privileged) forwards lines over its stdout with a prefix
  -> core: LogBroadcaster (tokio broadcast channel, capacity 1000)
  -> app: iced subscription, appends to Vec<LogEvent>, max 2000 lines, auto-scroll
```

Rules:

- **Never block UI:** All process output is read on a background tokio task, sent via channel.
- **Line buffering:** Helper and sources must flush after every line. `pkexec` can buffer, so helper writes with explicit flush and uses `std::io::stdout().flush()` after each line.
- **Sanitization:** Strip tokens, passwords, keyring values, and home path secrets before emitting a `LogEvent`. GitHub token must never appear. URLs with `?token=` are redacted to `?token=[redacted]`.
- **No secrets in logs:** Log file on disk uses the same sanitized lines as the UI.
- **Backpressure:** If UI is slow, drop oldest lines, keep last 2000. Never grow unbounded.
- **Persistence:** Persist last run log to `~/.local/share/package-manager/last-run.log`, max 2 MB, rotated.
- **Copy / Clear:** Buttons in panel header: `Copy` copies all visible lines to clipboard, `Clear` clears in-memory buffer, `Expand` toggles panel height (small 180px / large 320px / collapsed).

### Config

```toml
[ui]
show_terminal = false      # checkbox state, persisted
terminal_auto_show = true  # auto-open panel when an update starts, even if checkbox was off
terminal_max_lines = 2000
terminal_font_size = 12
```

Behavior:

- If `show_terminal` is off and user clicks `Update All`, and `terminal_auto_show` is true, panel auto-opens so user sees progress.
- User can uncheck at any time, panel collapses but logging continues in background.
- Checkbox state is persisted across restarts.

### Visual design

- Monospace font: `JetBrains Mono` / `Fira Code` / system monospace fallback, 12px.
- Dark terminal background `#0F1115` even in light theme, light gray text `#E5E7EB`, green for success, yellow for warnings, red for errors, dim gray for command echo.
- Auto-scroll to bottom on new line, unless user scrolled up manually. If user scrolled up, show a `Jump to bottom` pill.
- Search inside terminal is Phase 3: `Ctrl+F` inside panel filters lines.

### Example session

```
$ sudo apt-get update
Hit:1 http://archive.ubuntu.com/ubuntu noble InRelease
Reading package lists... Done
$ sudo apt-get dist-upgrade -y
[APT] Updating VLC 3.0.20 -> 3.0.21...
Setting up vlc (3.0.21)...
[APT] VLC updated OK
[Hydra] Checking hydralauncher/hydra...
[Hydra] Downloading hydra_2.1.0_amd64.deb (68 MB) 45%
[Hydra] Downloading hydra_2.1.0_amd64.deb (68 MB) 100%
$ sudo dpkg -i hydra_2.1.0_amd64.deb
[Hydra] Verified: Hydra 2.1.0
All updates done.
```


## 12.6. Theme System — Customize Everything via JSON + Theme Editor

The updater should not be boring. Every visual part is themable via a plain `.json` file. No Rust code needed to make a new look. Share a JSON file, get a new theme.

### Where themes live

```
~/.config/package-manager/
  config.toml
  themes/
    my-theme.json          # user themes
    midnight-neon.json
  themes.json              # optional index, auto-generated
```

Built-in themes ship with the app: `Default Light`, `Default Dark`, `Ubuntu Orange`, `Midnight Neon`, `Dracula`.

User picks a theme in `Settings > Appearance`. App watches the `themes/` folder and hot-reloads on save, so editing JSON in VS Code updates the UI instantly.

### Theme JSON Schema (v1)

One file = one complete theme. All fields optional, missing fields fall back to Default.

```json
{
  "name": "Midnight Neon",
  "author": "you",
  "version": "1",
  "mode": "dark",
  "description": "Dark + purple neon accent, big radius, cozy list.",
  "colors": {
    "background": "#0F1115",
    "surface": "#1A1D23",
    "surfaceHover": "#23272F",
    "text": "#E5E7EB",
    "textMuted": "#9CA3AF",
    "accent": "#A78BFA",
    "accentText": "#FFFFFF",
    "success": "#22C55E",
    "warning": "#F59E0B",
    "error": "#EF4444",
    "border": "#2E333D"
  },
  "terminal": {
    "background": "#0A0A0F",
    "text": "#E5E7EB",
    "command": "#8B9CC7",
    "success": "#22C55E",
    "warning": "#F59E0B",
    "error": "#EF4444"
  },
  "typography": {
    "fontFamily": "Inter, sans-serif",
    "fontSize": 14,
    "monoFontFamily": "JetBrains Mono, monospace",
    "monoFontSize": 12,
    "headingWeight": 700
  },
  "shape": {
    "borderRadius": 14,
    "cardRadius": 16,
    "buttonRadius": 10,
    "spacing": 16
  },
  "layout": {
    "density": "comfortable",
    "iconSize": 48,
    "showIcons": true,
    "showBadges": true,
    "compactList": false
  },
  "effects": {
    "shadow": true,
    "transparency": 0.0,
    "animations": true,
    "glowAccent": false,
    "backgroundImage": null
  }
}
```

Field notes:

- `mode`: `light` | `dark` | `system`. Used for OS-level contrast and for default terminal colors if `terminal` is missing.
- `density`: `compact` | `comfortable` | `cozy`. Controls row height, padding, icon size scale. `compact` = 32px icon, tight rows. `cozy` = 56px icon, airy.
- `effects.backgroundImage`: optional path or URL to a subtle background image. Blurred + dimmed automatically, never blocks text. `null` = solid color.
- `effects.transparency`: 0.0 to 0.85, window opacity. Kept low by default for readability.
- `effects.glowAccent`: if true, accent buttons get a soft glow. Fun, but off by default.

### Theme Engine

```
themes/*.json
  -> ThemeLoader (serde_json, validation)
  -> Theme (resolved struct, defaults filled)
  -> iced Theme + custom Palette
  -> GUI re-renders, no restart
```

Rules:

- **Never crash on bad theme.** If JSON is invalid, show a toast `Theme "X" is invalid, using Default. See log for line.` and fall back to Default. Log the exact serde error.
- **Validate ranges:** colors must be `#RRGGBB` or `#RRGGBBAA`, `fontSize` 10-20, `borderRadius` 0-28, `transparency` 0.0-0.85, `iconSize` 24-64. Out-of-range values are clamped, not rejected.
- **Hot reload:** File watcher on `themes/` folder. On save, reload in <200ms.
- **No code execution:** Theme JSON is data only. No scripts, no commands, no paths outside allowed dirs. `backgroundImage` must be inside `~/.config/package-manager/` or `~/Pictures/` and must be an image file.

### Theme Editor (in-app, no JSON needed)

`Settings > Appearance > Theme Editor` — for people who do not want to edit JSON by hand.

Layout: left = controls, right = live preview (fake app list + mini terminal).

Editor sections:

- **Colors:** color pickers for Background, Surface, Text, Accent, Success, Warning, Error. Preset swatches: Ubuntu Orange, Dracula, Nord, Catppuccin, Neon.
- **Terminal:** separate pickers for terminal background / text / command color.
- **Typography:** dropdown for font family (system fonts), slider for font size 10-20, mono font for terminal.
- **Shape & Layout:** sliders for border radius, spacing, icon size. Toggle for density (Compact / Comfortable / Cozy), toggles for Show icons, Show badges, Glow, Animations, Shadow.
- **Background:** picker for solid color or image (blur + opacity slider).
- **Actions:** `Save as...`, `Duplicate`, `Export JSON`, `Import JSON`, `Reset to Default`, `Copy to clipboard`.

Editor behavior:

- Every change updates the live preview instantly, without saving.
- `Save` writes to `themes/<name>.json` and sets it as active theme.
- `Export` downloads / copies the JSON so user can share it on GitHub / Discord.
- `Import` validates, shows preview, asks `Use this theme?` before applying.

### Example: what a user can do

- Make it look like Ubuntu: orange accent `#E95420`, radius 8, compact.
- Make it look like a hacker terminal: black background, green accent `#00FF88`, mono font everywhere, glow on.
- Make it cozy: pastel accent, big radius 20, cozy density, background image of their wallpaper, blurred.

All from one JSON file or from the editor, no rebuild.


## 12.7. Provenance — "Where Is This App Coming From?"

Every app must show where it is coming from and where its update will come from. No silent updates from an unknown place. The user should be able to answer in one glance: is this from Ubuntu, from Flathub, from GitHub, or from nowhere I can verify?

### UI — List Row

Each row shows two lines, not just a badge:

```
[Icon] Hydra Launcher                      [GitHub] [Verified]
       2.0.0 -> 2.1.0  •  from github.com/hydralauncher/hydra
       [Update]

[Icon] Firefox                             [Flatpak] [Official]
       128 -> 129  •  from Flathub (https://dl.flathub.org)
       [Update]

[Icon] VLC                                 [APT] [Official]
       3.0.20 -> 3.0.21  •  from Ubuntu noble/main
       [Update]

[Icon] Some Random Tool                    [Manual .deb] [Unknown]
       1.0.0  •  from Unknown — manual .deb, no repo found
       [Link to GitHub] [Update]
```

- **Badge:** source kind, e.g. `[APT]`, `[Flatpak]`, `[GitHub]`, `[Manual .deb]`
- **Trust pill:** `[Official]`, `[Verified]`, `[Community]`, `[Unknown]` with color: green for Official/Verified, yellow for Community, red/gray for Unknown.
- **Origin line:** `from <display_name>` — always visible under version. Truncated with tooltip for long URLs.

### UI — App Detail Panel

Full provenance card, clickable:

```
Origin
  Source:        GitHub
  From:          github.com/hydralauncher/hydra
  Repo URL:      https://github.com/hydralauncher/hydra  [Open]
  Release:       https://github.com/hydralauncher/hydra/releases/tag/v2.1.0  [Open]
  Trust:         Verified — checksum verified, repo linked by you
  Signature:     Verified (SHA256)
  Last checked:  2 min ago

Also available from:
  • Ubuntu repo (APT) — version 1.9.0 (older)  [Switch source]
  • Flathub (Flatpak) — not available
```

For APT:

```
  Source:        APT
  From:          Ubuntu noble/main
  Origin:        Ubuntu (http://archive.ubuntu.com/ubuntu)
  Trust:         Official
```

For Flatpak:

```
  Source:        Flatpak
  From:          Flathub
  Remote URL:    https://dl.flathub.org/repo/
  Trust:         Official / Verified (Flathub verified app)
```

For Manual .deb:

```
  Source:        Manual .deb
  From:          Unknown — manual .deb, no repo found
  Trust:         Unknown
  Action:        [Link to GitHub] to verify origin
```

### How provenance is detected

**APT:**
- `apt-cache policy <pkg>` → parse `500 http://archive.ubuntu.com/ubuntu noble/main` lines. That gives origin + component.
- `/etc/apt/sources.list.d/*.list` + `apt-cache policy` for PPA detection: if origin is `LP-PPA-...`, mark as `Community`, show PPA name.
- If no origin line, mark as `ManualDeb / Unknown`.

**Flatpak:**
- `flatpak list --app --columns=application,origin` → remote name.
- `flatpak remote-list --show-details` → remote URL, e.g. Flathub `https://dl.flathub.org/repo/`.
- Flathub verified apps: check Flathub API or `flatpak remote-info` metadata for `verified` flag if available. If not available, default to `Verified` for Flathub, `Community` for other remotes.

**GitHub:**
- From tracked registry: `owner/repo` is known, `release_url` from API.
- Trust: `Verified` if checksum verified OR repo is in presets (e.g. Hydra). `Community` if user linked a fork / unofficial repo. Show warning if repo is not the official one.
- Show full repo URL, never just "GitHub". User must be able to click and see the repo.

**Manual .deb:**
- `apt-cache policy` shows no origin → `Unknown`. Never claim it is from Ubuntu or GitHub.
- After user links to GitHub, provenance upgrades from `Unknown` to `GitHub / Verified` (if checksum passes) or `GitHub / Community`.

### Multi-source conflicts

Same app can exist in multiple places, e.g. VLC in APT + Flathub + GitHub.

- Orchestrator keeps one canonical entry (priority: user-linked GitHub > Flatpak > APT, unless user set a preference).
- Detail panel shows `Also available from:` with versions for each source.
- User can set `Preferred source` per app in Detail: `Prefer Flatpak for Firefox`. Stored in DB.
- Update always uses the canonical source's provenance. Terminal panel also shows it: `[Update] Using Flatpak / Flathub for Firefox`.

### Verification & Safety

- Never show a fake origin. If we cannot prove it, show `Unknown`.
- If update URL domain does not match provenance URL domain, block and show error: `Update URL does not match origin. Expected github.com, got evil.com`.
- For GitHub, verify checksum if provided. If checksum missing, `Signature: Missing` + warning in Detail, still allow with confirm.
- For APT / Flatpak, rely on system GPG / OSTree verification, surface `Signature: Verified` only if system reports success. Do not invent it.
- Log provenance in `history` table: what was updated, from where, to which version.

### Terminal panel integration

Terminal also shows provenance on every update:

```
$ flatpak update -y org.mozilla.firefox
[Flatpak • Flathub] Updating Firefox 128 -> 129...
[GitHub • hydralauncher/hydra] Downloading hydra_2.1.0_amd64.deb...
[APT • Ubuntu noble/main] Updating VLC...
```

## 13. GUI — Minimalist Modern UI (English)

**Design principles:**
- One screen for 95% of use. No tabs maze.
- Left: summary + filters. Right: list.
- Large `Update All` button, always visible when updates exist.
- Every row: icon (48px), name, current -> latest, source badge, size, per-app `Update` button.
- Empty state: `You're up to date` with checkmark, not an empty list.
- English copy throughout. Example strings: `Updates available`, `Up to date`, `Checking for updates...`, `Link to GitHub`, `View changelog`.

**Layout (Iced):**

```
+--------------------------------------------------------------+
|  Package Manager        [Search...]      [Check] [Update All] |
|  3 updates available • Last checked 2 min ago                |
+--------------------------------------------------------------+
| [All] [APT] [Flatpak] [GitHub] [Manual]    [ ] Show system   |
+--------------------------------------------------------------+
| [Icon] Hydra Launcher        2.0.0 -> 2.1.0   [GitHub] [Verified] | 
|        from github.com/hydralauncher/hydra              [Update] |
| [Icon] Firefox (Flatpak)     128 -> 129        [Flatpak] [Update] |
| [Icon] VLC                   3.0.20 -> 3.0.21  [APT] [Update] |
+--------------------------------------------------------------+
| System packages (12) — collapsed by default                  |
+--------------------------------------------------------------+
| [x] Show terminal                        [Copy] [Clear] [^]  |
| +----------------------------------------------------------+ |
| | $ sudo apt-get update                                  | |
| | Hit:1 http://archive.ubuntu.com noble InRelease         | |
| | [APT] Updating VLC 3.0.20 -> 3.0.21 ...                 | |
| | [Hydra] Downloading hydra_2.1.0_amd64.deb (68 MB) 45%  | |
| +----------------------------------------------------------+ |
+--------------------------------------------------------------+
```

**Screens:**
- **Dashboard:** list above.
- **App Detail:** modal / side panel with changelog, repo link, asset name, size, `Update` + `Link to GitHub` + `Ignore this version`.
- **Settings:** GitHub token, check interval, auto-check on launch, show system packages, cache clear, presets.

**State (MVU):**

```rust
pub enum Message {
    ScanRequested,
    ScanFinished(Vec<InstalledApp>),
    CheckFinished(Vec<UpdateCandidate>),
    UpdateAllRequested,
    UpdateOneRequested(String),
    Progress(ProgressEvent),
    LinkGithubRequested(String),
    SearchChanged(String),
    FilterChanged(Filter),
}
```

**Async:** All source calls run in `tokio::task::spawn`, results sent back via `iced::subscription` / channel. UI never blocks.

**Theming:** Light + Dark, follows system by default. Use `iced::Theme::Dark` / `Light`, custom palette: neutral grays, one accent (e.g. `#2563EB` or Ubuntu orange `#E95420` as optional accent). Rounded corners, 12px radius, 16px spacing.

## 14. Privilege Separation

- GUI runs as normal user.
- Helper binary `package-manager-helper` is tiny, auditable, does only:
  - `apt-update` -> `apt-get update`
  - `apt-upgrade <pkg>...` -> `apt-get install --only-upgrade -y -- <pkg>...` (never installs new packages; names validated against Debian policy, max 500)
  - `apt-fix` -> `apt-get install -f -y`
  - `install-deb <path> <package> <sha256|sha512> <hex>` -> verified copy, then `dpkg -i` (see 10.4)
  - `dpkg -i <path>` (path must be inside cache dir)
  - `flatpak update` for system remotes
- Helper is invoked via `pkexec` with explicit args. No shell interpolation, no `sh -c`.
- Helper validates: file is a regular `.deb` owned by the caller inside the caller's download folder, its checksum, and that the package name is the approved one (see 10.4).
- The helper clears its environment, uses absolute program paths and `--` before package names.
- All privileged actions are logged to the root-owned `/var/log/package-manager-helper.log` (not a user-writable path: a root process appending to a user-controlled file could be redirected with a symlink).

### Threat model and known limits

**What is defended**
- A malicious or confused release: downloads must be under `github.com/<owner>/<repo>/releases/download/` of the linked repo, over HTTPS, redirects only to GitHub's release CDN hosts; size, `.deb` format and the maintainer's checksum are verified; the package name inside the `.deb` must equal the one approved.
- Bugs or races in the GUI: the helper re-validates everything itself and installs a private, re-hashed copy, so swapping the file after the GUI's checks changes nothing.
- Hostile arguments: package names follow Debian policy, paths are absolute and resolved (and re-checked from `/proc/self/fd` after opening), and every log line is stripped of control characters so entries cannot be forged.
- The helper binary itself: release builds run only `/usr/libexec/package-manager/package-manager-helper`, and only if it and its folder are owned by root and not group/world-writable. `$UPDATE_MANAGER_HELPER` and "next to the executable" work in debug builds only. `pkexec` is started by absolute path.
- `packaging/install.sh` installs the release helper (root-owned), the polkit policy, the app (`/usr/local/bin/package-manager`) and a desktop entry; `uninstall` removes them.
- `data/polkit/org.packagemanager.helper.policy` requires an administrator password for every operation (no cached authorization).

**What is NOT defended (by design or not yet)**
- **The polkit prompt is the real security boundary.** The helper's checks protect against bugs and malicious releases, not against a program that already has administrator authorization. This is why the policy never caches it.
- **A `.deb` runs maintainer scripts as root.** Linking a repository means trusting its owner. A checksum published in the same release proves the file is intact, not who made it. Signature verification (OpenPGP / sigstore) is not implemented.
- A repository link replacing an installed distribution package needs an extra confirmation in the UI; the installer only guarantees the `.deb` really is the approved package name.
- `apt-upgrade` uses APT's own dependency resolution, which may install extra dependencies or remove conflicting packages.
- Users not listed in `/etc/passwd` (LDAP/SSSD) cannot install GitHub packages (fails closed). `XDG_CACHE_HOME` is ignored for the download folder because the helper only trusts `~/.cache/package-manager/downloads`.
- No GitHub token support yet, so the unauthenticated limit (60 requests/hour) applies.

## 15. Config & Storage

```
~/.config/package-manager/
  config.toml          # settings, github token ref, filters
  tracked.db           # SQLite: tracked_apps, history, icon index
~/.cache/package-manager/
  icons/
  downloads/
  github-cache/        # ETag + release JSON cache
~/.local/share/package-manager/
  logs/
```

`config.toml` (every key optional; missing keys use defaults, unknown keys are ignored):

```toml
[general]
show_system_packages = false

[ui]
show_terminal = true        # persisted when the checkbox is toggled
terminal_auto_show = true   # open the panel when an update starts

[sources]
apt = true
flatpak = true
```

An invalid file is moved to `config.toml.broken` and defaults are used, with a notice in the UI.
`tracked.db` is migrated with `PRAGMA user_version`; a database from a newer version is refused, not modified.
Today it holds the update `history` table (including each update's provenance). Planned keys (`[github]`, `check_interval_hours`, `theme`) are added when the feature that reads them lands.

## 16. Error Handling & Observability

- `thiserror` for typed errors per source: `AptError`, `FlatpakError`, `GithubError`.
- `anyhow` only at binary boundary.
- Every failure surfaces as a user-friendly message in UI, not a panic. Example: `APT is locked by another process. Try again in a moment.`
- `tracing` logs to file + optional `--verbose`.
- Telemetry: none. No phone-home, no analytics.

## 17. Testing Strategy

- **Unit:** version compare, asset pattern matching, icon lookup, dedup logic. Must pass without network / sudo.
- **Integration:** fake `UpdateSource` implementations, verify orchestrator plan ordering.
- **Golden tests:** parse real `dpkg-query` output fixtures, real GitHub release JSON fixtures (Hydra).
- **Manual QA:** VM with Ubuntu 24.04, install Hydra `.deb` manually, verify update flow end-to-end.
- **CI:** `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`, build `.deb` in GitHub Actions.

## 18. Build & Distribution

- `cargo build --release` -> single binary + helper binary.
- Packaging: `cargo-deb` for `.deb`, plus Flatpak manifest in Phase 5.
- Desktop file: `org.packagemanager.PackageManager.desktop` with icon.
- First run: onboarding wizard — 2 steps: `Detect installed apps` + `Link Hydra if found` + `Done`.

## 19. Future Extensions (Post-v1)

- Snap source (same trait).
- AppImage tracker: scan `~/Applications/*.AppImage`, link to GitHub.
- Background daemon + desktop notifications: `New updates available`.
- Scheduled checks via `systemd --user` timer.
- Rollback: keep previous `.deb` in cache, offer `Revert`.
- Export / import tracked apps list.

## 20. Open Decisions

- Iced vs egui — final decision after Phase 0 prototype (2-day spike).
- SQLite vs JSON for tracked apps — SQLite chosen, but JSON is acceptable for MVP if it unblocks.
- Helper via `pkexec` vs `polkit` DBus service — `pkexec` for MVP, DBus service if UX demands fewer prompts.

---
*Owner: Core team. Review this doc when adding a new source — it must implement `UpdateSource` and pass the orchestrator tests.*
