# Kasumi Browser Release Notes

All notable changes to Kasumi Browser are documented here. The project follows SemVer, and every batch of changes bumps the version as required by [AGENTS.md](AGENTS.md).

---

## v0.1.6 — 2026-11-23

### Added

- Added `.github/workflows/pr.yml` for pull requests targeting `main`. It checks formatting, runs `cargo check --bins`, and runs the unit tests with `cargo test --bin kasumi-browser`.
- Added Cargo and CEF caches to the PR workflow to keep CI runs fast.

### Changed

- Change the versioning policy so a version bump is released when a PR is merged into `main`; PRs include the bump so CI validates the exact release state.
- Bumped the project and macOS bundle version from `0.1.5` to `0.1.6`.

### Notes

- All future changes should land through pull requests and pass the PR workflow before merge.

---

## v0.1.5 — 2026-11-23

### Added

- Added the missing Back/Forward icon rendering bug to `BUGFIX_TASKS.md`, including likely font-glyph/disabled-state causes and a native `egui::Painter` / `ColorImage` fix path.
- Updated the handoff implementation order and manual verification checklist to cover visible navigation icons.

### Changed

- Bumped the project and macOS bundle version from `0.1.4` to `0.1.5`.

### Notes

- This is a documentation-only batch. The runtime fixes are still deferred to the next versioned batch.

---

## v0.1.4 — 2026-11-23

### Added

- Added `BUGFIX_TASKS.md` as the handoff for reported chrome/navigation issues: unclear toolbar buttons, tab styling, tooltips, stale omnibox URL, F12 DevTools, view-source, and missing favicons.
- Documented current findings, likely root causes, exact files to inspect, implementation leads, and acceptance criteria for each bug.
- Linked the bugfix handoff from `README.md` and the future-development list.

### Changed

- Bumped the project and macOS bundle version from `0.1.3` to `0.1.4`.

### Notes

- This is a documentation-only batch. The runtime fixes are intentionally left for the next versioned batch.

---

## v0.1.3 — 2026-11-23

### Added

- Added `RELEASE_NOTES.md` as the canonical record of shipped changes.
- Linked the release notes from `README.md`.

### Changed

- Bumped the project and macOS bundle version from `0.1.2` to `0.1.3`.

---

## v0.1.2 — 2026-11-23

Initial public source release: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser).

### Added

- Renamed the product from LiteBrowser to **Kasumi Browser** across source, package, binary, CEF helper, bundle ID, database path, and documentation.
- Implemented the Rust/CEF core shell with the `mimalloc` global allocator and the CEF subprocess fork trap.
- Added unbundled macOS CEF support by setting `browser_subprocess_path` when not running from a `.app` bundle.
- Built native browser chrome with `winit`, `wgpu`, and `egui`: horizontal tab strip, navigation buttons, omnibox, ad-block shield badge, RAM readout, and hibernate-all button.
- Embedded CEF as a native OS child view (`NSView` on macOS, `HWND` on Windows, `X11` on Linux), never using OSR.
- Implemented tab creation, switching, closing, reopening, hibernation, and waking with metadata preserved in Rust.
- Added the `sysinfo` RAM watchdog with normal, pressure, and critical hibernation thresholds.
- Integrated `adblock-rust` request interception with built-in starter rules and optional EasyList/EasyPrivacy filter files.
- Added SQLite WAL persistence for history, bookmarks, and session tabs, restoring only the active tab live at startup for low-RAM startup.
- Wired keyboard shortcuts: `Ctrl/Cmd+T`, `Ctrl/Cmd+W`, `Ctrl/Cmd+L`, `Ctrl/Cmd+Shift+T`, `Alt+H`, and `F12`.
- Added macOS `.app` bundling support and release packaging instructions.
- Documented macOS Developer ID signing/notarization, Windows Authenticode signing, and Linux checksum/GPG signing.
- Added `.gitignore` to exclude build artifacts, local CEF files, machine-specific Cargo config, logs, and OS/editor files.
- Added `.cargo/config.toml.example` as a safe local CEF/Xcode configuration template.
- Added mandatory version-bump rules to `AGENTS.md`.
- Added `Future Development` and `#will be added later` sections to `README.md`.
- Published the verified source to the public GitHub repository.

### Fixed

- Fixed the macOS CEF/winit reentrancy crash by pumping CEF from a main-thread `CFRunLoop` timer instead of `window_event` handlers and ignoring reentrant `about_to_wait` ticks.
- Fixed macOS shutdown ordering so browsers and the SQLite WAL are flushed in `about_to_wait`, the pump timer is disabled, and `cef::shutdown()` runs while winit run-loop observers still exist.
- Changed `LifeSpanHandler::do_close` to return `1` so CEF actually destroys browsers and emits `on_before_close`.
- Fixed Retina layout by making `ContentRect` logical/DIP-based, respecting the flipped winit `NSView` on macOS, and converting to physical pixels for Windows/X11 APIs.
- Fixed false-positive `.app` bundle detection and the missing unbundled CEF subprocess path.
- Fixed the `CFRunLoopTimerCreate` FFI signature by including the required `CFIndex order` argument.
- Removed temporary high-volume shutdown/layout tracing from normal log output.

### Verification

- `cargo fmt --check` passed.
- `cargo check --bins` passed.
- `cargo test --bin kasumi-browser` passed.
- Repeated interactive macOS runs confirmed startup, page load, correct CEF viewport geometry, and clean shutdown with exit code `0`.

---

## Future development

- Vertical tabs
- Theme awareness / OS light-dark following
- Full Windows and Linux validation
- More unit tests
- Settings UI
- Bookmark manager UI
- Download manager
- Automated cross-platform integration tests
- Release CI and signed artifact publishing
