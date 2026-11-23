# Kasumi Browser Release Notes

All notable changes to Kasumi Browser are documented here. The project follows SemVer, and every batch of changes bumps the version as required by [AGENTS.md](AGENTS.md).

---

## v0.2.0 — 2026-11-23

### Added

- Collapsible vertical workspace rail: starts expanded at 180 px with tabs, Command, New tab, Hibernate, shield, and RAM controls; `«` collapses it to a 44 px favicon-only rail with clickable tab switching and an active-tab indicator.
- Floating `Cmd/Ctrl+K` command palette with tab search, address/search resolution, keyboard selection (`↑`/`↓`, `Enter`, `Esc`), and quick actions. `Cmd/Ctrl+L` now opens the same palette seeded with the active URL.
- CEF now forwards `Cmd/Ctrl+K` from the web view to the native chrome, so the palette opens even when page content has keyboard focus.
- Rail controls use the Material Symbols icon font (pinned to the egui 0.30-compatible release), replacing hand-drawn glyphs with crisp, consistent icons.
- Rail tooltips now appear vertically centered beside the hovered row in both expanded and collapsed modes, use high-contrast text on a dark frame, and render in a transparent click-through overlay above the CEF page instead of inside the rail.
- Each tab row now reveals a hover-only × close button: right side when expanded, top-right when collapsed. The loading/hibernated marker shifts left so the two never overlap.
- The command palette now renders in a transparent, always-on-top native overlay window: the page stays visible behind it, input is focused immediately, and any primary click outside the palette card closes it. A focus request raised during the opening frame is carried into the overlay's next frame instead of being discarded.
- The collapse control is pinned to the outer edge of the expanded rail.
- Cross-platform parent-view extraction for macOS `NSView`, Windows `HWND`, and X11/XCB windows; show/hide/resize use one platform-agnostic interface.

### Changed

- CEF content now starts at `x = rail_width`, `y = 0` and fills the full window height, replacing the previous fixed 76 px top chrome.
- The command palette uses a separate transparent winit window instead of covering the native CEF child view, so page content remains visible behind the dimmed overlay on macOS, Windows, and Linux.
- Command palette labels now render inside their highlight rectangles, and mouse hover no longer overrides the keyboard selection.
- Bumped the project and macOS bundle version from `0.1.9` to `0.2.0`.
- Removed the obsolete `BUGFIX_TASKS.md` handoff; its completed items remain documented in the v0.1.9 and v0.2.0 release notes.

### Fixed

- Eliminated the `egui_wgpu::renderer: Detected a linear (sRGBA aware) framebuffer` warning by preferring `Rgba8Unorm`/`Bgra8Unorm` surface formats for the chrome, palette overlay, and tooltip overlay. Surface creation errors are now logged instead of panicking.
- Tooltip overlays are now anchored to the main window's content origin rather than its frame origin, and the visible card fills its overlay window so it is vertically centered on the hovered tab row on macOS and HiDPI displays. The command palette uses the same content-origin mapping.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 56 tests, including default rail visibility, rail widths, over-page tooltip metrics/anchoring in both modes, overlay content-origin scaling, hover close-button geometry, palette focus-request carry-over, click-outside dismissal, command palette candidates/keyboard selection/row fill, CEF Cmd/Ctrl+K forwarding, rail-relative content rectangles, and non-sRGB surface-format fallback behavior.
- `cargo build --release`
- Manual runtime verification completed on macOS: expanded/collapsed rail, command palette overlay and click-outside dismissal, hover close buttons, vertically centered tooltips, and clean shutdown confirmed by the user.

---

## v0.1.9 — 2026-11-23

### Fixed

- Tooltips are now anchored inside the native chrome region so they are no longer hidden behind the CEF page view.
- Middle-clicking anywhere on a tab now closes it.
- Closing a tab now completes the CEF browser teardown, so media such as YouTube audio stops immediately instead of continuing in the background.
- Favicons now prefer conventional `favicon.ico` URLs, fall back to `<origin>/favicon.ico` when CEF reports no icon links, and reject later decorative icon candidates that do not match the site tab icon.

### Changed

- Bumped the project and macOS bundle version from `0.1.8` to `0.1.9`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 36 tests, including omnibox edge cases, tooltip anchoring, CEF/switch invariants, adblock request mapping and blocking, favicon validation, hibernation thresholds, shortcut modifiers, and SQLite session/history/bookmark behavior.
- `cargo build --release`
- Manual macOS run confirmed that closing a YouTube tab stops audio immediately.

---

## v0.1.8 — 2026-11-23

### Fixed

- Tooltips now appear immediately by removing egui's default hover delay.
- `Cmd/Ctrl+L` now works even when the omnibox already has keyboard focus, and selects the full URL.
- Favicon selection now accepts only the first successful icon per navigation, preventing later `apple-touch-icon`/large PNG candidates from overwriting the site's real tab icon.

### Changed

- Bumped the project and macOS bundle version from `0.1.7` to `0.1.8`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser`
- Manual macOS run pending after this verification batch.

---

## v0.1.7 — 2026-11-23

### Fixed

- The omnibox now follows the active tab's committed URL after link clicks, Back/Forward, and tab switches. It stops overwriting text while focused, and Cmd/Ctrl+L focuses and selects the whole URL.
- Back and Forward controls are drawn as native arrow shapes instead of relying on font glyphs, with dimmed-but-visible disabled states.
- Tooltips now cover tabs, close buttons, new tab, Back, Forward, Reload/Stop, the shield badge, Hibernate All, RAM readout, and the omnibox.
- F12 now opens DevTools when the web view has focus via a CEF `KeyboardHandler`; the winit shortcut remains as a fallback, and repeated presses no longer spawn duplicate windows.
- Right-click → View Page Source now opens a visible `view-source:` tab and reuses an existing source tab if present.

### Added

- End-to-end favicon pipeline: CEF favicon URL callbacks now trigger main-thread image downloads, decode to RGBA, and render in the tab strip with a neutral fallback.
- Modernized the tab strip with rounded surfaces, active accent line, hover states, and favicon slots while keeping the 38 px tab-strip / 76 px chrome geometry.
- Added an omnibox unit test for `view-source:` URLs.

### Changed

- Bumped the project and macOS bundle version from `0.1.6` to `0.1.7`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser`

---

## v0.1.6 — 2026-11-23

### Added

- Added `.github/workflows/pr.yml` for pull requests targeting `main`. It checks formatting, runs `cargo check --bins`, and runs the unit tests with `cargo test --bin kasumi-browser` on the supported `macos-15-intel` runner.
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
