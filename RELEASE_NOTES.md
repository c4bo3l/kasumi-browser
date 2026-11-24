# Kasumi Browser Release Notes

All notable changes to Kasumi Browser are documented here. The project follows SemVer, and every batch of changes bumps the version as required by [AGENTS.md](AGENTS.md).

---

## v0.7.0 — Unreleased

### Added

- Native download manager with a WAL-backed history, active progress, speed, file path, pause/resume/cancel, retry, remove, clear-finished, open-file, and show-in-folder controls. Downloads run through CEF's existing browser process and native child views, with no save prompt, OSR view, or second renderer.
- Parallel segmented HTTP(S) downloads. After CEF resolves the destination, Kasumi cancels the CEF copy and fetches up to eight balanced `Range` segments through CEF `URLRequest`s, writing to a `.kasumi.part` file on a background Rust thread and renaming it on completion. Pause cancels active requests; resume continues from each segment's last byte; retry starts a fresh generation so late callbacks cannot corrupt the file. The Downloads page shows the active connection count, and non-HTTP or non-range-capable transfers still work as a single native request.
- A native Downloads page inside Settings, opened by `Cmd/Ctrl+J`, the rail button in both modes, the macOS File menu, or the command palette. Stale downloads are marked interrupted after a restart so Retry is always available instead of dead controls.
- Incognito mode research notes in the README future-development roadmap.
- Version bumped from `0.6.0` to `0.7.0` across the package, lockfile, canonical spec, bundle metadata, About menu, and docs.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 142 tests
- `cargo build --release`
- PR CI checks formatting, binaries, and unit tests on macOS 15 Intel, Windows, and Linux; interactive macOS verification is required before merge.

---

## v0.6.0 — 2026-11-24

### Added

- Native Workspaces. Each workspace owns its pinned and regular tabs, remembers the tab selected in it, and can be switched from a dedicated strip in both rail modes. Workspace metadata and per-workspace session tabs persist in SQLite, and startup still spawns only the active tab while restoring every other tab as hibernated.
- The native Settings dialog is split into **General**, **Workspaces**, and **Bookmarks** pages. The Workspaces page lists every workspace with Default/Active badges and edits the selected workspace with Material Symbols icon buttons. Creation uses a dedicated mode opened by **New workspace** or the rail `+`; its form replaces the list/editor instead of appearing beside an existing workspace, **Back** returns without creating, and a successful create returns to the list with the new workspace selected. Switching, default selection, and deletion remain explicit actions. The accent tints only native chrome accents — rail, command palette, tab borders, loading indicator, and tooltips — so CEF page colours are untouched.
- Any workspace can be selected as the default workspace. The default controls which workspace opens at startup; switching workspaces during the current session still takes effect immediately. Deleting the default workspace promotes another workspace instead of leaving the browser without a startup space.
- Workspace-named bookmark folders. Saving a bookmark uses the active workspace name; every bookmark remains globally visible and can be opened from any other workspace. The Bookmarks Settings page lists folders and includes an **Open** action for each saved page.
- SQLite schema and migrations for `workspaces`, workspace-aware `session_tabs`, and workspace-scoped bookmarks. Legacy databases gain a default `Personal` workspace, legacy `Toolbar` bookmarks are moved into it, and session/bookmark reads tolerate older column layouts. A persisted `is_default` flag is backfilled from the previous active workspace.
- Unit tests for workspace tab/pin/bookmark ownership, workspace-aware insertion order, unique workspace names, accent tinting, workspace persistence, stale-workspace normalization, default-workspace startup/selection/deletion and empty-workspace restore, separate edit/create drafts, dedicated create-mode state, collapsed workspace switching and tooltips, collapsed status-badge sizing, Settings page selection, bookmark folder isolation, `Cmd/Ctrl+Q` routing, legacy database migration, and closing the last tab in a workspace.

### Changed

- The collapsed and expanded rails now show a workspace switcher above the tab list; tabs are filtered to the active workspace, and pinned tabs remain at the top of each workspace group. Collapsed mode uses larger 34×34 icon buttons, accent borders, a current-space dot, and name tooltips so spaces can be changed without expanding the rail. Its RAM and ad-block status icons now use dedicated 20 px badge rows with value tooltips instead of the expanded rail's 15 px icon size. The redundant `KASUMI` rail title was removed.
- The rail's **New workspace** button opens directly in Workspaces create mode; the regular Settings button and `Cmd/Ctrl+,` still open General first. Settings actions use Material-style filled/tonal/outlined icon buttons, and editing uses an explicit **Save changes** action.
- Command-palette tab results now show their workspace name alongside the URL.
- Closing the active tab keeps the user in the same workspace when other tabs remain, and opens a fresh tab there instead of jumping to another workspace when it was the workspace's last tab.
- Bookmark folder renames are scoped by workspace id, so archived folders from deleted workspaces are never accidentally renamed with an unrelated active workspace.
- Bumped the project, macOS bundle, About-menu version, docs, and lockfile from `0.5.2` to `0.6.0`.
- Expanded **Future Development** with out-of-the-box picture-in-picture research: prefer Chromium's native PiP in CEF, avoid an OSR/second-renderer fallback, and route native overlay lifecycle through the existing UI action and platform-view paths.

### Fixed

- Deleting a workspace no longer discards its bookmarks; its folder is archived as `Archived {name}` and its tabs move to the first remaining workspace.
- Duplicate workspace names are rejected by appending `2`, `3`, and so on, keeping bookmark folder names unambiguous.
- Closing the last tab in one workspace no longer silently switches to a different workspace.
- Restoring a session no longer overrides the chosen default workspace with another workspace's saved tab when the default has no saved tabs.
- Editing a workspace name no longer renames that workspace when **Create workspace** is clicked. Creation now has its own mode and draft, so changing `Personal` to `asd` and creating a workspace leaves `Personal` intact and creates only the new workspace from the create form.
- `Cmd+Q` now closes the app gracefully. The macOS app menu includes **Quit Kasumi Browser**, the native event monitor maps `Cmd+Q`, and the main keyboard handler maps `Ctrl/Cmd+Q` to the same deferred CEF/SQLite shutdown path as the window close button.
- Existing databases created by the first Workspaces build now migrate before the default-workspace row is written, so startup no longer fails with `table workspaces has no column named is_default`. The previously active workspace is promoted to default.
- Collapsed-rail RAM and ad-block icons no longer stay at the expanded rail's 15 px size. Both status badges now reserve a 24 px collapsed row with a 20 px glyph and show their current value in a tooltip.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 124 tests
- `cargo build --release`
- PR CI checks formatting, binaries, and unit tests on macOS 15 Intel, Windows, and Linux; interactive macOS verification is required before merge.

---

## v0.5.2 — 2026-11-24

### Changed

- Expanded **Future Development** with Google Meet/WebRTC research notes: CEF-to-egui permission callback bridge, media-access and site-permission prompts, screen-share source selection, macOS camera/microphone entitlements, and persisted permission decisions.
- Added tiny local AI research notes covering sub-0.4B on-device model candidates (`LFM2.5-230M`, `Gemma-3-270M-it`, `SmolLM2-360M-Instruct`), `Q8_0` quantization, grammar-constrained output, and a 2048-token context cap.
- Documented a proposed CEF/Chromium engine update process: scheduled crates.io check, review PR with coordinated version bump, full three-platform CI plus runtime smoke test, and delivery through signed GitHub Releases rather than runtime binary swaps.
- Gated the macOS-only `KasumiEvent` variants behind `cfg(target_os = "macos")` so Windows and Linux no longer emit dead-code warnings.
- Bumped the project, macOS bundle, About-menu version, docs, and lockfile from `0.5.1` to `0.5.2`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 103 tests
- `cargo build --release`
- PR CI checks formatting, binaries, and unit tests on macOS 15 Intel, Windows, and Linux.
- No user-visible behavior changed in this release.

---

## v0.5.1 — 2026-11-23

### Fixed

- DevTools can be brought forward again after a window resize: F12 and the DevTools rail action now call `show_dev_tools` even when a DevTools browser already exists, letting CEF focus the existing window instead of returning early.
- Middle-clicking a link no longer navigates the current tab. CEF's open-from-tab disposition is mapped to a Kasumi tab: middle-click opens a live background tab, while Ctrl/Cmd+click and popup-style dispositions open a foreground tab.
- Background tabs opened this way hide their native child view until selected and participate in normal hibernation/session persistence. Browser creation now uses CEF's cross-platform `set_as_child` helper so the same path compiles on macOS, Windows, and Linux; macOS uses CEF's `hidden` flag, Windows clears `WS_VISIBLE`, and Linux unmaps the X11 child on creation. CEF window handles now convert between pointer and XID forms through one shared helper.
- Bumped the project, macOS bundle, About-menu version, docs, and lockfile from `0.5.0` to `0.5.1`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 103 tests
- `cargo build --release`
- PR CI checks formatting, binaries, and unit tests on macOS 15 Intel, Windows, and Linux.

---

## v0.5.0 — 2026-11-23

### Added

- Persistent pinned tabs: pin or unpin from the rail context menu, command palette, or clickable rail pin badge. Pinned tabs occupy a compact section at the top of both rail modes and are stored in SQLite session snapshots, including a migration for databases created by older releases.
- Closed-tab undo preserves pin state, and new tabs, duplicates, and restored sessions keep pinned tabs ahead of regular tabs.
- Bumped the project, macOS bundle, About-menu version, docs, and lockfile from `0.4.1` to `0.5.0`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 96 tests
- `cargo build --release`

---

## v0.4.1 — 2026-11-23

### Changed

- Consolidated the README’s “will be added later” items into **Future Development** and identified pinned tabs as the next feature because they deliver immediate value and provide the foundation for per-workspace pinned lists.
- Bumped the project, macOS bundle, About-menu version, docs, and lockfile from `0.4.0` to `0.4.1`.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser`
- No runtime behavior changed in this documentation release.

---

## v0.4.0 — 2026-11-23

### Added

- Native Settings overlay opened with `Cmd/Ctrl+,`, the bottom rail **Settings** button, the macOS **File → Settings…** menu, or the command palette. It renders in the existing transparent, always-on-top native overlay so the CEF page remains visible behind it.
- Persisted preferences in SQLite WAL: theme preference (`System`/`Light`/`Dark`), rail startup state, search provider, ad-block enabled state, and normal-pressure hibernation idle timeout.
- Theme override layered over the live OS theme: `System` tracks `ThemeChanged` events, while `Light`/`Dark` stay fixed until changed in Settings.
- Configurable omnibox search provider with DuckDuckGo, Google, and Bing choices. The command palette and omnibox both use the selected provider for non-URL text, and fresh tabs now open that provider's homepage.
- The command palette now offers explicit **Search in this tab** and **Search in a new tab** choices for queries (and **Open in this tab** / **Open in a new tab** for addresses), listed ahead of matching tabs.
- Loading tabs show an animated native spinner in the favicon slot, replacing the static favicon until navigation completes; the duplicate row loading dot was removed.
- Settings entry points in the bottom action block of the expanded/collapsed rail, the macOS native **File → Settings…** menu, command palette quick action, CEF web-view shortcut forwarding, and the main-window keyboard handler. The web view also recognizes `Cmd/Ctrl+.` as an alias and falls back to CEF's UTF-16 character fields when `windows_key_code` is not populated for `Cmd+,`.
- On macOS, a local NSEvent monitor now captures shell shortcuts before AppKit/CEF can consume them, so `Cmd+,`, `Cmd+.`, and the other browser shortcuts work even while a web page has keyboard focus. The CEF-forwarded Settings request is also consumed by the main event loop instead of being left set.

### Changed

- The shared overlay now dispatches Settings, tab menu, and command palette input with clear priority; opening one closes the others.
- Changing the ad-block preference flips a shared atomic read by CEF request handlers, so blocking starts or stops without recreating browsers.
- Changing the hibernation timeout updates the live watchdog; high RAM pressure still shortens the timeout and critical pressure still hibernates all background tabs immediately.
- The rail startup preference is applied only when it changes: editing an unrelated setting no longer overrides a rail the user collapsed manually.
- Rail tooltip cards are wider and taller with more padding and larger text, and the Settings card has a roomier layout.
- The Settings card is now centered in the main browser window instead of pinned near the top, and the shared overlay follows the main window when it is moved to another display.
- Bumped the project and macOS bundle version from `0.3.0` to `0.4.0`.

### Fixed

- New tabs now use the selected search provider's homepage; choosing Google or Bing no longer leaves DuckDuckGo as the new-tab landing page.
- The command palette now selects its seeded active URL on the frame after the input gains focus, so typed searches replace the old URL instead of appending to a DuckDuckGo address.
- The Settings overlay is repositioned after it becomes visible and on main-window moves, so it opens on the same display as the browser rather than the primary display.
- Shutdown now finalizes CEF teardown once instead of re-logging readiness and rescheduling the one-shot observer on every reentrant tick; the app still exits cleanly with the SQLite WAL flushed.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 91 tests, including preference defaults/normalization/JSON round-trip, SQLite preference save/load fallback, theme override resolution, search-provider URL mapping and homepages, explicit this-tab/new-tab palette actions, loading-spinner rotation geometry, Settings overlay exclusivity, web-view settings shortcut forwarding with UTF-16 fallback, macOS native shortcut classification, secondary-display overlay positioning, deferred palette text selection, tooltip geometry, and pressure-aware custom idle timeouts.
- `cargo build --release`
- Manual macOS verification confirmed by the user: Settings opens centered on the current display, Google is used for new tabs and command-box searches, explicit this-tab/new-tab palette actions work, loading tabs show the favicon spinner, page-focus shortcuts work, the bottom-rail and File-menu Settings entry points work, tooltips are readable, preferences persist, and the browser quits cleanly.

---

## v0.3.0 — 2026-11-23

### Added

- Native tab context menu on rail rows in both expanded and favicon-only modes. It offers New tab, Duplicate tab, Copy URL, Reload tab, Close tab, Close other tabs, Close tabs below this tab, Reopen closed tab, and DevTools.
- The context menu renders in the existing transparent, always-on-top overlay above the CEF page. It does not dim the page, supports `↑`/`↓`/`Enter`/`Esc`, and dismisses on a primary click outside the card.
- `Cmd/Ctrl+Z` now undoes the last tab close, matching `Cmd/Ctrl+Shift+T`. Repeated presses restore earlier closed tabs in reverse-close order.
- The native rail, command palette, context menus, and tooltips now follow the OS light/dark appearance. Theme changes apply live without a restart, and the startup frame uses the current system theme.

### Changed

- Closed tabs now retain their title and original rail index, so restored tabs return to their previous position instead of always opening at the end.
- Duplicate tab now inserts the copy immediately to the right of the original tab.
- The command-palette overlay renderer is shared with the tab menu; opening one now explicitly closes the other.
- The tab context-menu command is now labeled "Close tabs below this tab" instead of the ambiguous "Close tabs to right".
- `F12` opens Chromium DevTools in its own window.
- Bumped the project and macOS bundle version from `0.2.0` to `0.3.0`.

### Fixed

- Context-menu row highlighting now uses the exact clickable row rect. Nested egui child widgets were registering their own hover areas and stealing pointer hover from the row, which made highlights appear only near the top edge.
- New and restored tabs now load reliably. CEF browser creation is queued from `focus_tab` and drained from `about_to_wait` instead of running inside a winit window-event callback, where macOS CEF's external pump could intermittently drop the initial navigation.
- If a browser still has an empty or `about:blank` main frame after creation, the tab's target URL is reissued with escalating delays (750 ms, 1.5 s, 3 s). A committed navigation cancels the retries, so healthy pages are not loaded twice.
- Fallback favicon downloads no longer start in `OnAfterCreated`. They wait until a real URL commits, avoiding a Chromium loader race that could intermittently leave new tabs blank.
- Rail tooltips now clear when the pointer leaves the rail or enters the CEF child view, so they no longer remain visible after the cursor moves away.
- Tooltips are limited to the collapsed rail, where they label icon-only buttons. Expanded rail buttons now all render visible text labels, including the collapse control.
- Fixed repeated `Cmd/Ctrl+T` (and the other shell shortcuts) after a web view gains focus. CEF now forwards `Cmd/Ctrl+T`, `Cmd/Ctrl+W`, `Cmd/Ctrl+K`/`Cmd/Ctrl+L`, `Cmd/Ctrl+Shift+T`, `Cmd/Ctrl+Z`, and `F12` to the native tab manager instead of trapping them in the child view.
- Fixed a macOS shutdown abort (`tried to handle event while another event is currently being handled`): `cef::shutdown()` now runs from a one-shot CFRunLoop observer after the winit event handler returns, while winit's run-loop observers remain alive.
- The ad blocker no longer cancels main-frame navigations; only subresources are filtered, preventing filter rules from breaking a page load.
- Reverted the experimental DevTools docking work; CEF 156/macOS native-parent docking proved unstable, and DevTools now consistently opens in a separate window.

### Verification

- `cargo fmt --all -- --check`
- `cargo check --bins`
- `cargo test --bin kasumi-browser` — 74 tests, including context-menu geometry/actions/enabled states, closed-tab metadata and restore-index clamping, initial-navigation recovery, tooltip pointer-exit handling, collapsed-only tooltip behavior, web-view shortcut forwarding, main-frame adblock exemption, `Cmd/Ctrl+Z` shortcut modifier rules, light/dark palette contrast, and palette/menu mutual exclusion.
- `cargo build --release`
- Manual runtime verification confirmed on macOS: `F12` opens Chromium DevTools in a separate window; dark and light system appearances switch the native chrome live; normal tab switching, resize, and quit behaved as expected.

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
- Full Windows and Linux validation
- More unit tests
- Settings UI
- Bookmark manager UI
- Download manager
- Automated cross-platform integration tests
- Release CI and signed artifact publishing
