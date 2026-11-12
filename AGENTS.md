# AGENTS.md — How AI Agents (and Humans) Work in This Repo

> This file is for anyone — human or AI agent — contributing to Package Manager.
> Read it before writing code. Keep it up to date when conventions change.
> Language: English for code, docs, commits, PRs and UI.

## 1. Project in One Paragraph

Pure Rust update manager for Ubuntu / Debian-based distros. One minimalist GUI that updates APT, Flatpak, manually installed `.deb` packages, and GitHub-released apps (e.g. Hydra Launcher) via a GitHub Bridge. GUI runs as user, privileged work goes through a small `pkexec` helper. See `ARCHITECTURE.md` for the full design and `ROADMAP.md` for phases.

## 2. Quick Start

```bash
# Clone
git clone https://github.com/<owner>/package-manager.git
cd package-manager

# Check toolchain
rustc --version  # 1.80+
cargo --version

# Run (GUI)
cargo run -p package-manager-app

# Run with logs
RUST_LOG=debug cargo run -p package-manager-app

# Tests
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

Release install (needed for GitHub/APT installs; release builds only trust the helper at `/usr/libexec/package-manager/`):

```bash
cargo build --release
./packaging/install.sh            # developer install; asks for your admin password via pkexec
./packaging/build-deb.sh          # builds dist/package-manager_<version>_<arch>.deb for users
./packaging/install.sh uninstall
```

Re-run the installer after every rebuild. `cargo run` (debug) finds the helper next to the executable and needs no install.

Never run the GUI as root. Never run `cargo run` with `sudo`. If you need to test the helper, use `pkexec` explicitly and log what you did.

## 3. Repo Layout

```
crates/
  app/               # iced GUI only. No apt/flatpak/github calls here directly.
  core/              # Domain, orchestrator, traits, errors. No UI, no CLI parsing.
  sources-apt/       # APT + dpkg implementation
  sources-flatpak/   # Flatpak implementation
  sources-github/    # GitHub Bridge
  icons/             # .desktop parsing, icon lookup, cache
  helper/            # Privileged helper binary (tiny, auditable)
  config/            # Config + SQLite migrations
  runtime/           # Composition root: wires concrete sources into the orchestrator
docs/
  ARCHITECTURE.md
  ROADMAP.md
  AGENTS.md
```

### Dependency Rules

- `app` -> `core`, `runtime`, `icons`, `config`. Never -> `sources-*` directly.
- `runtime` -> `core` + `sources-*`. The only crate that knows which sources exist.
- `core` -> no source crates, no GUI crates. Only traits + domain.
- `sources-*` -> `core` only.
- `helper` -> std only + minimal deps. Keep it tiny and reviewable.
- If you need a new dep, justify it in the PR. Prefer std + already-used crates.

## 4. How to Work as an Agent

### Before You Code

1. Read `ARCHITECTURE.md` section relevant to your task.
2. Check `ROADMAP.md` — find the checkbox for your task. Do not build Phase 5 work during Phase 1.
3. Search for existing code: `rg "UpdateSource"`, `rg "InstalledApp"`. Do not reinvent types.
4. If the task is ambiguous, ask one clarifying question with options, then proceed with the stated assumption.

### While You Code

- **Small PRs:** One feature / one fix per PR. Max ~400 lines + tests. If it grows, split it.
- **English everywhere:** Code, comments, commit messages, UI strings, error messages. No Portuguese in code, even in comments.
- **No placeholders:** No `TODO` without a linked issue. No `unimplemented!()` in merged code. No fake data in production paths.
- **No shell hacks:** Use `std::process::Command` with explicit args. Never `sh -c "apt ... $user_input"`. Never interpolate user input into a shell string.
- **No `unwrap()` in production paths:** Use `Result` + `thiserror`. `unwrap()` is allowed only in tests and in `main()` startup where failure is fatal and logged.
- **No panics on user input:** Bad repo name, bad version string, missing icon, offline — all are `Result::Err` with a friendly message, never a crash.

### After You Code

1. `cargo fmt`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`
4. Manual QA if you touched GUI or helper: run on Ubuntu VM or host, screenshot if UI changed.
5. Update docs if you changed architecture or added a source.

## 5. Code Style

- `cargo fmt` is law. Run it before every commit.
- `clippy -D warnings` must pass.
- Use `thiserror` for library errors, `anyhow` only at binary edge (`main`).
- Naming:
  - Types: `PascalCase` — `InstalledApp`, `UpdateCandidate`, `GithubRepo`
  - Functions: `snake_case` — `check_updates()`, `resolve_icon()`
  - Constants: `SCREAMING_SNAKE_CASE`
  - GUI messages: `PascalCase` — `Message::UpdateAllRequested`
- Errors: one enum per source, e.g. `AptError`, `GithubError`, with a `#[error("...")]` message that a user can understand.
- Async: `tokio`, `async_trait` for `UpdateSource`. Never block UI thread with I/O, sleep, or `Command::output()` on main thread. Spawn a task.
- Logging: `tracing::{info, warn, error, debug}`. No `println!` in library code. Log privileged actions.

### Example — Good vs Bad

```rust
// GOOD: typed error, no unwrap, explicit args
pub async fn check_updates(&self, apps: &[InstalledApp]) -> Result<Vec<UpdateCandidate>, GithubError> {
    let release = self.client.latest_release(&app.repo).await?;
    Ok(...)
}

// BAD: do not do this
pub fn check(_apps: &[InstalledApp]) -> Vec<UpdateCandidate> {
    let out = std::process::Command::new("sh").arg("-c").arg(format!("curl {}", url)).output().unwrap();
    // ...
}
```

## 6. Adding a New Source (e.g. Snap, AppImage)

Follow this checklist. Do not skip steps.

1. Create `crates/sources-<name>/` with `Cargo.toml`, `src/lib.rs`.
2. Implement `UpdateSource` trait from `core`: `kind()`, `scan()`, `check_updates()`, `apply()`.
3. Add domain mapping: how does this source map to `InstalledApp` + `AppSource`?
4. Add fixtures: real CLI output / API JSON in `crates/sources-<name>/tests/fixtures/`.
5. Add unit tests: scan parsing, version compare, asset matching if any.
6. Wire into orchestrator in `core` behind a feature flag if experimental: `cargo --features snap`.
7. Add GUI filter + badge in `app` (e.g. `[Snap]` badge).
8. Update `ARCHITECTURE.md` (new section) + `ROADMAP.md` (checkbox).
9. Manual QA on Ubuntu VM with a real app from that source.

## 7. GitHub Bridge Rules

This is the most sensitive part. Be extra careful.

- Never install from a repo the user did not explicitly link. No auto-linking without confirmation.
- Always show before install: repo (`owner/repo`), tag, asset name, size, changelog URL.
- Prefer HTTPS only. Reject `http://` URLs.
- Verify checksum if release provides `.sha256` / `checksums.txt`. If no checksum, show warning `No checksum provided by maintainer` and require explicit confirm.
- Cache GitHub responses with ETag. Do not hammer API. Respect 403 rate-limit, show countdown.
- Store token in keyring, never in `config.toml`, never in logs, never in git.
- Version compare: try `semver`, fallback to loose compare. `v2.0.0` == `2.0.0`. Never claim update if equal after normalization.
- Download to `~/.cache/package-manager/downloads/<app>/<version>/`. Validate file is a `.deb` (magic bytes / `file` output) before calling helper.
- Helper must validate: path is inside cache dir, extension is `.deb`, package name is in the approved plan.


### Terminal Panel Rules — "Show terminal"

- The panel is a **read-only log viewer styled as a terminal** in v1. No interactive input, no `sh`, no user-typed commands.
- Every background process must emit `LogEvent` via `LogBroadcaster`. Do not `println!` from sources. Do not write directly to GUI state from a task.
- Stream, do not batch: read child stdout/stderr line by line on a background tokio task, flush after each line. Helper must flush after each line because `pkexec` can buffer.
- Format:
  - Command echo: `$ <command>` (without token, without password)
  - Per-app: `[<AppName>] <message>` — e.g. `[Hydra] Downloading...`
  - Status: `Done`, `Failed: <friendly reason>`
- Sanitize before emitting: strip GitHub token, `?token=`, passwords, keyring values. If in doubt, redact.
- Cap buffer at 2000 lines, drop oldest. Persist last run to `last-run.log`, max 2 MB, rotated.
- Checkbox state lives in `config.toml` `[ui] show_terminal`. Persist on toggle. `terminal_auto_show` auto-opens panel on update start.
- UI: auto-scroll to bottom unless user scrolled up. If scrolled up, show `Jump to bottom` pill. Buttons: `Copy`, `Clear`, `Expand`.
- Tests: unit test `LogEvent` sanitization (token never leaks), test that broadcast does not block UI, test that helper flushes.
- Do not log full download URLs with tokens. Do not log home path secrets.



### Provenance Rules — Where Is This App Coming From?

- **Never fake origin.** If you cannot prove it from `apt-cache policy`, `flatpak remote-list`, or GitHub registry, show `Unknown`. No guessing.
- Every `InstalledApp` must have a `Provenance`. No app without provenance in production code.
- UI must always show: badge (`[APT]` / `[Flatpak]` / `[GitHub]` / `[Manual .deb]`) + trust pill (`Official` / `Verified` / `Community` / `Unknown`) + `from <origin>` line.
- Detail panel must show full URLs, clickable, plus signature status and `Also available from` if multi-source.
- Safety: if update download domain != provenance domain, block update, show error. Test this.
- Trust rules:
  - APT from Ubuntu/Debian official repo = Official, PPA = Community
  - Flathub = Official/Verified, other Flatpak remote = Community
  - GitHub preset (Hydra) + checksum verified = Verified, user-linked fork = Community, no checksum = Signature Missing + warning
  - Manual .deb with no origin = Unknown
- Terminal must prefix updates with provenance: `[GitHub • hydralauncher/hydra]`, `[Flatpak • Flathub]`, `[APT • Ubuntu noble/main]`.
- Tests: fixtures for `apt-cache policy` output with PPA, for `flatpak remote-list --show-details`, for GitHub release with/without checksum. Verify Unknown case is not mislabeled.
- Do not hide Unknown behind a nice badge. Unknown is red/gray, explicit, with action `Link to GitHub`.

### Theme System Rules

- Themes are **data only**. Never execute code from a theme file. Never allow shell commands, paths outside `~/.config/package-manager/` or `~/Pictures/` for `backgroundImage`.
- Schema lives in `crates/themes/` or `crates/config/`. Single source of truth: `Theme` struct + `serde` + defaults. Do not duplicate field names in GUI.
- Validation: use `serde_json` + manual clamp. Invalid JSON -> toast + fallback to Default, never panic, never blank screen. Log serde error with line number.
- Ranges: colors `#RRGGBB` / `#RRGGBBAA`, fontSize 10-20, radius 0-28, transparency 0.0-0.85, iconSize 24-64. Clamp, do not reject whole theme for one out-of-range value unless JSON is unparsable.
- Hot reload: file watcher, debounce 150ms, reload <200ms. Do not restart app.
- Editor: every control updates live preview instantly without saving. Save writes `themes/<name>.json`. Export copies JSON to clipboard / file. Import validates before applying, asks confirm.
- Do not hard-code colors in GUI code. Always read from current `Theme`. If you add a new color, add it to schema + defaults + built-ins + editor + docs in same PR.
- Test: unit test for invalid JSON fallback, clamp tests, test that Default theme always parses, test that exported theme re-imports identically.
- Examples: keep 3+ example JSON themes in `themes/` folder in repo, so users can copy them.

## 8. GUI Rules (Iced)

- All UI strings in English. Use constants or a `strings.rs`, no hard-coded Portuguese.
- MVU: `Model` + `Message` + `update()` + `view()`. No mutable global state.
- All I/O in background tasks. UI thread only renders.
- Icons load async, show placeholder while loading. Never block on icon I/O.
- States to handle: Loading, Empty (up to date), Error (with Retry), Offline, Rate Limited, Updating (progress).
- Layout: keep it minimalist. One main list, one `Update All` button. Do not add tabs / dashboards without discussion.
- Accessibility: buttons have labels, contrast passes, keyboard: `/` focuses search, `Ctrl+R` checks.
- Screenshots: `cargo run -p package-manager-app --features screenshots -- --screenshot-dir DIR [--demo-repo owner/repo] [--prefix NAME]` saves PNGs of the real window (list, and the Add-from-GitHub panel for a repo) and exits; the feature is development-only. Current reference shots live in `docs/screenshots/`.
- Screenshots: if you change UI, attach before/after screenshots in PR, including terminal panel open + closed, and with at least 2 themes (Default + one custom).
- Do not hard-code colors. Use Theme palette.
- Terminal panel must never block UI thread. All log I/O is via channel + subscription.

## 9. Security Rules

- GUI never runs as root.
- Helper is minimal. Review every line added to `helper/`.
- No `sh -c`, no string interpolation into commands. Use `Command::new("apt-get").arg("update")`.
- Validate all paths: download path must be under cache dir, icon path must be under allowed dirs.
- Never log tokens, passwords, or full URLs with tokens.
- No analytics, no phone-home, no telemetry.
- Dependencies: run `cargo audit` before release. Do not add a dep with known RCE.

## 10. Testing Rules

- Unit tests must pass without network, without sudo, without `apt` / `flatpak` installed. Use fake sources.
- For source crates, use fixtures, not live CLI calls, in unit tests.
- Integration tests: fake `UpdateSource` + orchestrator, verify plan ordering (APT -> Flatpak -> GitHub).
- Golden tests: keep real `dpkg-query` output and real Hydra release JSON in fixtures. If GitHub API changes, update fixture + test.
- Manual QA checklist for PRs touching update flow:
  - [ ] Fresh Ubuntu 24.04 VM
  - [ ] Install old Hydra `.deb` manually, verify provenance shows `Unknown`
  - [ ] Link to GitHub, verify provenance shows `GitHub • hydralauncher/hydra` + `Verified`
  - [ ] Link to GitHub, check, update, verify version
  - [ ] APT lock case: run `apt update` in another terminal, verify friendly error
  - [ ] Offline: disconnect, verify cached + offline badge
- Do not commit `.deb` files, downloaded assets, or tokens. `.gitignore` covers `target/`, `*.deb`, `downloads/`, `*.db`.

## 11. Commits & PRs

- Commits: Conventional Commits, English.
  - `feat(github): add asset pattern matching`
  - `fix(apt): handle APT lock gracefully`
  - `docs(arch): clarify privilege separation`
  - `test(core): add orchestrator ordering test`
- PR template:
  - What changed + why
  - Roadmap checkbox linked
  - Tests run (`cargo test`, `clippy`, `fmt`)
  - Manual QA done (if UI/helper)
  - Screenshots (if UI)
  - Risks / follow-ups
- One reviewer for docs, two for `helper/` or `sources-github/`.

## 12. Common Commands

```bash
# Format, lint, test
cargo fmt
cargo clippy --workspace -- -D warnings
cargo test --workspace

# Run specific crate tests
cargo test -p package-manager-core
cargo test -p package-manager-sources-github

# Audit deps
cargo audit

# Build release .deb (Phase 5)
cargo build --release
cargo deb --package package-manager-app

# Clean cache during dev
rm -rf ~/.cache/package-manager/icons ~/.cache/package-manager/downloads
```

## 13. What NOT to Do

- Do not add Snap / AppImage / Fedora support in Phase 1-2. Follow ROADMAP.md order.
- Do not add a store / browse / install-new-apps feature in v1. Update-only.
- Do not run GUI as root to "make it easier".
- Do not use Python / shell scripts for core logic. Rust only.
- Do not hard-code Hydra version or asset URL. Use GitHub API + pattern matching.
- Do not invent icons. Use `.desktop` + theme + GitHub avatar fallback + initial tile, in that order.
- Do not add telemetry.

## 14. If You Get Stuck

- Check `ARCHITECTURE.md` first, then `ROADMAP.md`.
- Search codebase: `rg "TODO"`, `rg "UpdateSource"`.
- For Iced, check `crates/app/` examples and official Iced docs, not random blog posts.
- For GitHub API, test with `curl` + fixture JSON before writing Rust code:
  ```bash
  curl -s https://api.github.com/repos/hydralauncher/hydra/releases/latest | head -n 100
  ```
- If still stuck, open a Draft PR with what you have + questions, do not go silent.

---
*Agents: if you change a convention, update this file in the same PR. This file is the contract.*
