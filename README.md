# Kasumi Browser

A low-memory, daily-driver desktop browser written in Rust and powered by CEF (`tauri-apps/cef-rs`). All browser chrome is native (`winit` + `wgpu` + `egui`); pages render in native child views with no WebUI, OSR, or second renderer.

Repository: [github.com/c4bo3l/kasumi-browser](https://github.com/c4bo3l/kasumi-browser) · [Release notes](RELEASE_NOTES.md) · [PR checks](https://github.com/c4bo3l/kasumi-browser/actions/workflows/pr.yml)

Current version: **0.10.0**. Version bumps land in release PRs and become official at merge, as required by [AGENTS.md](AGENTS.md).

---

## Design

- **Zero WebUI:** tabs, rail, omnibox, settings, downloads, and overlays are drawn natively.
- **Native page embedding:** `NSView` / `HWND` / `X11` child views preserve zero-copy compositing and hardware video decoding.
- **Low RAM:** CEF processes are consolidated, background tabs are fully closed after idle or under memory pressure, and restored tabs start hibernated.
- **Private by design:** Incognito uses an ephemeral CEF request context and never writes history, sessions, bookmarks, downloads, or site permissions to SQLite.
- **Network-level blocking:** `adblock-rust` cancels ad and tracker requests before Blink compiles or downloads them.
- **Crash-safe storage:** SQLite WAL stores history, bookmarks, downloads, workspaces, preferences, and session metadata.

## Features

- Collapsible vertical workspace rail (208 px expanded / 44 px icon rail), pinned tabs, tab context menu, and per-workspace tab state.
- Floating `Cmd/Ctrl+K` command palette for tab search, URL/search resolution, and native actions.
- Native Workspaces, Incognito mode, Settings, downloads bubble, bookmark manager, tab menu, tooltips, and system notifications.
- Native WebRTC media-access prompts for camera, microphone, and desktop audio/video capture, with remembered decisions for normal spaces. `getDisplayMedia` opens Kasumi's own display/window picker and passes the selected source to Chromium.
- 24 DIP bottom HUDs for Command/DevTools and blocked-request/RAM status.
- SQLite-backed history, bookmarks, downloads, pinned tabs, workspaces, preferences, and low-RAM session restore.
- Chromium DevTools in a separate window; `F12`, rail button, tab menu, and command palette all open it.

## Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Ctrl/Cmd + T` / `W` | New tab / close tab |
| `Ctrl/Cmd + K` / `L` | Command palette / palette with active URL |
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

---

## Release signing

Sign after building and bundling. Never commit certificates or credentials.

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

Windows and Linux native child-view, tooltip, download, bookmark, and media-permission flows still need hands-on verification.

## Roadmap

Current focus: WebRTC calls. Next: website notifications (Notification API permission prompts, native notification display, and click-through), native print support, browser import, cross-platform verification, auto-update, PiP, local AI, and extension support. See [RELEASE_NOTES.md](RELEASE_NOTES.md) for completed work.

## Adblock filter lists

Built-in fallback rules work out of the box. For full coverage:

```bash
mkdir -p assets/filters
curl -Lo assets/filters/easylist.txt https://easylist.to/easylist/easylist.txt
curl -Lo assets/filters/easyprivacy.txt https://easylist.to/easylist/easyprivacy.txt
```

Filters are compiled on startup.
