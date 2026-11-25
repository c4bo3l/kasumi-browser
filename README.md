# Kasumi Browser

A high-performance, daily-driver desktop web browser written in **Rust** powered by the **Chromium Embedded Framework (CEF)** (`tauri-apps/cef-rs`).

Kasumi Browser is built from the ground up to eliminate browser bloat while maintaining 100% compatibility with modern web applications (YouTube, Google Docs, Figma, WebGL, WASM, WebRTC).

Repository: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser)

PR checks: [![PR checks](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml/badge.svg)](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml)

Release notes: [RELEASE_NOTES.md](RELEASE_NOTES.md)

---

## ✅ What Has Been Done

Current version: **0.9.0**. A version bump is released when a PR is merged into `main`; PRs include the bump in `Cargo.toml`, `src/bin/bundle.rs`, `AGENTS.md`, `README.md`, `RELEASE_NOTES.md`, and `Cargo.lock` so CI checks the exact release state, as required by `AGENTS.md`.

- Renamed the project from LiteBrowser to **Kasumi Browser** everywhere: package, binary, CEF helper, bundle ID, window title, database path, and docs.
- Implemented the CEF subprocess fork trap and `mimalloc` global allocator.
- Added unbundled macOS CEF support by setting `browser_subprocess_path` when the binary is not running from a `.app` bundle.
- Built the native chrome with `winit` + `wgpu` + `egui`: collapsible vertical workspace rail, floating command palette, ad-block shield badge, RAM readout, and hibernate-all button. The expanded rail has an editable, copyable, bookmarkable address field at the top of the tab manager, with a native confirmation after copying, and the collapse/expand control now sits at the bottom of the rail beside the other status actions.
- Embedded the CEF browser as a native child view (`NSView` / `HWND` / `X11`), never OSR.
- Implemented tab creation, switching, closing, reopening, hibernation, and waking with session metadata kept in Rust.
- Added native **Incognito** mode: `Cmd/Ctrl+Shift+N`, the macOS **Kasumi Browser → New Incognito Window** menu item, the rail incognito buttons, and the command palette all open one private workspace backed by an ephemeral CEF request context with an empty cache path. Private tabs never write history, sessions, workspaces, bookmarks, or downloads to SQLite; popups, middle-click tabs, duplicates, reopened tabs, and view-source navigations inherit private isolation, and closing the last private tab discards the workspace and request context.
- Reduced both bottom page HUD strips to **24 DIPs** with 18×18 icon buttons, a 2 DIP icon inset, 2 DIP vertical margin, and 9 pt numeric labels, so hover tooltips stay readable without covering page content.
- Added the `sysinfo` RAM watchdog with normal, pressure, and critical hibernation thresholds.
- Integrated `adblock-rust` request interception with built-in starter rules plus optional EasyList/EasyPrivacy files.
- Added SQLite WAL persistence for history, bookmarks, downloads, and session tabs, with low-RAM startup restore (only the active tab is spawned live).
- Wired keyboard shortcuts: `Ctrl/Cmd+T`, `Ctrl/Cmd+W`, `Ctrl/Cmd+K`/`Ctrl/Cmd+L`, `Ctrl/Cmd+J`, `Ctrl/Cmd+,`, `Ctrl/Cmd+Shift+T`, `Ctrl/Cmd+Shift+N`, `Ctrl/Cmd+Z`, `Alt+H`, `F12`, and `Ctrl/Cmd+Q`. Browser shortcuts are also forwarded from the CEF child view so they keep working while a page has focus; `Ctrl/Cmd+Q` goes through the shell's graceful shutdown path and flushes CEF and SQLite before exit.
- Added a native rail tab context menu with duplicate, pin/unpin, copy URL, reload, close, close others, close tabs below, reopen-closed-tab, and DevTools actions.
- Added persistent pinned tabs: pin from the rail context menu, command palette, or the rail pin badge; pinned tabs occupy a compact section at the top of both rail modes and are restored across restarts.
- Added native Workspaces: each workspace owns its pinned and regular tabs, remembers its active tab, and can be switched from the rail or collapsed favicon strip. Workspaces persist across restarts, and session startup reopens the last active workspace and loads only its active tab live. The collapsed rail uses large, centered icon buttons with accent borders, even spacing between spaces, a current-space indicator, and name tooltips so switching spaces is easy without expanding the rail. Its RAM and ad-block status icons now scale up to 20 px in dedicated badge rows with value tooltips instead of staying at the expanded rail's 15 px size. The redundant `KASUMI` rail title was removed.
- The native Settings dialog has dedicated **General**, **Workspaces**, and **Bookmarks** pages. The Workspaces page lists every workspace with Default/Active badges and edits the selected workspace with Material Symbols icon buttons; **Save changes** commits that workspace. Creation is a separate mode opened by **New workspace** or the rail `+`: the create form replaces the list/editor instead of appearing beside it, **Back** returns without creating, and a successful create returns to the list with the new workspace selected. Accent colours tint native chrome accents only (rail, palette, tab borders, loading indicator, and tooltips), never CEF page content.
- Added a native download manager with no save prompt, no OSR view, and no second renderer. CEF resolves the destination, then HTTP(S) transfers are taken over by Kasumi's native segmented engine using up to eight parallel `Range` requests and a background part-file writer. Downloads are tracked in an Arc/Edge-style native bubble (not a Settings page): the header aggregates active/done/failed transfers and overall progress. Active rows show a progress bar with icon-only pause/resume and cancel buttons at its right edge; finished rows drop the bar and swap those icons for open-file and show-in-folder. Each row also shows speed, ETA, path, connection count, retry, and remove-info controls, and the bubble header shows an aggregate ETA alongside overall speed. The rail Downloads button carries an active-transfer count badge (or a quiet history dot), so download activity is visible in both rail modes. Open the bubble from the rail button, `Cmd/Ctrl+J`, the macOS **Kasumi Browser** app menu, or the command palette; a new transfer reveals it automatically. A click-through native notification confirms completed downloads and reports interrupted transfers without stealing focus from the page. Stale downloads are marked interrupted after a restart instead of showing dead controls. The **General** settings page has a **Downloads** section: set a custom download folder (empty means the standard Downloads folder) and enable **Ask where to save each file** for a native save dialog on every transfer. While **Ask where to save each file** is enabled, the download-folder field is disabled because the save dialog owns the destination. Download-row action icons are small and transparent, with hover colour used for feedback. Failed finalizations are demoted to Interrupted instead of claiming a file that does not exist. Downloads keep the server's suggested filename and extension, including when the native segmented engine takes over from CEF.
- Any workspace can be selected as the **default workspace** with the **Make default** button on the Workspaces page. **Settings → General → Startup** controls what opens on launch: **Last used space** (the default) restores the space open at shutdown, while **Default space** always opens the workspace chosen in Workspaces. Switching workspaces during the current session still takes effect immediately, and deleting the default workspace automatically promotes another workspace.
- Bookmarks are saved into a folder named after the active workspace but stay globally visible from every workspace; renaming renames the folder, and deleting archives it as `Archived {name}` instead of deleting bookmarks. Open the dedicated bookmark manager with `Cmd/Ctrl+Shift+B`, the command palette's **Open bookmark manager** command, or the rail **Settings** button. It searches by title/URL/folder and supports editing the title/URL, moving between folder options, and deleting individual rows with icon-only actions. The expanded rail address field and command palette toggle the current page: an outlined star means it is not bookmarked, a filled accent star means it is saved, and clicking again removes it with a native confirmation.
- `F12`, the rail **DevTools** button, the tab context menu, and the command palette all open Chromium DevTools in a separate window.
- Closed tabs now retain their title and original position, so `Cmd/Ctrl+Z` and `Cmd/Ctrl+Shift+T` restore them where they were.
- Browser creation is deferred out of window-event callbacks and blank initial frames are retried with escalating delays, so new and restored tabs reliably load their page on all platforms. Fallback favicon downloads wait for a committed URL, and main-frame navigations are never cancelled by the ad blocker.
- Rail tooltips appear only in collapsed mode, where they serve as labels for icon-only buttons. They are compact and translucent, so page content stays readable through the native overlay. Expanded rail buttons all render visible text labels, and tooltips clear immediately when the pointer leaves the rail or enters the CEF child view. The expanded rail's address field is now a framed surface with a globe glyph plus compact bookmark and copy actions; its width is computed from the remaining space and long URLs scroll inside the field, so it stays inside the rail with even side margins. Copying shows a transient native **Copied** confirmation, and the star shows **Bookmark saved** or **Bookmark removed** as it toggles.
- Fixed the macOS shutdown reentrancy abort: `cef::shutdown()` now runs from a one-shot CFRunLoop observer after the winit event handler returns, while run-loop observers are still alive.
- Fixed the macOS CEF/winit reentrancy crash: CEF is pumped from a main-thread `CFRunLoop` timer, never from a `window_event` handler, and reentrant `about_to_wait` ticks are ignored.
- Fixed macOS shutdown ordering: browsers and WAL are flushed in `about_to_wait`, the pump timer is disabled, and `cef::shutdown()` runs while winit's run-loop observers still exist.
- Fixed `do_close` to return `1` so CEF actually destroys browsers and fires `on_before_close`.
- Fixed Retina layout: `ContentRect` is logical/DIP-based, macOS respects the flipped winit `NSView`, and Windows/X11 convert to physical pixels.
- Unified native child-view handling across macOS `NSView`, Windows `HWND`, and X11/XCB windows, including rail-relative CEF viewport geometry.
- Added release packaging and code-signing documentation.
- Added `.gitignore` to keep `target/`, local CEF files, machine-specific Cargo config, logs, and OS/editor files out of the repository.
- Added `RELEASE_NOTES.md` as the canonical record of shipped changes.
- Published the source as a public GitHub repository.
- Added a native Settings overlay opened with `Cmd/Ctrl+,`, the bottom rail **Settings** button, the macOS **Kasumi Browser → Settings…** app-menu item, or the command palette. It is centered in the main browser window on its current display, persists theme override, rail state, search provider, ad-block toggle, and hibernation idle timeout in SQLite WAL, and applies changes immediately. Collapsing or expanding the rail from the rail button, command palette, or keyboard shortcut now saves that state immediately, so the next launch restores it. The macOS application menu now uses the product name **Kasumi Browser** even for unbundled development binaries, and its actions live directly in the app menu instead of a separate File menu.
- The selected search provider now also controls the new-tab landing page, and the command palette replaces the seeded active URL when you start typing, so changing to Google/Bing does not leave DuckDuckGo in the search or new-tab path. The palette now offers explicit **Search in this tab** and **Search in a new tab** choices for queries (and **Open in this tab** / **Open in a new tab** for addresses), with those actions listed before tab matches.
- Loading tabs now show an animated native spinner in the favicon slot instead of a static favicon; the spinner disappears as soon as the page finishes loading.
- Fixed the original chrome UX/navigation feedback items: Back/Forward icon states, omnibox URL synchronization, F12 DevTools, view-source navigation, favicons, and hover tooltips.
- Fixed DevTools refocusing after a window resize: the DevTools menu/F12 now calls `show_dev_tools` even when CEF reports an existing DevTools browser, which brings the existing window forward instead of doing nothing.
- Fixed middle-click and Ctrl+click link activation: CEF now cancels the source-tab navigation and opens a real Kasumi tab, with middle-click loading in the background and Ctrl+click in the foreground. Background tabs use CEF's cross-platform parent-window setup: macOS starts the `NSView` hidden, Windows clears `WS_VISIBLE`, and Linux unmaps the X11 child as soon as it is created. CEF window handles now convert between pointer and XID forms through one helper on every platform.
- Updated the dependency set to current releases: `egui`/`egui-wgpu`/`egui-winit` 0.37, `egui_material_icons` 0.9, `wgpu` 30, `rusqlite` 0.43, `sysinfo` 0.40, `objc2` 0.6, `objc2-app-kit`/`objc2-foundation` 0.3, `block2` 0.6, `windows-sys` 0.61, `x11rb` 0.14, and `pollster` 1, plus CEF `156.7.0+156.0.31`, `adblock` `0.13.6`, and `url` `2.5.9`.
- Fixed macOS HUD/rail tooltips losing hover delivery after the CEF page takes keyboard focus. The shell now monitors pointer moves with a local `NSEvent` monitor and a low-frequency pointer poll, forwards them into egui, and keeps the tooltip overlay above the page without stealing page input.
- Added 187 unit tests covering incognito workspace reuse/navigation/popup isolation/SQLite bypass/context teardown, omnibox resolution, bookmark search/URL normalization and manager layout, bookmark move duplicate guards, bookmark rail/palette actions, toggle/un-bookmark state, and native confirmations, terminal download notification transitions and toast queueing, search-provider selection and homepages, rail/palette/menu/settings geometry and state, explicit this-tab/new-tab palette actions, pinned-tab ordering and menu/palette commands, workspace tab/pin/bookmark ownership, accent tinting, stale-workspace normalization, configurable last-active/default workspace startup selection, default-workspace selection/deletion and empty-workspace restore behavior, separate workspace edit/create drafts and dedicated create-mode state, collapsed workspace switching and compact translucent tooltips, collapsed status-badge sizing, Settings page selection, downloads-bubble state/aggregation/anchor/finite-layout behavior, finished-row action swapping and rail download badges, loading-spinner rotation geometry, context-menu actions, middle-click/Ctrl+click tab disposition mapping, closed-tab metadata, initial-navigation recovery, tooltip pointer-exit handling, 24 DIP bottom HUD geometry, collapsed-only tooltip behavior, winit/CEF/macOS incognito shortcut classifiers, web-view shortcut forwarding (including UTF-16 character fallback), the macOS NSEvent shortcut monitor, graceful `Cmd/Ctrl+Q` routing, secondary-display overlay positioning, deferred palette text selection, light/dark palette contrast and theme override, hibernation thresholds and background-spawn rules, adblock mapping, download state/storage/restart behavior, ETA formatting, parallel segment planning, range-header parsing, eligibility, part-file writing, CEF download target paths, preference and rail-state persistence, storage (including legacy session, bookmark/workspace, and parallel-download migrations), and CEF switch invariants.
- Verified with `cargo fmt --all -- --check`, `cargo check --bins`, `cargo test --bin kasumi-browser` (187 tests), and `cargo build --release`. PR CI now runs the same checks and unit tests on macOS 15 Intel, Windows, and Linux; interactive macOS verification is performed before each release push.
- Removed the obsolete `BUGFIX_TASKS.md` handoff; its completed items are recorded in `RELEASE_NOTES.md`.

---

## ⚡ Core Engineering Pillars

1. **Ultra-Low Memory Footprint:**
   - **ZERO WebUI:** Never renders browser chrome using HTML/CSS/JS or secondary CEF views. All tabs, URL bars, buttons, and telemetry badges are drawn natively in Rust using `winit` + `wgpu` + `egui`, keeping browser shell RAM under `20 MB`.
   - **Aggressive Tab Hibernation:** Background tabs are completely closed at the CEF `BrowserHost` level (`host.close_browser(1)`) after an idle timeout or under memory pressure, freeing 100% of their Blink/V8 OS process memory while preserving session state in SQLite.
   - **Process Consolidation:** Hard-capped renderer processes (`--renderer-process-limit=6`) and domain sharing (`--process-per-site`).
   - **Telemetry Stripping:** Unnecessary background services disabled via CLI flags (`disable-background-networking`, `disable-sync`, `disable-speech-api`, `disable-component-update`, `disable-breakpad`).
2. **Network-Level Ad & Tracker Blocking:**
   - Powered by Brave's native Rust `adblock` engine (`adblock-rust`).
   - Network requests are intercepted inside `ResourceRequestHandler::on_before_resource_load` on Chromium's `TID_IO` thread and cancelled *before* Blink downloads or compiles scripts/trackers.
   - Live blocked count displayed per tab in the omnibox shield badge.
3. **Daily-Driver Ergonomics & Crash-Safe Persistence:**
   - Fast SQLite persistence in **WAL mode** for browsing history, bookmarks, and open session tabs.
   - **Instant Startup:** On launch, only the active tab is spawned live in CEF; all restored session tabs are loaded directly into `Hibernated` state, guaranteeing instant launch without memory spikes.
   - Full keyboard-first navigation shortcuts.
4. **Zero-Copy Native Embedding:**
   - Native OS child window viewport embedding (`NSView` on macOS, `HWND` on Windows, `X11` on Linux) beneath the egui chrome.
   - Never uses Off-Screen Rendering (OSR), preserving direct zero-copy GPU compositing and hardware video decoding.

---

## 🧭 Browser Layout

Kasumi uses a minimalist vertical layout instead of a Chrome-style top tab strip:

- The workspace rail starts expanded at **208 px** with a workspace switcher, pinned tabs, a **New tab** button directly below the pinned group, regular tabs, Hibernate, and collapse controls. Command and DevTools live in a 24 DIP bottom-left page HUD, while blocked-request count and RAM usage live in a 24 DIP bottom-right page HUD. The extra width keeps tab titles and the framed address field comfortable. The `«` button collapses it to a **44 px favicon-only rail** that keeps coloured workspace icons, tab icons, active state, and click-to-switch available without text. Right-clicking an unpinned tab offers **Move tab to** another workspace.
- A floating **Cmd/Ctrl+K command palette** replaces the traditional visible URL bar. It searches open tabs, resolves domains/searches, and exposes quick actions. It opens in a transparent always-on-top overlay so the page stays visible behind it, focuses its input immediately, and closes when you click outside the palette card.
- CEF content lives to the right of the rail and fills the window height above the reserved bottom HUD strip. Rail tooltips appear in a transparent, click-through overlay, vertically centered beside the hovered row, so the page stays visible and remains interactive.
- Right-clicking a rail tab opens a native context menu in the same transparent overlay used by the palette. It stays above the page without dimming it, supports `↑`/`↓`/`Enter`/`Esc`, and closes on a primary click anywhere outside the card.
- Rail controls use Material Symbols icons for crisp, consistent glyphs at every DPI. Expanded controls have visible labels; tooltips are reserved for the collapsed icon rail. Each tab shows a hover-only **×** close button (right side when expanded, top-right when collapsed).
- The rail, command palette, context menus, tooltips, and Settings overlay follow the OS light/dark appearance automatically. System theme changes apply live without restarting the browser, web pages receive the same system color preference from Chromium, and Settings can override the appearance with a fixed Light or Dark choice.
- A native **Settings** card opens with `Cmd/Ctrl+,`, the rail button, or the command palette in the same transparent overlay. It keeps the page visible behind it, saves changes automatically to SQLite, and closes on `Esc`, **Done**, or a primary click outside the card.

`F12` opens Chromium DevTools for the active tab in its own window.

---

## ⌨️ Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Ctrl / Cmd + T` | Open new tab |
| `Ctrl / Cmd + W` | Close active tab |
| `Ctrl / Cmd + ,` (or `Ctrl / Cmd + .`) | Open native Settings |
| `Ctrl / Cmd + K` | Open the floating command palette |
| `Ctrl / Cmd + L` | Open the command palette seeded with the active URL |
| `Ctrl / Cmd + Shift + T` | Re-open last closed tab |
| `Ctrl / Cmd + Shift + N` | Open the incognito space (or another private tab when already incognito) |
| `Ctrl / Cmd + Shift + C` | Copy the active page URL to the clipboard |
| `Ctrl / Cmd + Z` | Undo the last tab close and restore its position |
| `Alt + H` | Hibernate all background tabs immediately |
| `F12` | Open Chromium DevTools for the active tab in a new window |
| `↑` / `↓`, `Enter`, `Esc` (in palette or tab menu) | Select, run, or close a command |
| `Enter` (in palette) | Navigate to URL, domain (auto-prefixes `https://`), or a search using the provider selected in Settings |

---

## 🏗️ Architecture

```text
kasumi-browser/
├── AGENTS.md                     # Master engineering specification & rules
├── Cargo.toml                    # Dependencies & release profile (Thin LTO, mimalloc)
├── assets/
│   └── filters/                  # Optional EasyList / EasyPrivacy filter files
└── src/
    ├── main.rs                   # MiMalloc, CEF subprocess fork trap, winit event loop
    ├── config.rs                 # Persisted preferences, thresholds, rail widths, palette geometry
    ├── events.rs                 # Lock-free BrowserEvent & UiAction bridges
    ├── app/
    │   ├── mod.rs                # KasumiBrowserApp (cef::App implementation)
    │   └── flags.rs              # De-bloating Chromium CLI switches
    ├── browser/
    │   ├── mod.rs
    │   ├── client.rs             # TabClient (LifeSpanHandler, LoadHandler, DisplayHandler)
    │   ├── adblocker.rs          # Brave adblock engine & ResourceRequestHandler
    │   └── hibernation.rs        # TabManager, TabState machine, sysinfo RAM watchdog
    ├── storage/
    │   ├── mod.rs
    │   ├── schema.sql            # SQLite WAL schema (workspaces, history, bookmarks, session_tabs, preferences)
    │   └── db.rs                 # Dedicated background Tokio/SQLite persistence worker
    └── ui/
        ├── mod.rs
        ├── platform_view.rs      # Cross-platform child window show/hide/resize
        └── chrome.rs             # egui + wgpu workspace rail, command palette, tab menu, and Settings
```

---

## 🚀 Building & Running

### Prerequisites

1. **Rust:** Latest stable Rust toolchain (`rustup update stable`).
2. **CEF Framework:**
   - By default, `cef-rs` automatically fetches matching CEF binaries for your platform if `CEF_PATH` is not explicitly set.
   - Alternatively, copy `.cargo/config.toml.example` to `.cargo/config.toml` and point `CEF_PATH` to a local CEF directory:
     ```toml
     [env]
     CEF_PATH = "/path/to/cef_distribution"
     ```
   - `.cargo/config.toml` is gitignored because it contains machine-specific absolute paths.

### Development Run

The easiest way to launch Kasumi in dev mode is the `dev` script (or `make dev`), which is the Rust equivalent of `npm run dev`:

```bash
./scripts/dev.sh            # build + run with RUST_LOG=info
make dev                    # same thing, via Makefile
./scripts/dev.sh --clean    # clear the CEF cache first
./scripts/dev.sh --release  # optimized build + run
./scripts/dev.sh --watch    # auto-restart on source changes (install cargo-watch first)
./scripts/dev.sh --log=debug  # override the log level
```

On Windows, use `scripts\dev.bat` instead.

You can also run the raw Cargo command directly:

```bash
cargo run --bin kasumi-browser
```

### Release Build

```bash
cargo build --release
```

The optimized binary with Thin LTO and symbols stripped will be placed in `target/release/kasumi-browser`.

### macOS Application Bundle (`.app`)

On macOS, Chromium requires helper subprocess bundles and standard framework directory layouts when distributing as a standalone `.app`.

1. Install `bundle-cef-app`:
   ```bash
   cargo install bundle-cef-app
   ```
2. Build the app bundle:
   ```bash
   bundle-cef-app \
     --bundle-path target/release/KasumiBrowser.app \
     --binary-path target/release/kasumi-browser \
     --framework-path "$CEF_PATH/Chromium Embedded Framework.framework"
   ```
3. Launch the packaged bundle:
   ```bash
   open target/release/KasumiBrowser.app
   ```

> The repository also contains `cargo run --bin bundle`, a thin wrapper around `cef::build_util::mac::bundle` that writes `target/bundle/KasumiBrowser.app`. You can use either path; `bundle-cef-app` is the documented external tool and the in-repo binary is the project-native option.

---

## 🔏 Release Build Code Signing

Sign the final bundled app/executables **after** the release build and CEF bundling are complete. Never commit certificates, passwords, or signing identities into the repository.

### macOS — Developer ID + notarization

Prerequisites:

- Apple Developer Program membership.
- A **Developer ID Application** certificate installed in the Keychain.
- An app-specific password stored with `xcrun notarytool store-credentials`.
- The CEF framework and all helper apps must be inside `KasumiBrowser.app` before signing.

1. Build and bundle:

   ```bash
   cargo build --release
   cargo run --release --bin bundle
   ```

2. Sign nested CEF helpers/frameworks first, then the outer app:

   ```bash
   IDENTITY="Developer ID Application: Your Name (TEAMID)"
   APP="target/bundle/KasumiBrowser.app"

   find "$APP" -type f -perm +111 -print0 | while IFS= read -r -d '' file; do
     case "$file" in
       *.framework/*|*Helper*|*.app/Contents/MacOS/*)
         codesign --force --timestamp --options runtime --sign "$IDENTITY" "$file"
         ;;
     esac
   done

   codesign --force --deep --timestamp --options runtime \
     --sign "$IDENTITY" "$APP"

   codesign --verify --deep --strict --verbose=2 "$APP"
   ```

3. Notarize and staple:

   ```bash
   ditto -c -k --keepParent "$APP" KasumiBrowser.zip

   xcrun notarytool submit KasumiBrowser.zip \
     --keychain-profile "kasumi-notarization" \
     --wait

   xcrun stapler staple "$APP"
   spctl --assess --type execute --verbose=4 "$APP"
   ```

4. Distribute the stapled `.app` or a new archive created from it.

### Windows — Authenticode

Prerequisites:

- An EV or standard code-signing certificate (`PFX`/`P12`) and its password.
- Windows SDK `signtool.exe` available on `PATH`.

```powershell
$PFX = "C:\secure\kasumi-code-signing.pfx"
$PASSWORD = $env:PFX_PASSWORD
$exe = "target\release\kasumi-browser.exe"

signtool sign /f $PFX /p $PASSWORD /fd SHA256 `
  /tr http://timestamp.digicert.com /td SHA256 $exe

signtool verify /pa /v $exe
```

Sign every shipped `.exe` (including CEF helper executables, if present) with the same certificate and timestamp server.

### Linux

Linux has no single universal desktop code-signing standard. Distribute checksums and GPG signatures for release archives:

```bash
gpg --armor --detach-sign --output kasumi-browser.tar.gz.asc kasumi-browser.tar.gz
sha256sum kasumi-browser.tar.gz > kasumi-browser.tar.gz.sha256
```

---

## 🐞 Known Issues

- **Windows and Linux interactive checks:** native child-view, tooltip, download, and bookmark flows still need hands-on verification on those platforms.

---

## 🚧 Future Development

**Priority queue:** 1) Google Meet / WebRTC calls, 2) Native print support, 3) Import from major browsers, 4) Cross-platform verification. **Next up: Google Meet / WebRTC calls.**

- **Google Meet / WebRTC calls:** bridge CEF permission callbacks into the native egui chrome, add screen-share source selection, and handle macOS camera/microphone entitlements.
- **Native print support:** bridge CEF's print callbacks into the native egui chrome and the platform print dialog on macOS, Windows, and Linux. Support `Cmd/Ctrl+P`, print preview, page range, copies, and landscape/orientation, plus “Save as PDF” with a native destination picker. Print jobs should capture the active tab without waking hibernated background tabs or introducing an OSR/second renderer.
- **Import from major browsers:** add a native first-run/import flow that pulls bookmarks, history, and saved passwords from Chrome, Edge, Firefox, Safari, and Brave profiles, with a preview and conflict-safe merge into Kasumi's SQLite WAL stores.
- **Cross-platform verification:**
  - **Windows and Linux testing:** native child-view code exists for `HWND` and `X11`, but full interactive verification is still needed on those platforms.
  - **Packaged-app verification:** macOS, Windows, and Linux release artifacts still need interactive testing after signing/bundling.
  - **Release CI and signed artifact publishing:** extend the existing PR checks to signed release builds and published artifacts.
  - **More integration tests:** unit coverage is in place for omnibox resolution, rail/palette/menu/settings state, closed-tab restoration metadata, tooltips, hibernation thresholds, adblock mapping, preference persistence, storage, and CEF switches; automated end-to-end browser tests are still future work.
- **Auto-update via GitHub Releases:** check the latest published release, download and verify the signed platform artifact, and install it from a native update prompt with a fallback to the releases page.
- **Picture-in-picture (PiP) out of the box:** make `<video>` PiP work without user setup on all three platforms. Prefer CEF/Chromium's built-in PiP for native child views; if a CEF milestone does not surface a PiP window, add a native always-on-top video overlay without introducing an OSR CEF view or a second renderer process.
- **Tiny local AI:** explore a sub-0.4B on-device model for omnibox intent routing, tab grouping, and short structured extraction without sending page data to the cloud.
- **Automated CEF/Chromium engine updates:** add a scheduled check that opens a review PR when a newer `cef` crate is published, instead of silently swapping the engine at runtime.
- **Google account sync:** optional end-to-end-encrypted sync of bookmarks, history, and preferences to a Google account, with explicit opt-in, per-device control, and conflict resolution so the low-RAM local-first design stays intact.
- **Chrome Web Store compatibility:** evaluate a CEF-compatible extension bridge so Chrome Web Store extensions can run in Kasumi, without reintroducing a WebUI or second renderer; document which extension APIs are supported and fall back gracefully for the rest.
- **Extension manager:** a native panel to install, enable/disable, update, and remove extensions, inspect their permissions, and control per-site access, all drawn with the existing egui chrome rather than an HTML page.

### Picture-in-picture (PiP) — research notes

- **Start with the browser engine:** verify whether CEF 156's native-window mode already honours `HTMLVideoElement.requestPictureInPicture()` and creates a Chromium PiP window. Keep `PictureInPicture` out of `--disable-features`, and avoid switches that strip media playback or overlay support.
- **Do not use OSR as a fallback:** the sub-20 MB shell target and zero-copy compositing depend on native child views. If engine PiP is unavailable, the fallback should be a small `winit` overlay window that reuses the existing GPU device and presents the video frame without creating another CEF browser.
- **Native integration points:** expose PiP from the rail context menu and command palette, track enabled state per tab, and route Enter/Close through the existing `UiAction` → `TabManager` message path. Reuse `platform_view.rs` for parent/owner and resize handling on `NSView`, `HWND`, and X11.
- **Lifecycle and memory:** PiP must survive tab switches and workspace changes, keep audio/video state in the original CEF browser, and be released when the tab closes or hibernates. Add interactive checks for drag/resize, always-on-top stacking, focus/click-through, and macOS/Windows/Linux window ordering.

### Incognito mode — research notes

- **Engine isolation:** create a separate CEF request context with a unique identifier and in-memory storage for private browsers. Verify against CEF 156 that cookies, HTTP cache, IndexedDB, localStorage, and service workers stay isolated from the standard context and from other private windows.
- **Native chrome only:** keep the zero-WebUI rule by representing private windows as native `TabManager` state, with an unmistakable incognito badge in the rail and Settings. Do not add a second CEF view for chrome or a wrapper profile UI.
- **Storage boundary:** route private tabs through an ephemeral storage sink so navigation history, bookmarks, downloads, and session snapshots never reach the WAL database. Only non-private user actions should persist; normal workspaces remain untouched.
- **Lifecycle and verification:** close every private `BrowserHost` and its request context when the last private tab closes; add interactive checks that cookies do not leak, downloads disappear on exit, crashes leave no private session file, and hibernation/wake preserves privacy isolation.

### Google Meet / WebRTC calls — research notes

- **Permission bridge:** CEF permission callbacks are async and reference-counted. The CEF handler should send an event containing the callback handle over the existing lock-free `crossbeam_channel` to the main UI thread; the main thread parks it in `BrowserUiState`, egui renders a next-frame prompt, and Allow/Deny resolves the callback exactly once. Do not lock a `std::sync::Mutex` from a CEF callback, and call `egui_ctx.request_repaint()` so the prompt appears immediately even when the pointer is idle.
- **Media access (`getUserMedia`):** Google Meet requests microphone/camera through the media-access permission callback. Decode the requested bitmask (`audio = 1 << 0`, `video = 1 << 1`) into a human-readable description, then grant with the exact requested mask or cancel.
- **Site permissions:** notification/clipboard prompts arrive through the permission-prompt callback and should use the same pending-request state machine with Accept/Deny results.
- **Screen sharing (`getDisplayMedia`):** Kasumi uses native child views, not OSR, which avoids the common picker-parenting failure. The Alloy runtime is still the default in current CEF builds; enabling the Chrome runtime may provide Chromium's native desktop-media picker where supported. Otherwise, enumerate monitors/windows (for example with `xcap`) and render an egui picker grid, or use `--auto-select-desktop-capture-source="Entire screen"` for kiosk/testing builds.
- **Platform gotchas:** macOS bundles need `NSMicrophoneUsageDescription` and `NSCameraUsageDescription` in `Info.plist`, plus `com.apple.security.device.audio-input` and `com.apple.security.device.camera` entitlements. Persist `(origin, permission mask)` decisions in SQLite so known origins can be auto-resolved without showing the modal on every restart.

### Tiny local AI — research notes

- **Use cases:** omnibox intent routing (`search` / `command` / `url`), tab categorization, and short JSON extraction. Keep inference on an idle background worker, load the model on demand, and unload it after inactivity so the sub-20 MB shell target is preserved.
- **Candidate models:**

  | Model | Params | `Q8_0` GGUF | Best for | Caveat |
  | --- | --- | --- | --- | --- |
  | `LFM2.5-230M` | 230M | ~245 MB | Structured routing, JSON/schema extraction, tab grouping | Newer architecture; confirm `llm2` support in the chosen `llama.cpp` build |
  | `Gemma-3-270M-it` | 270M | ~285 MB | Multilingual titles, short text cleanup | Large vocabulary; give 2–3 few-shot examples for strict JSON |
  | `SmolLM2-360M-Instruct` | 360M | ~385 MB | Broad backend compatibility, fast CPU inference | Larger RAM footprint; prefer 360M over 135M for classification |

- **Inference tips:** prefer `Q8_0` at this scale because 4-bit quant saves little RAM but degrades structured output; use GBNF grammar-constrained sampling so output is valid JSON or an enum label; cap `n_ctx` at 2048 tokens to keep the KV cache under ~15 MB.

### CEF / Chromium engine updates — proposed process

- Keep the `cef` crate pinned in `Cargo.toml` and the canonical `AGENTS.md` snippet. The matching CEF binaries are fetched from that version, so a crate bump is the engine upgrade.
- Add a scheduled workflow (weekly plus manual `workflow_dispatch`) that queries crates.io for the newest stable `cef` release. When one exists, open a PR that bumps the crate, lockfile, canonical docs, and app version through one shared bump script.
- Let Dependabot/Renovate handle ordinary Rust crates, but exclude `cef` from auto-merge: generated FFI signatures and OS-event types change between Chromium milestones, so engine bumps need human review.
- Gate engine PRs on the full macOS/Windows/Linux CI matrix plus a runtime smoke test (page load, WebGL/WASM, and a media-permission prompt). Label them `cef-update` and record the exact CEF/Chromium version in the release notes for rollback.
- Ship engine upgrades through signed GitHub Releases artifacts and the auto-update path, not by replacing CEF binaries inside an already installed app.

---

## 🛡️ Adblock Filter Lists

Kasumi Browser includes built-in fallback rules covering major ad and tracker networks. To load the full EasyList and EasyPrivacy rule sets:
1. Create `assets/filters/`
2. Download `easylist.txt` and `easyprivacy.txt`:
   ```bash
   mkdir -p assets/filters
   curl -Lo assets/filters/easylist.txt https://easylist.to/easylist/easylist.txt
   curl -Lo assets/filters/easyprivacy.txt https://easylist.to/easylist/easyprivacy.txt
   ```
On startup, Kasumi Browser will automatically detect and compile these rules into the native adblocker engine.
