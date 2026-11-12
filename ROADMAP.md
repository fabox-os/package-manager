# Package Manager — Roadmap

> Goal: One minimalist GUI in English that updates everything on Ubuntu / Debian-based distros — APT, Flatpak, manual `.deb` and GitHub apps like Hydra Launcher.
> Language: English for UI, code, docs and commits.

## Vision

A user installs Hydra from GitHub once. After that, they never have to think about it again. The Package Manager notices a new Hydra release, shows Hydra's icon, shows `2.0.0 -> 2.1.0`, and updates it with one click — same as any APT or Flatpak app.

## Milestones at a Glance

| Phase | Name | Outcome | Target |
|---|---|---|---|
| 0 | Foundation | Repo, workspace, GUI shell, CI | Week 1-2 |
| 1 | MVP — APT + Flatpak | List + Update All works for APT + Flatpak | Week 3-4 |
| 2 | GitHub Bridge | Hydra + any GitHub `.deb` updates from GitHub | Week 5-6 |
| 3 | Polish — Icons, Search, Detail | Feels like a real product | Week 7-8 |
| 4 | Background + Notifications | Checks automatically, notifies user | Week 9-10 |
| 5 | Packaging + Extras | `.deb` release, Snap, AppImage, rollback | Week 11-12 |

---

## Phase 0 — Foundation

**Goal:** You can run the app and see a shell, even if it does nothing yet.

### Tasks

- [ ] Init Cargo workspace: `app`, `core`, `sources-apt`, `sources-flatpak`, `sources-github`, `icons`, `helper`, `config`
- [ ] Choose GUI: spike Iced vs egui (2 days max), lock decision in ARCHITECTURE.md
- [ ] Implement domain models: `InstalledApp`, `UpdateCandidate`, `AppSource`, `IconRef`
- [ ] Implement `UpdateSource` trait + fake source for UI development
- [ ] Iced shell: header, filter bar, list placeholder, empty state, settings placeholder
- [ ] Theming: Light / Dark / System, accent color, rounded cards
- [ ] Config + SQLite scaffolding, migrations
- [ ] Logging with `tracing`, `--verbose` flag
- [ ] CI: `cargo test`, `clippy`, `fmt`, build on Ubuntu 24.04

### Acceptance Criteria

- `cargo run` opens a window with English UI, no crash.
- Fake source shows 5 fake apps with icons.
- `cargo test` passes.

### Exit: Demo video of shell + fake data.

---

## Phase 1 — MVP: APT + Flatpak

**Goal:** Real updates for the two main ecosystems. This is the first usable version.

### Tasks

- [ ] **Provenance — Where is this app coming from?**
  - [ ] `Provenance` model: kind, display_name, detail_url, trust (Official / Verified / Community / Unknown), signature status
  - [ ] APT provenance: parse `apt-cache policy` for origin + component, detect PPA as Community
  - [ ] Flatpak provenance: `flatpak remote-list --show-details` for remote URL, Flathub = Official/Verified
  - [ ] GitHub provenance: repo URL + release URL from API, Verified if checksum passes, Community if fork
  - [ ] Manual .deb: if no origin, show `Unknown — manual .deb, no repo found`, never fake it
  - [ ] List row: badge + trust pill + `from <origin>` line under version, always visible
  - [ ] Detail panel: full provenance card with clickable URLs, signature, last checked, `Also available from` list
  - [ ] Per-app `Preferred source` setting, stored in DB, update uses canonical source
  - [ ] Terminal panel shows provenance: `[GitHub • hydralauncher/hydra] Downloading...`
  - [ ] Safety: block if update URL domain != provenance domain, log provenance in history
- [ ] **APT Source**
  - [ ] Scan via `dpkg-query -W`
  - [ ] Join with `.desktop` files for name + icon
  - [ ] Check via `apt-get update` + `apt list --upgradable` (via helper)
  - [ ] Apply via `pkexec helper apt-upgrade`
  - [ ] Handle APT lock, held packages, phased updates
- [ ] **Flatpak Source**
  - [ ] Scan via `flatpak list --app`
  - [ ] Check via `flatpak remote-ls --updates`
  - [ ] Apply via `flatpak update -y`
  - [ ] Distinguish user vs system remotes
- [ ] **Orchestrator**
  - [ ] Parallel scan, dedup, plan ordering (APT -> Flatpak)
  - [ ] Progress events streamed to GUI
  - [ ] Verify after apply (re-scan version)
- [ ] **Terminal Panel — "Show terminal"**
  - [ ] Checkbox `[x] Show terminal` in footer, persisted in `config.toml` (`[ui] show_terminal`)
  - [ ] Docked log viewer under app list, monospace, dark background, read-only in v1
  - [ ] Stream live stdout/stderr from APT, Flatpak, helper, GitHub downloads via `LogBroadcaster` (tokio broadcast)
  - [ ] Show command echo `$ sudo apt-get update`, per-app lines `[Hydra] Downloading...`, status lines
  - [ ] Auto-show panel when update starts if `terminal_auto_show = true`, even if checkbox was off
  - [ ] Buttons: `Copy`, `Clear`, `Expand` (180px / 320px / collapsed)
  - [ ] Auto-scroll, max 2000 lines, sanitized logs (no tokens), persist last run to `last-run.log`
  - [ ] Handle `pkexec` buffering: helper flushes after every line
- [ ] **GUI**
  - [ ] Real list: icon, name, `current -> latest`, source badge, per-app Update
  - [ ] `Update All` button with progress bar
  - [ ] Search + filter by source (All / APT / Flatpak / GitHub / Manual)
  - [ ] Toggle: Show system packages (collapsed by default)
  - [ ] Error toasts: APT locked, network offline, Flatpak not installed
- [ ] **Helper**
  - [ ] Minimal `package-manager-helper` binary, `pkexec` invocation, arg validation
  - [ ] Log privileged actions

### Acceptance Criteria

- On clean Ubuntu 24.04, app lists real APT + Flatpak apps.
- `Update All` updates a test package end-to-end.
- Checkbox `Show terminal` shows live update output, like a normal terminal, docked under the list.
- No external terminal needed.
- App never runs GUI as root.

### Exit: Dogfood it for a week — use it instead of `apt upgrade`.

---

## Phase 2 — GitHub Bridge (Hydra Focus)

**Goal:** The killer feature. Hydra and other GitHub `.deb` apps update from GitHub.

### Tasks

- [ ] **Tracked Registry**
  - [x] SQLite table `tracked_apps`
  - [x] CRUD + migrations
  - [x] `presets.json` with Hydra: `hydralauncher/hydra`, asset pattern `hydralauncher_{version}_amd64.deb` (the real asset name; package `hydralauncher`)
- [ ] **GitHub Client**
  - [x] `GET /repos/{owner}/{repo}/releases/latest` with ETag cache
  - [x] Parse `tag_name`, `assets`, `body`, `html_url`
  - [x] Version normalize + compare (semver + fallback)
  - [ ] Optional token via keyring, rate-limit handling (60/hr unauth, 5000/hr auth)
  - [x] Offline / 403 / 404 friendly errors
- [ ] **Asset Matching**
  - [x] Pattern engine: `hydralauncher_{version}_amd64.deb`, `*_amd64.deb`, `*.AppImage`
  - [x] Prefer `.deb` on Debian, fallback to AppImage
  - [x] Checksum verification if `.sha256` / `checksums.txt` / electron-builder `latest-linux.yml` exists (formats are parsed; hashing happens in the download step)
  - [x] Download with progress + resume to cache dir
  - Signature verification (OpenPGP / sigstore) was tried and removed on purpose: most projects (Hydra included) publish no signatures. Checksums remain the integrity check.
- [ ] **Apply**
  - [x] `pkexec helper install-deb --path <file>` -> `dpkg -i` + `apt-get install -f -y` if needed
  - [x] Verify version after install (the installed version is read live from dpkg, so there is no copy in the DB to update)
  - [x] Cleanup cache (keep last 2 versions)
- [ ] **Manual .deb Tracker**
  - [ ] Detect `.deb` packages with no APT origin via `apt-cache policy`
  - [ ] Show badge `Manual .deb`, state `Up to date (unknown)` until linked
  - [x] `Link to GitHub` flow: paste `owner/repo` (or any github.com releases link), pick asset from latest release, save ("Add from GitHub" works for any repo, installed or not)
  - [ ] Auto-suggest preset if package name matches known preset
- [ ] **GUI**
  - [ ] App Detail panel: changelog, repo link, asset name, size, `View on GitHub`
  - [x] `Link to GitHub` panel (inline under the header, not a modal): preview of repo, tag, file, size, checksum status, trust and package name
  - [ ] `Ignore this version` action
  - [ ] GitHub badge + version diff

### Acceptance Criteria — Hydra Test

1. Install Hydra manually: download old `.deb` from GitHub, `dpkg -i`.
2. Open Package Manager -> Hydra appears with icon, badge `Manual .deb`, trust `Unknown`, origin `Unknown — manual .deb`.
3. Click `Link to GitHub` -> accept preset `hydralauncher/hydra`. Provenance upgrades to `GitHub • github.com/hydralauncher/hydra`, trust `Verified` after checksum.
4. Click `Check` -> Hydra shows update `old -> latest` with changelog link.
5. Click `Update` -> downloads `.deb`, installs, verifies, Hydra is on latest.
6. No terminal used in steps 2-5.

### Exit: Hydra end-to-end demo + 3 other GitHub apps tested (e.g. a `.deb` tool + an AppImage tool).

---

## Phase 3 — Polish: Icons, Search, Detail

**Goal:** Feels like a real product, not a prototype.

### Tasks

- [ ] **Icon Service**
  - [ ] `.desktop` Icon resolution: theme search, absolute paths, Flatpak export dirs
  - [ ] GitHub avatar fallback for linked apps
  - [ ] Initial-letter fallback with colored tile
  - [ ] Async loading, placeholder, disk cache with LRU (200 MB cap)
  - [ ] Cache index in SQLite
- [ ] **UX**
  - [ ] Search with debounce, fuzzy match on name + package
  - [ ] Sort: by name, by source, by size
  - [ ] App Detail: full changelog rendering, copy version, open homepage
  - [ ] Empty states: `You're up to date`, `No results for "xyz"`, `No Flatpak installed`
  - [ ] Keyboard: `/` to focus search, `Ctrl+R` to check
  - [ ] Settings page: token, interval, theme, show system packages, clear cache
  - [ ] Onboarding: 2-step wizard on first run (detect + link Hydra if found)
- [ ] **Theme System + Theme Editor — Make it not boring**
  - [ ] Theme JSON schema v1: colors, terminal, typography, shape, layout, effects
  - [ ] `ThemeLoader`: `serde_json` parse, validation, clamp out-of-range values, fallback to Default on error
  - [ ] Hot reload: watch `~/.config/package-manager/themes/` folder, reload in <200ms
  - [ ] Built-in themes: Default Light, Default Dark, Ubuntu Orange, Midnight Neon, Dracula
  - [ ] Settings > Appearance: theme gallery with live preview cards
  - [ ] Theme Editor: color pickers, sliders for radius / spacing / icon size, font pickers, density toggle, background image picker
  - [ ] Live preview pane in editor: fake app list + mini terminal updates instantly
  - [ ] Actions: Save as..., Duplicate, Export JSON, Import JSON, Reset to Default, Copy JSON to clipboard
  - [ ] Validation toasts: `Theme "X" is invalid, using Default` + log line
  - [ ] Docs: `THEMES.md` or section in README with schema + 3 example themes
- [ ] **Terminal Polish**
  - [ ] Syntax coloring: green success, yellow warning, red error, dim command echo
  - [ ] Search inside terminal (`Ctrl+F`), `Jump to bottom` pill when user scrolls up
  - [ ] Export log button, per-app filter in terminal (show only Hydra logs)
  - [ ] Copy single line on click, line numbers toggle
- [ ] **Performance**
  - [ ] Scan < 2s for 500 packages, check < 5s cached, < 15s cold
  - [ ] No UI freeze: all I/O off UI thread
  - [ ] Lazy icon loading
- [ ] **Tests**
  - [ ] Golden tests: real `dpkg-query` fixtures, real Hydra release JSON
  - [ ] Version compare fuzz: `v1.2.3`, `1.2.3-beta`, `2.0`, `2024.01.01`

### Acceptance Criteria

- Every app in list has a correct icon or a clean initial tile. No broken image.
- User can create a theme in editor, export JSON, import it on another machine, and it looks identical.
- Broken theme JSON never crashes app, falls back to Default with toast.
- Search feels instant.
- App passes manual UX review on 1080p + 4K.

### Exit: Screenshots + polish pass.

---

## Phase 4 — Background + Notifications

**Goal:** User does not have to open the app to know there are updates.

### Tasks

- [ ] Background checker: `systemd --user` timer or in-app scheduler (configurable interval, default 6h)
- [ ] Desktop notifications via `notify-rust`: `3 updates available — Hydra, Firefox, VLC`
- [ ] Tray / autostart option (opt-in)
- [ ] `Last checked` timestamp, `Check` button always available
- [ ] History: `history` table — what was updated, when, from which version to which
- [ ] History view in Settings / Detail
- [ ] Offline handling: show cached results with `Last checked` + `Offline` badge

### Acceptance Criteria

- Leave laptop overnight, wake up to notification if Hydra released overnight.
- History shows last 10 updates correctly.

### Exit: Background check demo.

---

## Phase 5 — Packaging + Extras

**Goal:** Anyone can install it, and it covers the long tail.

### Tasks

- [ ] Packaging
  - [ ] `cargo-deb` -> `.deb` release
  - [ ] Desktop file + icon + AppStream metadata
  - [ ] GitHub Actions release workflow: build `.deb`, attach to release
  - [ ] Flatpak manifest (dogfood: distribute Package Manager itself as Flatpak)
- [ ] **Snap Source** (feature flag `snap`)
  - [ ] Implement `UpdateSource` for Snap: `snap list`, `snap refresh --list`
- [ ] **AppImage Tracker** (feature flag `appimage`)
  - [ ] Scan `~/Applications/*.AppImage`, `~/.local/bin/*.AppImage`
  - [ ] Link to GitHub, same Bridge as `.deb`
- [ ] **Rollback**
  - [ ] Keep previous `.deb` in cache, offer `Revert` in App Detail
- [ ] **Export / Import**
  - [ ] Export tracked apps as JSON, import on new machine
- [ ] **Docs**
  - [ ] User guide, FAQ, screenshots, troubleshooting for APT lock / GitHub rate limit

### Acceptance Criteria

- Fresh Ubuntu VM: download `.deb` from GitHub, install, open app, it works.
- Snap + AppImage behind flags, no regression on APT/Flatpak/GitHub.

### Exit: v1.0 release.

---

## Backlog (Post-v1)

- [ ] CLI: `package-manager check`, `package-manager update --all` for power users
- [ ] Auto-update toggle per app
- [ ] Delta updates / zsync for large AppImages
- [ ] Multi-distro: add Fedora / Arch behind same `UpdateSource` trait (separate milestone, not v1)
- [ ] Flatpak remote management (add/remove remotes)
- [ ] Dark screenshots for store listings

## Risks

| Risk | Mitigation |
|---|---|
| GitHub rate limit (60/hr unauth) | ETag cache, batch checks, optional token, clear UI state |
| APT lock / broken dpkg | Friendly error, Retry, never force, log helper output |
| Iced learning curve | 2-day spike in Phase 0, egui fallback plan |
| `.deb` asset naming varies wildly | Pattern engine + manual pick from release assets, not hard-coded |
| User links wrong repo | Show repo + asset + changelog before install, require confirm |

## How to Track

- One GitHub Project board: columns `Backlog | In Progress | Review | Done`
- Every PR links to a Roadmap task checkbox.
- Weekly demo: 2 min screen recording of current build.
- Definition of Done: code + tests + docs + manual QA on Ubuntu 24.04 VM.

---
*Update this file when a phase ships. Mark tasks done, add date, link release.*
