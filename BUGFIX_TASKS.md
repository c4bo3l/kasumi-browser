# Kasumi Browser — UI / Navigation Bugfix Handoff

**Version:** v0.1.9
**Date:** 2026-11-23  
**Status:** All reported bug items are implemented. v0.1.9 adds 36 unit tests covering the pure logic behind the fixes, plus the final browser-teardown correction confirmed by the user on macOS.

This document captures the reported bugs, the code inspection already done, likely root causes, concrete implementation leads, and acceptance criteria. Work through the items in the order listed: the low-risk UI/navigation fixes first, then the CEF callback work.

> **Current status:** There are no remaining code items in this handoff. Remaining work is process/manual verification: push the v0.1.9 branch to PR #1, confirm CI, merge, then optionally validate the packaged macOS app and Windows/Linux builds. F12, view-source, favicon, tooltip, and navigation behavior were implemented in the browser code; cross-platform and packaged-app checks are future verification, not known code defects.

---

## Ground rules from `AGENTS.md`

- Keep browser chrome 100% native (`winit` + `wgpu` + `egui`). Do **not** add WebUI or a second CEF view.
- Keep CEF callbacks lock-free: emit events with `crossbeam_channel::Sender<BrowserEvent>`; never do SQLite, blocking file I/O, or cross-thread `Mutex` work in CEF handlers.
- Keep `cef::Browser` handles on the main UI thread in `TabManager`.
- Do not pump CEF from `window_event` handlers. On macOS, keep the main-thread `CFRunLoop` timer approach and disable the pump before `cef::shutdown()`.
- Preserve the chrome geometry: tab strip `38 px`, toolbar `38 px`, total `CHROME_HEIGHT = 76 px`.
- Every new batch of source changes must bump the version in `Cargo.toml`, `src/bin/bundle.rs`, `AGENTS.md` canonical snippet, `README.md`, `RELEASE_NOTES.md`, and `Cargo.lock`, and the commit message must mention the version.

---

## 1. “What are the buttons before the refresh button for?”

### Reported symptom

The user does not know what the `←` and `→` buttons immediately before `↻` / `✕` do.

### Current code

`src/ui/chrome.rs` renders:

- `←` → `UiAction::GoBack { tab_id }`
- `→` → `UiAction::GoForward { tab_id }`
- `↻` / `✕` → reload / stop

They are driven by `TabInfo.can_go_back` / `can_go_forward`, which come from `BrowserEvent::LoadingStateChange` in `src/browser/client.rs` and are stored in `TabManager` in `src/browser/hibernation.rs`.

### Likely cause

The buttons are functionally correct but have no affordance: glyph-only labels, no tooltips, no accessibility text, and no shortcut hints.

### Fix

Treat this together with the tooltip task:

- Add `Response::on_hover_text(...)` to Back, Forward, Reload/Stop, New Tab, Close Tab, Hibernate All, and the shield badge.
- Use explicit labels in tooltips, e.g.:
  - `Back — go to the previous page (Cmd/Ctrl+Left)`
  - `Forward — go to the next page (Cmd/Ctrl+Right)`
  - `Reload — reload this page (Cmd/Ctrl+R)`
  - `Stop — cancel the current load (Esc)`
  - `Hibernate all background tabs (Alt+H)`
- Optionally add compact text next to the glyphs if the toolbar width allows it; tooltips are the minimum acceptable fix.

### Acceptance criteria

- Hovering each toolbar button shows a plain-language tooltip.
- A new user can identify Back, Forward, Reload/Stop, and Hibernate without reading source.
- Disabled Back/Forward buttons still show their tooltip when hovered.

---

## 1a. Back and Forward buttons show no icon

### Reported symptom

The two navigation buttons before Reload appear blank; the expected `←` / `→` icons are not visible.

### Current code

`src/ui/chrome.rs` renders the controls as Unicode glyphs inside frameless buttons:

- Back: `RichText::new("←").size(16.0)` inside `Button::new(...).stroke(Stroke::NONE)`
- Forward: `RichText::new("→").size(16.0)` inside the same frameless style

The buttons are wrapped in `ui.add_enabled(can_back, ...)` / `ui.add_enabled(can_fwd, ...)`, where the enabled flags come from `BrowserEvent::LoadingStateChange`.

### Likely causes

1. The bundled `egui` font atlas may not contain the Unicode arrow glyphs at the requested style/size, so the text paints nothing.
2. `Stroke::NONE` removes the button frame, leaving no fallback shape if the glyph is missing.
3. `add_enabled(false)` dims the glyph heavily; if `can_go_back` / `can_go_forward` are not being set correctly, the buttons look empty rather than disabled.
4. Glyph rendering can vary between platforms because no explicit font family is configured.

### Fix

Make the icons independent of font glyph availability:

- Preferred: draw the arrow shapes directly with `egui::Painter` inside a sized clickable area (a short horizontal line plus two diagonal head lines for each direction), using `ui.painter().line_segment(...)`.
- Alternative: generate a tiny `ColorImage` / `TextureHandle` arrow icon once and render it with `egui::Image`. This stays fully native and adds no font dependency.
- Keep hit areas at least `24×24` logical points even if the drawn icon is smaller.
- Restore a subtle button frame or background on hover so an icon failure never looks like empty space.
- Verify `LoadingStateChange` is arriving by logging or inspecting `TabInfo.can_go_back` / `can_go_forward`; disabled state should be visually distinct but still show the arrow.
- Do not rely on emoji arrows. Keep tooltips from task 1 so meaning is clear even if an icon is unavailable.

### Acceptance criteria

- Back and Forward icons are visibly rendered on macOS, Windows, and Linux.
- Enabled and disabled states are visually distinct; disabled arrows are dimmed, not invisible.
- Icons do not disappear at 1x or 2x DPI.
- No new font file or WebUI dependency is introduced.

---

## 2. “The tabs don't look modern”

### Reported symptom

The tab strip looks flat and dated.

### Current code

`src/ui/chrome.rs` builds each tab from a `Frame::none()` with a flat gray fill, a clickable `Label`, and a `×` label for close. Active vs. inactive only changes the fill color (`60,60,60` vs. `45,45,45`). There is no favicon slot, hover state, separator, active accent, or polished close affordance.

### Likely cause

The tab strip is a minimal first-pass implementation. The spec requires title truncation, loading spinner, hibernation badge, and close button, but not yet visual polish.

### Fix

Modernize inside `src/ui/chrome.rs` only, preserving the 38 px tab-strip height:

- Give active tabs a subtle top accent line or lighter surface; inactive tabs a darker recessed surface.
- Add hover highlighting on the tab label and close button.
- Use rounded corners and thin separators between tabs.
- Render the close control as a small circular/rounded button instead of bare ` ×` text.
- Keep `⟳` loading and `💤` hibernated badges, but make them visually secondary to the title.
- Add a favicon slot once task 7 is implemented.
- Ensure title truncation remains at 24 chars and long titles never push the `+` button off-screen.

Do not change `TAB_BAR_HEIGHT`, `CHROME_HEIGHT`, or `ContentRect` unless all four are updated together and Retina layout is re-verified.

### Acceptance criteria

- Active, background, hibernated, and loading tabs are visually distinguishable at a glance.
- Tab close targets are obvious and do not overlap the title.
- Layout remains correct at 1x and 2x scale, and CEF content still begins exactly at y=76 logical points.

---

## 3. “Add tooltips for all buttons”

### Current code

No `on_hover_text` calls exist in `src/ui/chrome.rs`.

### Files to change

- `src/ui/chrome.rs` — primary file.
- `src/events.rs` / `src/browser/hibernation.rs` — only if keyboard shortcuts are added for actions that currently have none.

### Fix

Add tooltips to every interactive control:

- Tab label: full title and URL if available (needs URL exposed in `TabInfo`).
- Tab close: `Close tab`.
- New tab: `New tab (Cmd/Ctrl+T)`.
- Back: `Back (Cmd/Ctrl+Left)`.
- Forward: `Forward (Cmd/Ctrl+Right)`.
- Reload/Stop: dynamic `Reload (Cmd/Ctrl+R)` or `Stop loading (Esc)`.
- Shield badge: `Blocked requests: N` if made interactive; otherwise use a hover tooltip on the label.
- Hibernate: `Hibernate all background tabs (Alt+H)`.
- RAM readout: optional tooltip explaining it is browser-process RAM.

If shortcuts such as Cmd/Ctrl+R, Cmd/Ctrl+Left, or Esc are advertised, implement them in `src/main.rs` and dispatch through `UiAction`; otherwise omit the hint.

### Acceptance criteria

- Every clickable chrome control has a tooltip.
- Tooltip text matches the real action and shortcut.
- No tooltip covers the omnibox while typing.

---

## 4. “When I click a link, the omnibox doesn't show the current URL”

### Reported symptom

After in-page navigation (for example clicking a link), the omnibox keeps the old text.

### Current code

- `TabDisplayHandler::on_address_change` in `src/browser/client.rs` correctly sends `BrowserEvent::AddressChange { tab_id, url }`.
- `TabManager::handle_browser_event` in `src/browser/hibernation.rs` correctly writes `tab.url = url`.
- `TabManager::update_chrome_state` in `src/browser/hibernation.rs` only syncs the omnibox when it is empty:

```rust
if state.omnibox_text.is_empty() {
    state.omnibox_text = tab.url.clone();
}
```

### Likely cause

The omnibox is not re-synced after the first navigation. Once the user has typed anything (or after the initial URL is set), `state.omnibox_text` is never replaced with the active tab's current URL. Tab switches also do not force a sync.

### Fix

Track omnibox edit/focus state explicitly:

- Add `omnibox_focused: bool` to `ChromeState` (or expose `ChromeRenderer`'s private field through the render result).
- In `TabManager::update_chrome_state`, sync whenever:
  - the active tab changed since the last frame, or
  - the active tab URL changed since the last frame, or
  - the omnibox is not focused.
- Do **not** overwrite text while the user is typing or while `Ctrl/Cmd+L` has focus.
- On `BrowserEvent::AddressChange` for the active tab, set `state.omnibox_text = url` immediately if the omnibox is not focused.
- On Enter, after pushing `UiAction::Navigate`, mark the omnibox as unfocused so it settles to the resolved URL.
- Consider stripping `view-source:` display noise later, but do not hide real URLs.

### Acceptance criteria

- Typing a URL and pressing Enter results in the omnibox showing the final committed URL.
- Clicking a link updates the omnibox to the new URL.
- Back/Forward updates the omnibox.
- Switching tabs shows that tab's URL.
- `Ctrl/Cmd+L` selects the full current URL and typing is not clobbered by background events.

---

## 5. “F12 doesn't show the console”

### Reported symptom

Pressing F12 does not open DevTools / console.

### Current code

- `src/main.rs` handles `NamedKey::F12` only when the winit `KeyboardInput` event is not consumed by egui and the main window receives it.
- It dispatches `UiAction::OpenDevTools { tab_id }`.
- `TabManager::handle_ui_action` calls `host.show_dev_tools(None, None, None, None)` in `src/browser/hibernation.rs`.
- `TabClient` in `src/browser/client.rs` has no `KeyboardHandler`.
- `LifeSpanHandler::on_before_popup` cancels every popup and emits `BrowserEvent::NewWindow`.

### Likely causes

1. When the web page has focus, the key event goes to the CEF child view, not winit, so the F12 branch never runs.
2. `show_dev_tools(None, ...)` may create an unparented/hidden DevTools window on macOS.
3. DevTools popup creation may be affected by `on_before_popup` cancellation if CEF routes it through that path.

### Fix

- Add a `WrapKeyboardHandler` implementation to `TabClient` (likely `src/browser/client.rs` or a new `src/browser/devtools.rs`):
  - Override `on_pre_key_event`.
  - Detect F12 via `event.windows_key_code` (verify the CEF constant; `123` is the usual VK_F12 value).
  - Emit a new `BrowserEvent::OpenDevTools { tab_id }` over the existing sender.
  - Return `true` only after confirming CEF does not also handle F12, to avoid double opening.
- Add the matching `BrowserEvent` variant in `src/events.rs` and handle it in `TabManager::pump_events` by calling `show_dev_tools`.
- Keep the winit F12 shortcut as a fallback for when chrome has focus.
- For DevTools window placement, pass an explicit `WindowInfo` with a reasonable size instead of `None`, or confirm CEF's default DevTools window is visible on macOS.
- Ensure `on_before_popup` does not cancel DevTools windows; if CEF reports DevTools as a popup with a distinctive disposition/URL, allow it through.
- Add a `UiAction::OpenDevTools` chrome button or menu entry later if keyboard focus remains ambiguous.

### Acceptance criteria

- F12 opens usable DevTools when focus is in the page.
- F12 opens usable DevTools when focus is in the omnibox/chrome.
- Pressing F12 repeatedly toggles/focuses DevTools instead of spawning many windows.
- No reentrant CEF/winit shutdown or message-loop regression.

---

## 6. “Right-click → View page source → doesn't show anything”

### Reported symptom

The native context menu appears, but choosing View Page Source produces no visible source view.

### Current code

- `TabClient` has no `ContextMenuHandler`.
- `LifeSpanHandler::on_before_popup` always cancels popups and converts the target URL into a new tab via `BrowserEvent::NewWindow`.
- There is no explicit handling for `MENU_ID_VIEW_SOURCE` / `view-source:` URLs.
- The adblocker runs before resource loads; verify it is not cancelling the generated source view unexpectedly.

### Likely causes

- CEF's default command may navigate to `view-source:<url>` in a way this shell does not display because popups are cancelled or the browser host is not created with the expected client.
- With no `ContextMenuHandler`, there is no place to intercept the command and create a normal Kasumi tab explicitly.

### Fix

Implement a `WrapContextMenuHandler` in `src/browser/client.rs`:

- Override `on_context_menu_command`.
- When `command_id == MenuId::VIEW_SOURCE` (binding constant `cef_menu_id_t::MENU_ID_VIEW_SOURCE`, exposed as `MenuId::VIEW_SOURCE`), emit a new event such as `BrowserEvent::ViewSource { tab_id, url }`.
- On the main thread, `TabManager` creates a new tab with `view-source:{url}` using the existing `UiAction::NewTab { url }` path, so it gets a normal browser/viewport.
- Return `1` to indicate the command was handled; return `0` for all other commands so CEF keeps its defaults.
- Alternatively, if CEF's default command already creates a popup, adjust `on_before_popup` to allow `view-source:` popups instead of emitting `NewWindow` and cancelling.
- Add a test/verification that `view-source:` passes through `resolve_omnibox` unchanged (`Url::parse` accepts it) if the user types it manually.

### Acceptance criteria

- Right-click → View Page Source opens a visible tab showing source.
- The original page stays open and active state transitions correctly.
- Other context menu commands (copy, paste, inspect element if present) still behave normally.
- Hibernating/restoring the source tab preserves its `view-source:` URL.

---

## 7. “The web favicon doesn't show up”

### Reported symptom

Tabs show only titles/state badges; no site favicon appears.

### Current code

- `TabInfo` in `src/ui/chrome.rs` has no favicon field.
- `Tab` in `src/browser/hibernation.rs` has no favicon field.
- `BrowserEvent` in `src/events.rs` has no favicon variants.
- `TabDisplayHandler` in `src/browser/client.rs` implements `on_address_change`, `on_title_change`, and `on_loading_progress_change`, but **not** `on_favicon_urlchange`.
- The tab strip has no image slot or fallback icon.

### CEF API leads already checked

- `ImplDisplayHandler::on_favicon_urlchange(&self, browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>)` exists in cef-rs 156.6.
- `CefStringList` implements `IntoIterator<Item = String>`, so URLs can be copied without lock-free channel violations.
- `BrowserHost::download_image(...)` and `wrap_download_image_callback!` / `DownloadImageCallback::on_download_image_finished` exist.
- `Image::as_bitmap(...)` / `Image::as_png(...)` exist, so pixels can be copied without adding a heavy image dependency.

### Likely cause

CEF is never asked to download the favicon, and even if it were, there is no data path or UI slot to render it.

### Fix

End-to-end implementation plan:

1. **Events / state**
   - Add to `src/events.rs`:
     - `BrowserEvent::FaviconUrlChanged { tab_id, urls: Vec<String> }`
     - `BrowserEvent::FaviconLoaded { tab_id, rgba: Vec<u8>, width: u32, height: u32 }`
   - Add `favicon: Option<...>` to `Tab` and `TabInfo`. Prefer an `Arc<[u8]>`/texture-key descriptor in state; keep the actual `egui::TextureHandle` cache inside `ChromeRenderer`.

2. **CEF callback**
   - In `TabDisplayHandler`, implement `on_favicon_urlchange`.
   - Copy `icon_urls` into `Vec<String>` and emit `BrowserEvent::FaviconUrlChanged`.
   - Do not do blocking I/O in this callback.

3. **Download**
   - On the main thread, handle `FaviconUrlChanged` in `TabManager::pump_events`.
   - Use `browser.host().download_image(url, ..., callback)` with a `wrap_download_image_callback!` callback.
   - In `on_download_image_finished`, copy the image bytes (prefer `Image::as_bitmap` into RGBA) and send `FaviconLoaded` over the crossbeam channel.
   - Deduplicate URLs per tab to avoid repeated downloads on every navigation.

4. **Render**
   - In `ChromeRenderer`, maintain `HashMap<u64, egui::TextureHandle>` keyed by tab id (or URL).
   - On `FaviconLoaded`, install/replace the texture on the next frame.
   - Draw a 16×16 favicon before the title in the tab strip; use a neutral globe/document glyph fallback when absent.
   - Clear/replace the favicon when the tab navigates to a different site or is hibernated/woken if desired; preserving the last image across hibernation is acceptable.

5. **Persistence (optional follow-up)**
   - If favicons should survive restarts, add a small blob column/table in `src/storage/schema.sql` and route through `DbCommand`. This is optional for the first fix.

### Acceptance criteria

- Visiting a site with a standard `<link rel="icon">` shows its favicon in the tab.
- Favicons do not block or scale incorrectly at 1x/2x DPI.
- Missing/blocked favicons show the fallback without logging errors or spamming downloads.
- Hibernating/waking a tab does not leak textures or duplicate downloads.
- No SQLite/file I/O occurs inside CEF handler callbacks.

---

## Suggested implementation order

1. Omnibox URL sync (highest user impact, isolated state bug).
2. Back/Forward icon rendering (small, isolated `ChromeRenderer` fix).
3. Tooltips + toolbar button explanation.
4. Tab strip visual polish (and favicon slot placeholder).
5. Favicon pipeline (touches events, clients, state, renderer).
6. F12 DevTools handler.
7. View-source context menu handling.

After each item, run the verification commands below. Do not combine unrelated fixes into one batch.

---

## Verification checklist

```bash
cargo fmt --all -- --check
cargo check --bins
cargo test --bin kasumi-browser   # 36 tests
cargo build --release
```

Manual macOS checks:

```bash
cargo run --bin kasumi-browser
```

- Navigate with omnibox, then click an in-page link; omnibox updates.
- Use Back/Forward; icons are visible, omnibox and enabled state update.
- Hover every chrome control; tooltips appear.
- Switch tabs; each tab shows correct title, URL, favicon, and state badge.
- Press F12 with focus in the page and with focus in the omnibox.
- Right-click a page and choose View Page Source.
- Hibernate all background tabs, wake one, then quit; exit code is `0` and no CEF reentrancy crash occurs.

Confirmed on macOS in v0.1.9: closing a YouTube tab stops audio immediately after the CEF native view is detached and `try_close_browser()` completes.

Packaged macOS check:

```bash
cargo build --release
cargo run --release --bin bundle
open target/bundle/KasumiBrowser.app
```

---

## Versioning for the next batch

The next agent should start by bumping to **v0.2.0** (this handoff batch is v0.1.9):

- `Cargo.toml`
- `src/bin/bundle.rs`
- `AGENTS.md` canonical `Cargo.toml` snippet
- `README.md` current version
- `RELEASE_NOTES.md`
- `Cargo.lock` via `cargo check`

Suggested PR title / merge commit:

```text
v0.2.0: fix chrome UX and navigation feedback
```

Publish with a pull request:

```bash
git push -u origin <branch>
gh pr create --title "v0.2.0: fix chrome UX and navigation feedback" --fill
gh pr checks --watch
gh pr merge --squash
```
