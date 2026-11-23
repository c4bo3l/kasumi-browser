# Kasumi Browser

A high-performance, daily-driver desktop web browser written in **Rust** powered by the **Chromium Embedded Framework (CEF)** (`tauri-apps/cef-rs`).

Kasumi Browser is built from the ground up to eliminate browser bloat while maintaining 100% compatibility with modern web applications (YouTube, Google Docs, Figma, WebGL, WASM, WebRTC).

Repository: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser)

Release notes: [RELEASE_NOTES.md](RELEASE_NOTES.md)

Pending UI/navigation bugfix handoff: [BUGFIX_TASKS.md](BUGFIX_TASKS.md)

---

## ✅ What Has Been Done

Current version: **0.1.5**. Every batch of changes bumps the version in `Cargo.toml`, `src/bin/bundle.rs`, `AGENTS.md`, and `Cargo.lock` as required by `AGENTS.md`.

- Renamed the project from LiteBrowser to **Kasumi Browser** everywhere: package, binary, CEF helper, bundle ID, window title, database path, and docs.
- Implemented the CEF subprocess fork trap and `mimalloc` global allocator.
- Added unbundled macOS CEF support by setting `browser_subprocess_path` when the binary is not running from a `.app` bundle.
- Built the native chrome with `winit` + `wgpu` + `egui`: horizontal tab strip, toolbar, omnibox, ad-block shield badge, RAM readout, and hibernate-all button.
- Embedded the CEF browser as a native child view (`NSView` / `HWND` / `X11`), never OSR.
- Implemented tab creation, switching, closing, reopening, hibernation, and waking with session metadata kept in Rust.
- Added the `sysinfo` RAM watchdog with normal, pressure, and critical hibernation thresholds.
- Integrated `adblock-rust` request interception with built-in starter rules plus optional EasyList/EasyPrivacy files.
- Added SQLite WAL persistence for history, bookmarks, and session tabs, with low-RAM startup restore (only the active tab is spawned live).
- Wired keyboard shortcuts: `Ctrl/Cmd+T`, `Ctrl/Cmd+W`, `Ctrl/Cmd+L`, `Ctrl/Cmd+Shift+T`, `Alt+H`, and `F12`.
- Fixed the macOS CEF/winit reentrancy crash: CEF is pumped from a main-thread `CFRunLoop` timer, never from a `window_event` handler, and reentrant `about_to_wait` ticks are ignored.
- Fixed macOS shutdown ordering: browsers and WAL are flushed in `about_to_wait`, the pump timer is disabled, and `cef::shutdown()` runs while winit's run-loop observers still exist.
- Fixed `do_close` to return `1` so CEF actually destroys browsers and fires `on_before_close`.
- Fixed Retina layout: `ContentRect` is logical/DIP-based, macOS respects the flipped winit `NSView`, and Windows/X11 convert to physical pixels.
- Added release packaging and code-signing documentation.
- Added `.gitignore` to keep `target/`, local CEF files, machine-specific Cargo config, logs, and OS/editor files out of the repository.
- Added `RELEASE_NOTES.md` as the canonical record of shipped changes.
- Published the source as a public GitHub repository.
- Verified with `cargo check --bins`, `cargo test --bin kasumi-browser`, `cargo fmt --check`, and repeated interactive macOS test runs with clean shutdown.

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

## ⌨️ Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Ctrl / Cmd + T` | Open new tab |
| `Ctrl / Cmd + W` | Close active tab |
| `Ctrl / Cmd + L` | Focus omnibox and select text |
| `Ctrl / Cmd + Shift + T` | Re-open last closed tab |
| `Alt + H` | Hibernate all background tabs immediately |
| `F12` | Open Chromium DevTools for the active tab |
| `Enter` (in Omnibox) | Navigate to URL, domain (auto-prefixes `https://`), or DuckDuckGo search |

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
    ├── config.rs                 # Thresholds (RAM pressure, timeouts, chrome dimensions)
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
    │   ├── schema.sql            # SQLite WAL schema (history, bookmarks, session_tabs)
    │   └── db.rs                 # Dedicated background Tokio/SQLite persistence worker
    └── ui/
        ├── mod.rs
        ├── platform_view.rs      # Cross-platform child window show/hide/resize
        └── chrome.rs             # egui + wgpu top bar (tabs, omnibox, shield badge, RAM meter)
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

- **Vertical tabs:** not implemented yet; the current tab strip is horizontal.
- **Theme awareness:** not implemented yet; the chrome currently uses a fixed dark style and does not follow the OS light/dark setting.
- **Windows and Linux testing:** native child-view code exists for `HWND` and `X11`, but full interactive verification is still needed on those platforms.
- **More unit tests:** expand coverage beyond omnibox resolution to tab state transitions, hibernation thresholds, adblock resource-type mapping, storage commands, and UI actions.
- **Chrome UX/navigation fixes:** see [BUGFIX_TASKS.md](BUGFIX_TASKS.md) for the tracked missing Back/Forward icons, tooltips, tab styling, omnibox sync, DevTools, view-source, and favicon work.

---

## #will be added later

- Settings UI
- Bookmark manager UI
- Download manager
- Automated cross-platform integration tests
- Release CI and signed artifact publishing

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
