# Kasumi Browser

A low-memory, daily-driver desktop browser written in Rust and powered by CEF (`tauri-apps/cef-rs`). All browser chrome is native (`winit` + `wgpu` + `egui`); pages render in native child views with no WebUI, OSR, or second renderer.

Repository: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser) · [Release notes](RELEASE_NOTES.md) · [PR checks](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml)

Current version: **0.13.0**. Version bumps land in release PRs and become official at merge, as required by [AGENTS.md](AGENTS.md).

---

## Design

- **Zero WebUI:** tabs, rail, omnibox, settings, downloads, and overlays are drawn natively.
- **Native page embedding:** `NSView` / `HWND` / `X11` child views preserve zero-copy compositing and hardware video decoding.
- **Low RAM:** CEF processes are consolidated, background tabs are fully closed after idle or under memory pressure, and restored tabs start hibernated.
- **Private by design:** Incognito uses an ephemeral CEF request context and never writes history, sessions, bookmarks, downloads, or site permissions to SQLite.
- **Network-level blocking:** `adblock-rust` cancels ad and tracker requests before Blink compiles or downloads them.
- **Crash-safe storage:** SQLite WAL stores history, bookmarks, downloads, workspaces, preferences, and session metadata.

## Features

### Browsing and native chrome

- **Zero WebUI:** tabs, rail, omnibox, settings, command palette, menus, tooltips, downloads bubble, find bar, and notifications are all drawn natively with `winit`, `wgpu`, and `egui`. Pages render in native child views with no OSR or second renderer.
- **Native tabs:** create, switch, close, duplicate, reopen, and undo close; middle-click closes a tab, middle-clicking a link opens a live background tab, and Ctrl/Cmd+click opens a foreground tab. Tabs show favicons, animated loading spinners, active indicators, and hover close buttons.
- **Pinned tabs:** pin/unpin from the tab context menu, command palette, or rail badge. Pinned tabs stay at the top of each workspace and survive session restore and undo-close.
- **Tab context menu:** new tab, duplicate, copy URL, reload, close, close other tabs, close tabs below, reopen closed tab, move to workspace, and DevTools, with keyboard navigation and click-outside dismissal.
- **Navigation:** back/forward, reload/stop, an omnibox that follows the committed URL, URL or search resolution through DuckDuckGo/Google/Bing, `view-source:` tab reuse, and copy URL with a native confirmation.
- **Collapsible vertical workspace rail:** tab list, pinned section, Command button, New tab, Hibernate All, shield badge, RAM readout, address field, and workspace switcher in both expanded and icon-only modes.
- **Native overlays:** the command palette, Settings, tab menus, tooltips, find bar, and notification cards render above the CEF child view in transparent native windows. Toast notifications are click-through and do not steal page input.
- **Theme-aware UI:** native chrome follows the OS light/dark appearance live, with explicit Light/Dark overrides. Accent colour is applied only to browser chrome, never to page content.
- **Bottom page HUDs:** Command/DevTools controls and live blocked-request/RAM status stay visible while browsing.
- **Native DevTools window:** `F12`, the rail, the tab menu, and the command palette open or bring forward Chromium DevTools.
- **Native printing:** `Cmd/Ctrl+P` and the command palette open CEF's platform print dialog for the active tab.

### Find in page

- `Cmd/Ctrl+F` opens a native find bar with match count, previous/next navigation, and close.
- Native CEF find remains authoritative for substring match counts and ordinals; a bounded renderer bridge supplies rich snippet results.
- Snippet results show surrounding text with bold/coloured matches, heading breadcrumbs, hover previews, and click-to-jump navigation.
- Regex, whole-word, and multi-term modes, with inline invalid-regex feedback and per-term colour highlighting.
- `This tab` and `All tabs` scopes, grouped cross-tab results with favicon/title/count metadata, and wake-and-search replay for hibernated tabs.
- Copy matching lines, sentences, or hrefs, and optionally filter results by their source text.

### Workspaces, commands, and settings

- **Workspaces:** each workspace owns its pinned and regular tabs, selected tab, and default flag. Switch from the rail, choose a default startup space, and create, rename, or delete spaces. Deleting a workspace moves its tabs and archives its bookmark folder.
- **Command palette:** `Cmd/Ctrl+K` searches tabs and bookmarks and resolves addresses or searches; `Cmd/Ctrl+L` opens it seeded with the active URL. It offers explicit open/search in this tab or a new tab, native actions, workspace labels, keyboard selection, immediate focus, and click-outside dismissal.
- **Native Settings:** General, Workspaces, and Bookmarks pages with persisted preferences for theme, rail state, search provider, ad-block state, hibernation timeout, startup space, and download folder.

### Privacy, performance, and web platform

- **Incognito:** a reserved private workspace with a lazily created ephemeral CEF request context. Private tabs never write history, session, bookmarks, downloads, or site permissions to SQLite.
- **Aggressive tab hibernation:** idle tabs are fully closed at the CEF `BrowserHost` level, with a configurable timeout, shortened timeout under high RAM pressure, and immediate hibernation of background tabs under critical pressure. Restored sessions reopen only the active tab live.
- **Network-level blocking:** `adblock-rust` cancels ad and tracker subresource requests before Blink downloads or compiles them. Built-in starter rules work out of the box; optional EasyList/EasyPrivacy files can be added locally.
- **Process consolidation:** Chromium switches enforce process-per-site and a renderer-process cap, with background networking and telemetry services disabled.
- **WebRTC permissions:** native camera, microphone, and desktop audio/video prompts with per-origin decisions remembered for normal spaces. `getDisplayMedia` opens Kasumi's own display/window picker and passes the exact selected source to Chromium, with a video-only retry when macOS denies system audio.
- **Web Notification API:** a renderer shim with remembered per-site permission prompts and clickable native notification cards. Activating a card focuses the source tab and dispatches `click`; closing dispatches `close`. Incognito decisions stay in memory.

### Storage, downloads, and bookmarks

- **Crash-safe SQLite WAL storage:** history, bookmarks, downloads, pinned tabs, workspaces, preferences, site permissions, and session snapshots, flushed cleanly on shutdown.
- **Native download manager:** WAL-backed history plus active progress, speed, ETA, path, connection count, pause/resume/cancel, retry, remove-info, clear-finished, open-file, and show-in-folder controls. The rail button shows an active count badge.
- **Segmented downloads:** HTTP(S) transfers use up to eight balanced `Range` requests through CEF into a `.kasumi.part` file, with resumable byte ranges and generation guards so late callbacks cannot corrupt a file. Non-range transfers fall back to a single native request.
- **Download notifications:** transparent native toasts announce completed or failed transfers without blocking page input.
- **Bookmark manager:** search by title, URL, or folder; edit titles and URLs; move between workspace-named folders; delete bookmarks; and toggle the current page with the rail/palette star. Bookmarks remain globally visible across workspaces.

### Packaging and developer workflow

- **Native app icons** for macOS, Windows, and Linux, with the bundled macOS app resolving the icon in Finder and the Dock.
- **macOS bundle support** with camera, microphone, screen-recording, and system-audio usage descriptions, ad-hoc signing for local TCC prompts, and Developer ID/notarisation guidance for releases.
- **Windows cross-build** from macOS via `make build-windows`, plus Authenticode signing instructions.
- **Local launchers:** `./scripts/dev.sh` / `make dev` and `scripts\dev.bat`, with clean-cache, release, watch, and custom log-level options.
- **Cross-platform PR CI** checks formatting, binaries, and unit tests on macOS 15 Intel, Windows, and Linux.

## Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Ctrl/Cmd + T` / `W` | New tab / close tab |
| `Ctrl/Cmd + K` / `L` | Command palette / palette with active URL |
| `Ctrl/Cmd + F` | Find in page |
| `Ctrl/Cmd + P` | Print active page |
| `Ctrl/Cmd + ,` | Settings |
| `Ctrl/Cmd + J` | Downloads |
| `Ctrl/Cmd + Shift + T` / `Z` | Reopen closed tab / undo close |
| `Ctrl/Cmd + Shift + N` | Incognito space or new private tab |
| `Ctrl/Cmd + Shift + C` | Copy active page URL |
| `Ctrl/Cmd + Shift + B` | Bookmark manager |
| `Alt + H` | Hibernate all background tabs |
| `F12` | DevTools |
| `Ctrl/Cmd + Q` | Graceful quit |
| `↑` / `↓`, `Enter`, `Esc` | Navigate and activate overlays |

---

## Architecture

```text
src/
├── main.rs                 # mimalloc, CEF subprocess trap, winit loop, shutdown
├── config.rs               # Preferences, thresholds, geometry constants
├── events.rs               # BrowserEvent / UiAction cross-thread bridges
├── app/                    # cef::App implementation and Chromium flags
├── browser/
│   ├── client.rs           # CEF handlers (lifecycle, load, input, permissions, downloads)
│   ├── adblocker.rs        # adblock-rust request interception
│   ├── find.rs             # rich find shim, snippets, and result parser
│   ├── notifications.rs    # Web Notification API shim and native bridge
│   └── hibernation.rs      # TabManager, workspace state, RAM watchdog
├── storage/                # SQLite WAL schema and async worker
└── ui/
    ├── chrome.rs           # egui rail, palette, overlays, HUDs, prompts
    └── platform_view.rs    # Native child-view show/hide/resize
```

See [AGENTS.md](AGENTS.md) for the full engineering specification.

---

## Build and run

Requires stable Rust. `cef-rs` fetches matching CEF binaries automatically unless `CEF_PATH` is set. To use a local CEF distribution, copy `.cargo/config.toml.example` to `.cargo/config.toml` and set `CEF_PATH`.

```bash
./scripts/dev.sh              # build + run (make dev is equivalent)
./scripts/dev.sh --clean      # clear CEF cache first
./scripts/dev.sh --release    # optimized run
./scripts/dev.sh --watch      # restart on source changes
cargo run --bin kasumi-browser
cargo build --release
```

On Windows use `scripts\dev.bat`.

### Windows cross-build from macOS

Requires `cargo-xwin` and LLVM tools (`clang-cl`, `lld-link`, `llvm-lib`) on `PATH`. Override the Makefile's `LLVM_BIN` if they live elsewhere.

```bash
make build-windows
```

This writes `target/x86_64-pc-windows-msvc/release/kasumi-browser.exe`.

App artwork lives in `assets/icons/source/`; platform-ready outputs are in `assets/icons/mac/`, `assets/icons/windows/`, and `assets/icons/linux/`.

### macOS app bundle

```bash
cargo install bundle-cef-app
cargo build --release
bundle-cef-app \
  --bundle-path target/release/KasumiBrowser.app \
  --binary-path target/release/kasumi-browser \
  --framework-path "$CEF_PATH/Chromium Embedded Framework.framework"
open target/release/KasumiBrowser.app
```

`cargo run --release --bin bundle` is the in-repo alternative; it writes `target/bundle/kasumi-browser.app`, includes the app icon from `assets/icons/mac/kasumi-browser.icns`, sets both `CFBundleIconFile` and `CFBundleIconName`, and adds camera, microphone, screen-recording, and system-audio usage descriptions to `Info.plist`.

For WebRTC tests on macOS, launch the bundled app rather than the bare `target/release/kasumi-browser` binary from a terminal. macOS attributes TCC camera, microphone, screen, and system-audio permissions to the responsible app bundle; running the bare binary can make macOS prompt the terminal or IDE instead. Kasumi's Allow/Deny prompt records the origin decision, releases the CEF media request, and syncs Chromium's own camera/microphone content settings. `getDisplayMedia` opens Kasumi's native source picker because CEF Alloy cannot present Chrome's screen picker inside an embedded native child view on macOS. The chosen display or window is resolved through Chromium's legacy mandatory desktop-capture constraints, which carry the exact `DesktopMediaID` into CEF's media request. If macOS denies system-audio capture, Kasumi retries the share video-only so the selected display or window still works.

**Bundled app vs `make dev`:** `make dev` runs under Terminal, which may already have camera, microphone, and screen-recording approval. The Finder-launched bundle is a separate macOS app and needs its own approvals. If the bundle cannot use the camera/microphone or share the screen, open **System Settings → Privacy & Security → Screen & System Audio Recording**, **Camera**, and **Microphone**, enable **KasumiBrowser**, then quit and reopen `target/bundle/kasumi-browser.app`. While the screen-recording approval dialog is pending, camera/microphone capture can fail; finish the approval and restart the app before testing Meet again. If the screen-recording request was previously denied, reset the bundle entry and relaunch to get a fresh prompt:

```bash
tccutil reset ScreenCapture com.kasumi.browser
open target/bundle/kasumi-browser.app
```

---

## Release signing

Sign after building and bundling. Never commit certificates or credentials.

`make bundle` ad-hoc signs the generated app by default so macOS reliably shows Camera, Microphone, and Screen Recording TCC prompts during local testing. Ad-hoc signatures change on every rebuild, so re-grant permissions after replacing the bundle. For a stable release identity, set `CODESIGN_IDENTITY`:

```bash
CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" make bundle
```

That path uses hardened runtime, a secure timestamp, and `assets/macos/KasumiBrowser.entitlements` for camera/microphone access.

**macOS:** install a Developer ID Application certificate and store notarization credentials with `xcrun notarytool store-credentials`. Sign nested CEF helpers/frameworks first, then the outer app, then notarize and staple:

```bash
IDENTITY="Developer ID Application: Your Name (TEAMID)"
APP="target/bundle/kasumi-browser.app"
find "$APP" -type f -perm +111 -print0 | while IFS= read -r -d '' file; do
  case "$file" in
    *.framework/*|*Helper*|*.app/Contents/MacOS/*)
      codesign --force --timestamp --options runtime --sign "$IDENTITY" "$file" ;;
  esac
done
codesign --force --deep --timestamp --options runtime --sign "$IDENTITY" "$APP"
ditto -c -k --keepParent "$APP" KasumiBrowser.zip
xcrun notarytool submit KasumiBrowser.zip --keychain-profile "kasumi-notarization" --wait
xcrun stapler staple "$APP"
spctl --assess --type execute --verbose=4 "$APP"
```

**Windows:** sign every shipped `.exe` with the same Authenticode certificate:

```powershell
signtool sign /f $PFX /p $env:PFX_PASSWORD /fd SHA256 `
  /tr http://timestamp.digicert.com /td SHA256 target\release\kasumi-browser.exe
signtool verify /pa /v target\release\kasumi-browser.exe
```

**Linux:** publish checksums and GPG signatures rather than a single platform signing format.

---

## Verification

```bash
cargo fmt --all -- --check
cargo check --bins
cargo test --bins
cargo build --release
```

CI runs the same checks on macOS 15 Intel, Windows, and Linux.

## Known issues

Windows and Linux native child-view, tooltip, download, bookmark, media-permission, website-notification, find-in-page, and printing flows still need hands-on verification. Linux printing also requires a CEF `PrintHandler` before the platform dialog and spooler path are supported.

## Roadmap

Up next: Linux print handler, browser import, cross-platform verification, auto-update, PiP, local AI, and extension support. See [RELEASE_NOTES.md](RELEASE_NOTES.md) for completed work and [Future development](RELEASE_NOTES.md#future-development) for deferred items.

## Adblock filter lists

Built-in fallback rules work out of the box. For full coverage:

```bash
mkdir -p assets/filters
curl -Lo assets/filters/easylist.txt https://easylist.to/easylist/easylist.txt
curl -Lo assets/filters/easyprivacy.txt https://easylist.to/easylist/easyprivacy.txt
```

Filters are compiled on startup.
