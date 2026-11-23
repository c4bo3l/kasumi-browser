# Kasumi Browser

A high-performance, daily-driver desktop web browser written in **Rust** powered by the **Chromium Embedded Framework (CEF)** (`tauri-apps/cef-rs`).

Kasumi Browser is built from the ground up to eliminate browser bloat while maintaining 100% compatibility with modern web applications (YouTube, Google Docs, Figma, WebGL, WASM, WebRTC).

Repository: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser)

PR checks: [![PR checks](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml/badge.svg)](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml)

Release notes: [RELEASE_NOTES.md](RELEASE_NOTES.md)

---

## ✅ What Has Been Done

Current version: **0.5.1**. A version bump is released when a PR is merged into `main`; PRs include the bump in `Cargo.toml`, `src/bin/bundle.rs`, `AGENTS.md`, `README.md`, `RELEASE_NOTES.md`, and `Cargo.lock` so CI checks the exact release state, as required by `AGENTS.md`.

- Renamed the project from LiteBrowser to **Kasumi Browser** everywhere: package, binary, CEF helper, bundle ID, window title, database path, and docs.
- Implemented the CEF subprocess fork trap and `mimalloc` global allocator.
- Added unbundled macOS CEF support by setting `browser_subprocess_path` when the binary is not running from a `.app` bundle.
- Built the native chrome with `winit` + `wgpu` + `egui`: collapsible vertical workspace rail, floating command palette, ad-block shield badge, RAM readout, and hibernate-all button.
- Embedded the CEF browser as a native child view (`NSView` / `HWND` / `X11`), never OSR.
- Implemented tab creation, switching, closing, reopening, hibernation, and waking with session metadata kept in Rust.
- Added the `sysinfo` RAM watchdog with normal, pressure, and critical hibernation thresholds.
- Integrated `adblock-rust` request interception with built-in starter rules plus optional EasyList/EasyPrivacy files.
- Added SQLite WAL persistence for history, bookmarks, and session tabs, with low-RAM startup restore (only the active tab is spawned live).
- Wired keyboard shortcuts: `Ctrl/Cmd+T`, `Ctrl/Cmd+W`, `Ctrl/Cmd+K`/`Ctrl/Cmd+L`, `Ctrl/Cmd+,`, `Ctrl/Cmd+Shift+T`, `Ctrl/Cmd+Z`, `Alt+H`, and `F12`. Browser shortcuts are also forwarded from the CEF child view so they keep working while a page has focus.
- Added a native rail tab context menu with duplicate, pin/unpin, copy URL, reload, close, close others, close tabs below, reopen-closed-tab, and DevTools actions.
- Added persistent pinned tabs: pin from the rail context menu, command palette, or the rail pin badge; pinned tabs occupy a compact section at the top of both rail modes and are restored across restarts.
- `F12`, the rail **DevTools** button, the tab context menu, and the command palette all open Chromium DevTools in a separate window.
- Closed tabs now retain their title and original position, so `Cmd/Ctrl+Z` and `Cmd/Ctrl+Shift+T` restore them where they were.
- Browser creation is deferred out of window-event callbacks and blank initial frames are retried with escalating delays, so new and restored tabs reliably load their page on all platforms. Fallback favicon downloads wait for a committed URL, and main-frame navigations are never cancelled by the ad blocker.
- Rail tooltips appear only in collapsed mode, where they serve as labels for icon-only buttons. Expanded rail buttons all render visible text labels, and tooltips clear immediately when the pointer leaves the rail or enters the CEF child view.
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
- Added a native Settings overlay opened with `Cmd/Ctrl+,`, the bottom rail **Settings** button, the macOS **File → Settings…** menu item, or the command palette. It is centered in the main browser window on its current display, persists theme override, rail startup state, search provider, ad-block toggle, and hibernation idle timeout in SQLite WAL, and applies changes immediately.
- The selected search provider now also controls the new-tab landing page, and the command palette replaces the seeded active URL when you start typing, so changing to Google/Bing does not leave DuckDuckGo in the search or new-tab path. The palette now offers explicit **Search in this tab** and **Search in a new tab** choices for queries (and **Open in this tab** / **Open in a new tab** for addresses), with those actions listed before tab matches.
- Loading tabs now show an animated native spinner in the favicon slot instead of a static favicon; the spinner disappears as soon as the page finishes loading.
- Fixed the original chrome UX/navigation feedback items: Back/Forward icon states, omnibox URL synchronization, F12 DevTools, view-source navigation, favicons, and hover tooltips.
- Fixed DevTools refocusing after a window resize: the DevTools menu/F12 now calls `show_dev_tools` even when CEF reports an existing DevTools browser, which brings the existing window forward instead of doing nothing.
- Fixed middle-click and Ctrl+click link activation: CEF now cancels the source-tab navigation and opens a real Kasumi tab, with middle-click loading in the background and Ctrl+click in the foreground. Background tabs use CEF's cross-platform parent-window setup: macOS starts the `NSView` hidden, Windows clears `WS_VISIBLE`, and Linux unmaps the X11 child as soon as it is created. CEF window handles now convert between pointer and XID forms through one helper on every platform.
- Added 103 unit tests covering omnibox resolution, search-provider selection and homepages, rail/palette/menu/settings geometry and state, explicit this-tab/new-tab palette actions, pinned-tab ordering and menu/palette commands, loading-spinner rotation geometry, context-menu actions, middle-click/Ctrl+click tab disposition mapping, closed-tab metadata, initial-navigation recovery, tooltip pointer-exit handling, collapsed-only tooltip behavior, web-view shortcut forwarding (including UTF-16 character fallback), the macOS NSEvent shortcut monitor, secondary-display overlay positioning, deferred palette text selection, light/dark palette contrast and theme override, hibernation thresholds and background-spawn rules, adblock mapping, preference persistence, storage (including legacy session migration), and CEF switch invariants.
- Verified with `cargo fmt --all -- --check`, `cargo check --bins`, `cargo test --bin kasumi-browser` (103 tests), and `cargo build --release`. PR CI now runs the same checks and unit tests on macOS 15 Intel, Windows, and Linux; interactive macOS verification is performed before each release push.
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

- The workspace rail starts expanded at **180 px** with tabs, New tab, Command, DevTools, Hibernate, blocked-request count, and RAM usage. The `«` button collapses it to a **44 px favicon-only rail** that keeps tab count, active state, and click-to-switch available without text.
- A floating **Cmd/Ctrl+K command palette** replaces the traditional visible URL bar. It searches open tabs, resolves domains/searches, and exposes quick actions. It opens in a transparent always-on-top overlay so the page stays visible behind it, focuses its input immediately, and closes when you click outside the palette card.
- CEF content lives to the right of the rail and fills the full window height. Rail tooltips appear in a transparent, click-through overlay, vertically centered beside the hovered row, so the page stays visible and remains interactive.
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
    │   ├── schema.sql            # SQLite WAL schema (history, bookmarks, session_tabs, preferences)
    │   └── db.rs                 # Dedicated background Tokio/SQLite persistence worker
    └── ui/
        ├── mod.rs
        ├── platform_view.rs      # Cross-platform child window show/hide/resize
        └── chrome.rs             # egui + wgpu vertical rail, command palette, tab menu, and Settings
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

```bash
cargo run
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

## 🚧 Future Development

**Next up: workspaces.** Pinned tabs are now the foundation, so an Arc-style space or edge workspace strip is the natural next step for grouping tabs by project, task, or context.

- **Workspaces:** Arc-style Spaces or an edge workspace strip for grouping tabs by project, task, or context.
- **Per-workspace pins and bookmarks:** each workspace will own its own pinned-tab list and bookmarks so switching spaces restores the full context without mixing them.
- **Bookmark manager UI:** a native view for organizing, searching, and editing bookmarks.
- **Download manager:** a native panel for active and completed downloads.
- **Windows and Linux testing:** native child-view code exists for `HWND` and `X11`, but full interactive verification is still needed on those platforms.
- **More integration tests:** unit coverage is in place for omnibox resolution, rail/palette/menu/settings state, closed-tab restoration metadata, tooltips, hibernation thresholds, adblock mapping, preference persistence, storage, and CEF switches; automated end-to-end browser tests are still future work.
- **Packaged-app verification:** macOS, Windows, and Linux release artifacts still need interactive testing after signing/bundling.
- **Release CI and signed artifact publishing:** extend the existing PR checks to signed release builds and published artifacts.

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
