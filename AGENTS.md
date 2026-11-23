# AGENTS.md — Master Engineering Spec & Strict Agent Rules for Kasumi Browser

> **AGENT DIRECTIVE:** This file is your single source of truth. Before writing or modifying any code, read every rule, architectural boundary, and phase requirement in this document. Do not deviate from the crate choices, threading model, or memory constraints defined below.

---

## 1. Project Identity & Core Constraints

You are building **Kasumi Browser**, a daily-driver desktop web browser written in **Rust** powered by the **Chromium Embedded Framework (CEF)** (`tauri-apps/cef-rs`).

Every single commit must honor three non-negotiable engineering goals:
1. **Ultra-Low Memory Footprint:**
   - **ZERO WebUI:** Never render browser chrome (tabs, URL bar, bookmarks, settings) using HTML/CSS/JS or a secondary CEF view. All browser UI MUST be drawn natively in Rust (`winit` + `wgpu` + `egui`) to keep shell RAM under `20 MB`.
   - **Aggressive Tab Hibernation:** Background tabs MUST be closed at the CEF `BrowserHost` level (`host.close_browser(1)`) after an idle timeout (or immediately under RAM pressure), freeing 100% of their Blink/V8 OS process memory while keeping metadata in Rust + SQLite.
   - **Process Consolidation:** Enforce `--process-per-site` and `--renderer-process-limit` via CEF CLI switches.
   - **Network-Level Ad/Tracker Blocking:** Block ad and tracker requests inside CEF's `ResourceRequestHandler` using `adblock` (`adblock-rust`) *before* Blink/V8 downloads or compiles scripts.
2. **Daily-Driver Reliability & Compatibility:**
   - Full support for modern web apps (YouTube, Google Docs, Figma, WebGL, WASM, WebRTC).
   - Crash-safe session persistence, history, and bookmarks stored in **SQLite (WAL mode)**.
   - Complete keyboard-first ergonomics (`Ctrl/Cmd+T`, `W`, `L`, `Shift+T`, `F`, `F12`, `Alt+H`, `Ctrl/Cmd+K`).
3. **Maximum Execution Speed:**
   - Use `mimalloc` as the `#[global_allocator]`.
   - Use **Native Child Window Embedding** (`raw-window-handle` + platform child view), NOT Off-Screen Rendering (OSR), to preserve zero-copy GPU compositing and hardware video decoding.

---

## 2. Strict AI Agent Guardrails (DO NOT VIOLATE)

### 2.1 Rust + CEF FFI & Lifecycle Rules
1. **`mimalloc` First:** `#[global_allocator] static GLOBAL: MiMalloc = MiMalloc;` MUST appear at the top of `src/main.rs`.
2. **macOS Dynamic Library Loader:** On `#[cfg(target_os = "macos")]`, `cef::library_loader::LibraryLoader` MUST load the CEF framework at the very start of `main()` before `cef::args::Args` or `cef::execute_process()` is called.
3. **Subprocess Fork Trap:** Chromium spawns renderer, GPU, and utility subprocesses by re-executing the main binary with `--type=...`. `cef::execute_process()` MUST run before initializing `winit`, `tokio`, `wgpu`, or `rusqlite`. If `exit_code >= 0`, call `std::process::exit(exit_code)` immediately.
4. **Never Block Chromium Threads:** CEF executes callbacks on `TID_UI`, `TID_IO`, and `TID_FILE`.
   - **FORBIDDEN:** Synchronous disk I/O, SQLite queries, or `std::sync::Mutex` locks that contend across threads inside any CEF handler (`Client`, `LifeSpanHandler`, `LoadHandler`, `RequestHandler`, `ResourceRequestHandler`).
   - **REQUIRED:** Send events from CEF callbacks to the main Rust thread using lock-free `crossbeam_channel::Sender<BrowserEvent>`. Send database writes to a background worker using `tokio::sync::mpsc::UnboundedSender<DbCommand>`.
5. **No `Arc<Mutex<Browser>>` Shared Across FFI:** Store `cef::Browser` handles exclusively on the main UI thread inside `TabManager`. Control them via messages/commands, never shared locks.
6. **Clean Shutdown Sequence:** Close all active `BrowserHost` instances and flush SQLite WAL before exiting the winit loop. On macOS, disable the main-thread CEF pump timer, call `cef::shutdown()` from `about_to_wait` while winit's run-loop observers are still alive, and ignore reentrant `about_to_wait` ticks raised by CEF's NSApp pump. `on_before_close` may be delivered during `cef::shutdown()`; never call CEF message-loop work or shutdown from a `window_event` handler.

### 2.2 Code Quality & Rust Best Practices
- **No `.unwrap()` or `.expect()` in runtime/FFI paths:** Handle missing `Option`s or FFI errors gracefully with `tracing::warn!` / `tracing::error!`.
- **No blocking the `winit` event loop:** Pump CEF via `cef::do_message_loop_work()` (with `external_message_pump: 1`) on `Event::AboutToWait` / timer ticks—never spawn a separate OS thread to pump CEF UI work.
- **Keep dependencies lean:** Do not add heavy async web frameworks, ORMs, or redundant crates beyond what is specified in `Cargo.toml`.

### 2.3 Versioning Discipline (MANDATORY)
- **A version bump is released when a pull request is merged into `main`.** Do not make direct version-only commits to `main`; land changes through a pull request instead.
- Every PR that changes shipped source, docs, packaging, or CI behavior MUST include the version bump in its branch so CI validates the exact release state. The version becomes official at merge time; do not bump again after the merge.
- Update `version` in `Cargo.toml`, the `semver::Version` passed to `BundleInfo` in `src/bin/bundle.rs`, the canonical `Cargo.toml` snippet in this file, `README.md`, `RELEASE_NOTES.md`, and let `cargo check` refresh `Cargo.lock`.
- Use SemVer: patch for fixes, minor for backward-compatible features, major for breaking changes.
- The PR title and merge commit MUST mention the new version (for example, `v0.1.1: fix macOS CEF layout`).
- Documentation changes that are part of the same PR are included in the same version bump.
- Every PR MUST pass `.github/workflows/pr.yml` before merge, including the unit tests.

---

## 3. Canonical `Cargo.toml`

Use these exact crate versions and release profile settings:

```toml
[package]
name = "kasumi-browser"
version = "0.1.9"
edition = "2021"

[dependencies]
# CEF safe bindings (v156.x; auto-fetches matching CEF binary if CEF_PATH is unset)
cef = "156.6"

# Low-fragmentation global allocator
mimalloc = { version = "0.1", default-features = false }

# Native windowing & GPU-accelerated Rust browser chrome
winit = "0.30"
raw-window-handle = "0.6"
egui = "0.30"
egui-wgpu = "0.30"
wgpu = "23"

# System RAM pressure monitoring for aggressive tab hibernation
sysinfo = "0.33"

# Brave's native Rust ad & tracker blocking engine (v0.13.x)
adblock = { version = "0.13", default-features = false, features = ["embedded-domain-resolver"] }

# Async storage worker & crash-safe SQLite persistence
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "fs"] }
rusqlite = { version = "0.32", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
url = "2"

# Lock-free FFI -> UI event bridge & structured logging
crossbeam-channel = "0.5"
tracing = "0.1"
tracing-subscriber = "0.3"

# Platform-specific child window positioning (HWND / NSView / X11)
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging"] }

[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.5"
objc2-app-kit = { version = "0.2", features = ["NSView", "NSWindow"] }
objc2-foundation = { version = "0.2", features = ["NSGeometry"] }

[target.'cfg(target_os = "linux")'.dependencies]
x11rb = "0.13"

[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

---

## 4. Repository Architecture

```text
kasumi-browser/
├── AGENTS.md                     # This specification & rulebook
├── Cargo.toml
├── assets/
│   └── filters/                  # EasyList / EasyPrivacy filter files (or fallback starter rules)
└── src/
    ├── main.rs                   # MiMalloc, CEF execute_process(), initialize(), winit loop, shutdown()
    ├── config.rs                 # Constants & user config (RAM thresholds, idle timeouts, search URL)
    ├── events.rs                 # BrowserEvent (CEF -> UI) & UiAction (UI -> TabManager) enums
    ├── app/
    │   ├── mod.rs                # KasumiBrowserApp (cef::App implementation)
    │   └── flags.rs              # Memory-saving & de-bloated Chromium CLI switches
    ├── browser/
    │   ├── mod.rs
    │   ├── client.rs             # TabClient (LifeSpanHandler, LoadHandler, DisplayHandler, RequestHandler)
    │   ├── adblocker.rs          # Arc<adblock::Engine> wrapper & ResourceRequestHandler
    │   └── hibernation.rs        # TabManager, TabState machine, sysinfo RAM watchdog
    ├── storage/
    │   ├── mod.rs
    │   ├── schema.sql            # SQLite WAL schema (history, bookmarks, session_tabs)
    │   └── db.rs                 # Background Tokio/SQLite worker & fast omnibox prefix search
    └── ui/
        ├── mod.rs
        ├── platform_view.rs      # Cross-platform child window show/hide/resize (Windows/macOS/Linux)
        └── chrome.rs             # egui + wgpu top bar (tabs, nav buttons, omnibox, RAM & adblock badges)
```

---

## 5. Detailed Subsystem Specifications

### 5.1 Chromium Memory & Bloat Switches (`src/app/flags.rs`)
In `App::on_before_command_line_processing`, verify `process_type` is empty/`None` (main browser process only), then inject:
- **Process Consolidation:**
  - `process-per-site` (shares 1 V8/Blink renderer process across tabs from the same domain)
  - `renderer-process-limit=6` (hard cap on concurrent renderer processes)
- **Telemetry & Service Removal:**
  - `disable-background-networking`
  - `disable-component-update`
  - `disable-domain-reliability`
  - `disable-sync`
  - `disable-speech-api`
  - `disable-breakpad`
  - `disable-features=Translate,OptimizationHints,MediaRouter,CalculateNativeWinOcclusion`
- **Cache Tuning:**
  - `blink-settings=decodedImageCacheLimitMB=32`

### 5.2 Tab Lifecycle & Hibernation State Machine (`src/browser/hibernation.rs`)
Every tab is tracked in `TabManager` with:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    Active,
    Background { since: std::time::Instant },
    Hibernated,
}

pub struct Tab {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub blocked_requests: u32,
    pub state: TabState,
    pub browser: Option<cef::Browser>,
}
```
- **Watchdog Rules (`tick()` called every 1s, RAM polled every 5s via `sysinfo::System`):**
  - Default idle timeout: `600` seconds (`10` minutes).
  - High memory pressure threshold: `ram_used / ram_total >= 0.80` -> shrink idle timeout to `30` seconds.
  - Critical memory pressure threshold: `ram_used / ram_total >= 0.90` -> hibernate ALL `Background` tabs immediately regardless of idle duration.
  - **Hibernation Execution:**
    1. Never hibernate `active_tab_id`.
    2. Queue session snapshot to SQLite (`DbCommand::SaveSession`).
    3. Take `tab.browser.take()`, call `host.close_browser(1)`, set `tab.state = TabState::Hibernated`, and reset `tab.blocked_requests = 0`.
  - **Waking Execution (`focus_tab(id)`):**
    1. Hide old active tab's native child window and set its state to `TabState::Background { since: Instant::now() }`.
    2. If target tab is `TabState::Hibernated`, transition to `TabState::Active` and call `browser_host_create_browser` with `tab.url` and parent window handle.
    3. If target tab already has a live `cef::Browser`, show and resize its native child window.

### 5.3 Network-Level Adblocker (`src/browser/adblocker.rs`)
- **Note on `adblock` v0.13 thread safety:** `adblock::Engine` is `Send + Sync` when the `single-thread` cargo feature is NOT enabled (which is configured in `Cargo.toml` above). Wrap it in `Arc<adblock::Engine>` so CEF's `TID_IO` thread can query it concurrently without locks.
- Load filter rules from `assets/filters/easylist.txt` and `assets/filters/easyprivacy.txt` if present on disk; otherwise initialize with a built-in starter filter set covering major ad/tracker networks (`doubleclick.net`, `googlesyndication.com`, `google-analytics.com`, `facebook.com/tr`, `adnxs.com`, `criteo.com`, `taboola.com`, `outbrain.com`, etc.) so blocking works out-of-the-box on first run.
- Inside `ResourceRequestHandler::on_before_resource_load`:
  1. Extract `url`, `first_party_url` (from `frame.url()`), and map `cef::ResourceType` to `adblock::request::Request`'s resource type string (`"script"`, `"image"`, `"stylesheet"`, `"xmlhttprequest"`, `"sub_frame"`, `"media"`, `"font"`, `"other"`).
  2. Call `engine.check_network_request(&req)`.
  3. If `result.matched == true`: emit `BrowserEvent::RequestBlocked { tab_id }` over the crossbeam channel and return `cef::ReturnValue::CANCEL`.

### 5.4 Native UI Chrome & Child Viewport (`src/ui/`)
- **Layout Geometry (logical points / DIPs):**
  - Top Tab Strip: `height = 38.0` px
  - Navigation & Omnibox Bar: `height = 38.0` px
  - Total Chrome Height: `CHROME_HEIGHT = 76.0` px
  - CEF Child Viewport Rect: `x = 0`, `y = 76`, `width = logical_window_width`, `height = logical_window_height - 76`.
  - macOS `NSView` frames and CEF `WindowInfo.bounds` use these logical values directly. Winit's content view is flipped (`isFlipped == true`), so the child view y-origin is `76`, not `parent_height - 76 - height`. Windows `HWND` and X11 APIs convert the logical rect to physical pixels with `scale_factor`.
- **UI Elements in `egui`:**
  - **Tab Strip:** Horizontal scrollable tabs showing `title` (truncated to 24 chars), loading spinner, `💤` badge if `TabState::Hibernated`, close button (`×`), and `+` new tab button.
  - **Toolbar:** Back (`←`), Forward (`→`), Reload/Stop (`↻`/`✕`), Omnibox text input (auto-selects all on `Ctrl/Cmd+L`), Shield badge (`🛡 {blocked_count}`), Hibernate All Background Tabs button (`💤`), and live System/App RAM readout (`RAM: 412 MB`).
  - **Omnibox Resolution:**
    - If input parses via `url::Url::parse` -> navigate directly.
    - Else if input has no spaces and contains `.` (e.g., `github.com/rust-lang`) -> prepend `https://` and navigate.
    - Else -> URL-encode query and navigate to `https://duckduckgo.com/?q={query}`.

### 5.5 SQLite Persistence (`src/storage/schema.sql` & `db.rs`)
Initialize SQLite on a dedicated background thread with:
```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 67108864;

CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    visit_count INTEGER NOT NULL DEFAULT 1,
    last_visited INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_last_visited ON history(last_visited DESC);

CREATE TABLE IF NOT EXISTS bookmarks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT 'Toolbar',
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS session_tabs (
    tab_index INTEGER PRIMARY KEY,
    url TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    is_active INTEGER NOT NULL DEFAULT 0,
    is_hibernated INTEGER NOT NULL DEFAULT 0
);
```

---

## 6. Phased Execution Plan (Follow Sequentially)

Complete one phase at a time. Run `cargo check` after every phase and resolve all compiler warnings/errors before moving to the next phase.

- **Phase 1: Core Skeleton, `mimalloc`, & CEF Subprocess Entry**
  - Create `Cargo.toml`, `src/config.rs`, `src/events.rs`, `src/app/flags.rs`, `src/app/mod.rs`, and `src/main.rs`.
  - Ensure `cef::execute_process()` runs first and `cef::initialize()` succeeds with `external_message_pump: 1`.
- **Phase 2: `winit` + `wgpu` + `egui` Shell & Native Child Window Embedding**
  - Implement `src/ui/platform_view.rs` (`show_view`, `hide_view`, `resize_view` for Windows `HWND`, macOS `NSView`, and Linux `X11`).
  - Implement `src/ui/chrome.rs` rendering the tab bar and omnibox in the top `76px`, and embed the active CEF browser below `y = 76px`.
- **Phase 3: `TabClient` Handlers & Tab Hibernation Engine**
  - Implement `LifeSpanHandler`, `LoadHandler`, and `DisplayHandler` in `src/browser/client.rs`.
  - Implement `TabManager` and the `sysinfo` RAM watchdog in `src/browser/hibernation.rs`.
  - Wire tab creation, switching, closing, hibernation (`Alt+H`), and waking on click.
- **Phase 4: `adblock-rust` Request Interception**
  - Implement `src/browser/adblocker.rs` and attach `RequestHandler` -> `ResourceRequestHandler` to `TabClient`.
  - Display live blocked request count per tab in the omnibox shield badge.
- **Phase 5: Async SQLite Persistence & Keyboard Shortcuts**
  - Implement `src/storage/schema.sql` and `src/storage/db.rs`.
  - Record visited URLs on navigation commit, restore open tabs on startup (spawning only the active tab live and loading all other restored tabs directly into `TabState::Hibernated` to guarantee instant, low-RAM startup!), and wire keyboard shortcuts (`Ctrl/Cmd+T`, `W`, `L`, `Shift+T`, `F12`, `Alt+H`).
- **Phase 6: Build Verification & Packaging**
  - Run `cargo build --release` and fix any platform-specific build/linker issues.
  - Document exact launch instructions in `README.md` (including `bundle-cef-app` for macOS).
  - Document release code-signing and notarization steps for macOS and Authenticode signing for Windows.
- **Phase 7: Documentation, Versioning & Publishing**
  - Keep `README.md` current with completed work, release signing steps, and future development items.
  - Bump the version for every batch of changes and publish the verified source to the public GitHub repository.