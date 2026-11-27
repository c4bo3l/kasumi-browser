use crate::browser::find::{
    parse_result as parse_find_result, FIND_RESULT_CONSOLE_PREFIX,
    RENDERER_SHIM as FIND_RENDERER_SHIM,
};
use crate::browser::notifications::{
    parse_close_message, parse_permission_message, parse_show_message,
    NOTIFICATION_CLOSE_CONSOLE_PREFIX, NOTIFICATION_PERMISSION_CONSOLE_PREFIX,
    NOTIFICATION_SHOW_CONSOLE_PREFIX,
};
use crate::events::{
    BrowserEvent, BrowserShortcut, DownloadEngine, DownloadInfo, DownloadState,
    MediaAccessPermissions, MediaPermissionCallback, PrintOptions, WebNotification,
    DISPLAY_MEDIA_CONSOLE_PREFIX,
};
use cef::*;
use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared snapshot of download preferences exposed to the CEF download
/// handler. `ask_where_to_save` is an atomic flag (like `adblock_enabled`), and
/// the directory is guarded by a short-lived read lock: the callback only
/// clones a small string, and writes happen exclusively on the UI thread when
/// the user applies Settings, so there is no cross-thread contention.
#[derive(Clone, Default)]
pub struct SharedDownloadPreferences {
    pub ask_where_to_save: Arc<AtomicBool>,
    pub download_directory: Arc<RwLock<String>>,
}

impl SharedDownloadPreferences {
    pub fn from_preferences(prefs: &crate::config::BrowserPreferences) -> Self {
        Self {
            ask_where_to_save: Arc::new(AtomicBool::new(prefs.ask_where_to_save)),
            download_directory: Arc::new(RwLock::new(prefs.download_directory.clone())),
        }
    }

    pub fn apply(&self, prefs: &crate::config::BrowserPreferences) {
        self.ask_where_to_save
            .store(prefs.ask_where_to_save, Ordering::Release);
        match self.download_directory.write() {
            Ok(mut directory) => *directory = prefs.download_directory.clone(),
            Err(poisoned) => *poisoned.into_inner() = prefs.download_directory.clone(),
        }
    }

    pub fn ask_where_to_save(&self) -> bool {
        self.ask_where_to_save.load(Ordering::Acquire)
    }

    pub fn download_directory(&self) -> String {
        match self.download_directory.read() {
            Ok(directory) => directory.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

wrap_life_span_handler! {
    pub struct TabLifeSpanHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                let _ = self.event_sender.send(BrowserEvent::BrowserCreated {
                    tab_id: self.tab_id,
                    browser: browser.clone(),
                });
            }
        }

        fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
            // Returning true tells CEF that the host window needs a custom close
            // notification. We must then complete the browser close ourselves;
            // otherwise the browser object lingers and page media keeps playing.
            let completed = browser
                .as_ref()
                .and_then(|browser| browser.host())
                .map(|host| {
                    // Returning true means the embedder owns native view
                    // teardown. Remove the child view before asking CEF to
                    // finish closing, otherwise the browser stays alive and
                    // page media keeps playing.
                    let view_handle = host.window_handle();
                    if !crate::ui::platform_view::cef_handle_is_null(view_handle) {
                        unsafe {
                            crate::ui::platform_view::detach_view(
                                crate::ui::platform_view::cef_handle_as_ptr(view_handle),
                            );
                        }
                    }
                    host.try_close_browser()
                })
                .unwrap_or(0);
            tracing::info!(
                "CEF do_close for tab {} (detached view, try_close_browser={})",
                self.tab_id,
                completed
            );
            1
        }
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            // Only report that the CEF browser is gone. Do NOT remove the tab here:
            // hibernation also closes browsers, and the tab metadata must survive.
            tracing::info!("CEF on_before_close for tab {}", self.tab_id);
            let _ = self.event_sender.send(BrowserEvent::BrowserClosed { tab_id: self.tab_id });
        }
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefStringUtf16>,
            _target_frame_name: Option<&CefStringUtf16>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            if let Some(url) = target_url {
                let url_str = url.to_string();
                if url_str.is_empty() {
                    return 1;
                }

                // DevTools and view-source windows are part of the browser
                // itself; routing them through the normal new-tab popup path
                // makes them invisible or useless in this shell.
                if is_internal_view_url(&url_str) {
                    return 0;
                }

                let background = new_tab_disposition(target_disposition).unwrap_or(false);
                let _ = self.event_sender.send(BrowserEvent::OpenUrlFromTab {
                    tab_id: self.tab_id,
                    url: url_str,
                    background,
                });
            }
            1 // Cancel ordinary popup windows; open them as Kasumi tabs instead.
        }
    }
}

/// Monotonic ids for media-access prompts. CEF handlers are per browser, so a
/// process-wide counter is the simplest way to keep concurrent prompts distinct
/// without sharing any mutable state across Chromium threads.
static NEXT_MEDIA_PERMISSION_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_media_permission_request_id() -> u64 {
    NEXT_MEDIA_PERMISSION_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Monotonic ids for Web Notification permission prompts. Kept separate from
/// media prompts so the UI can route a resolution to the right queue.
static NEXT_NOTIFICATION_PERMISSION_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_notification_permission_request_id() -> u64 {
    NEXT_NOTIFICATION_PERMISSION_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Translate Chrome-runtime permission request types into Kasumi's media bits.
///
/// `on_show_permission_prompt` uses `cef_permission_request_types_t`, not the
/// `cef_media_access_permission_types_t` passed to
/// `on_request_media_access_permission`. Treating the two as the same bitmask
/// made camera/microphone prompts unreadable and caused grants that Chromium
/// later discarded. Google Meet can also request camera pan/tilt/zoom together
/// with the camera stream; that is still a camera permission and must not send
/// the whole prompt to CEF's default IGNORE handling. Non-media prompts are
/// left to CEF's default handling.
fn media_permissions_for_prompt_types(requested: u32) -> Option<u32> {
    let camera = PermissionRequestTypes::CAMERA_STREAM.get_raw() as u32;
    let camera_ptz = PermissionRequestTypes::CAMERA_PAN_TILT_ZOOM.get_raw() as u32;
    let microphone = PermissionRequestTypes::MIC_STREAM.get_raw() as u32;
    let supported = camera | camera_ptz | microphone;

    if requested == 0 || requested & !supported != 0 {
        return None;
    }

    let mut permissions = 0;
    if requested & (camera | camera_ptz) != 0 {
        permissions |= MediaAccessPermissions::VIDEO;
    }
    if requested & microphone != 0 {
        permissions |= MediaAccessPermissions::AUDIO;
    }
    (permissions != 0).then_some(permissions)
}

/// Hand a CEF permission callback to the main UI thread. Returning 1 tells CEF
/// that Kasumi owns the prompt, so no Chrome-runtime or Alloy fallback UI runs.
fn send_media_permission_request(
    tab_id: u64,
    event_sender: &Sender<BrowserEvent>,
    origin: String,
    permissions: u32,
    callback: MediaPermissionCallback,
    prompt_id: Option<u64>,
) -> i32 {
    let request_id = next_media_permission_request_id();
    tracing::info!(
        "Media access request {} from {} for tab {} (permissions={}, prompt_id={:?})",
        request_id,
        origin,
        tab_id,
        permissions,
        prompt_id
    );
    let event = BrowserEvent::MediaAccessPermissionRequested {
        request_id,
        tab_id,
        origin,
        permissions,
        callback: callback.clone(),
        prompt_id,
    };
    if event_sender.send(event).is_err() {
        // Taking ownership means the callback must still be resolved if the UI
        // thread disappeared before it could show the prompt.
        tracing::warn!(
            "UI receiver dropped before media permission {} could be shown",
            request_id
        );
        callback.resolve(false, permissions);
    }
    1
}

wrap_permission_handler! {
    pub struct TabPermissionHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> i32 {
            let Some(callback) = callback else {
                return 0;
            };
            // Kasumi's embedded browsers use Alloy style, where this is the
            // only media permission callback. Chrome-style windows (for
            // example DevTools) use on_show_permission_prompt instead; both
            // paths feed the same native prompt.
            let requested = MediaAccessPermissions::from_bits(requested_permissions);
            if requested.bits() == 0 {
                return 0;
            }
            let origin = requesting_origin
                .map(|origin| origin.to_string())
                .unwrap_or_default();
            send_media_permission_request(
                self.tab_id,
                &self.event_sender,
                origin,
                requested_permissions,
                MediaPermissionCallback::MediaAccess(callback.clone()),
                None,
            )
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> i32 {
            let Some(callback) = callback else {
                return 0;
            };
            let Some(permissions) = media_permissions_for_prompt_types(requested_permissions) else {
                tracing::debug!(
                    "Leaving non-media permission prompt {} (types={}) to CEF",
                    prompt_id,
                    requested_permissions
                );
                return 0;
            };
            let origin = requesting_origin
                .map(|origin| origin.to_string())
                .unwrap_or_default();
            send_media_permission_request(
                self.tab_id,
                &self.event_sender,
                origin,
                permissions,
                MediaPermissionCallback::PermissionPrompt(callback.clone()),
                Some(prompt_id),
            )
        }

        fn on_dismiss_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            _result: PermissionRequestResult,
        ) {
            let _ = self.event_sender.send(BrowserEvent::PermissionPromptDismissed {
                tab_id: self.tab_id,
                prompt_id,
            });
        }
    }
}

wrap_load_handler! {
    pub struct TabLoadHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            let _ = self.event_sender.send(BrowserEvent::LoadingStateChange {
                tab_id: self.tab_id,
                is_loading: is_loading != 0,
                can_go_back: can_go_back != 0,
                can_go_forward: can_go_forward != 0,
            });
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            let Some(frame) = frame else {
                return;
            };
            if frame.is_main() == 0 {
                return;
            }
            let url = CefStringUtf16::from(&frame.url()).to_string();
            if !(url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("file://"))
            {
                return;
            }
            // Install the display-source shim on the browser's UI thread after
            // the document is ready. Doing this from a renderer context hook
            // made CEF's initial navigation fail and could re-enter V8 when
            // DevTools attached.
            frame.execute_java_script(
                Some(&CefString::from(
                    crate::browser::display_capture::RENDERER_SHIM,
                )),
                None,
                0,
            );
            // CEF has no native Web Notification presenter. Replace the API
            // with a shim that forwards permission requests and payloads to the
            // native chrome through the same internal console channel.
            frame.execute_java_script(
                Some(&CefString::from(
                    crate::browser::notifications::RENDERER_SHIM,
                )),
                None,
                0,
            );
            // Install the non-destructive find highlighter. It is idempotent and
            // also re-installed by each find request, so a search typed during
            // the initial navigation still works.
            frame.execute_java_script(Some(&CefString::from(FIND_RENDERER_SHIM)), None, 0);
        }
    }
}

// Streams CEF find results to the main UI thread. The `selection_rect` is
// deliberately ignored: it is a browser-viewport rectangle that belongs to
// the page and must never be retained outside the callback.
wrap_find_handler! {
    pub struct TabFindHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            identifier: i32,
            count: i32,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: i32,
            final_update: i32,
        ) {
            let _ = self.event_sender.send(BrowserEvent::FindResult {
                tab_id: self.tab_id,
                identifier,
                count,
                active_match_ordinal,
                final_update: final_update != 0,
            });
        }
    }
}

wrap_display_handler! {
    pub struct TabDisplayHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if let (Some(frame), Some(url)) = (frame, url) {
                if frame.is_main() != 0 {
                    let _ = self.event_sender.send(BrowserEvent::AddressChange {
                        tab_id: self.tab_id,
                        url: url.to_string(),
                    });
                }
            }
        }

        fn on_title_change(
            &self,
            _browser: Option<&mut Browser>,
            title: Option<&CefString>,
        ) {
            if let Some(title) = title {
                let _ = self.event_sender.send(BrowserEvent::TitleChange {
                    tab_id: self.tab_id,
                    title: title.to_string(),
                });
            }
        }

        fn on_favicon_urlchange(
            &self,
            _browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            if let Some(icon_urls) = icon_urls {
                let urls: Vec<String> = icon_urls.clone().into_iter().collect();
                tracing::info!("Favicon URLs for tab {}: {:?}", self.tab_id, urls);
                if !urls.is_empty() {
                    let _ = self.event_sender.send(BrowserEvent::FaviconUrlChanged {
                        tab_id: self.tab_id,
                        urls,
                    });
                }
            }
        }

        fn on_loading_progress_change(
            &self,
            _browser: Option<&mut Browser>,
            progress: f64,
        ) {
            let _ = self.event_sender.send(BrowserEvent::LoadingProgressChange {
                tab_id: self.tab_id,
                progress,
            });
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            let Some(message) = message else {
                return 0;
            };
            let message = message.to_string();

            if message.starts_with(DISPLAY_MEDIA_CONSOLE_PREFIX) {
                let Some(token) = message.strip_prefix(DISPLAY_MEDIA_CONSOLE_PREFIX) else {
                    return 0;
                };
                let token = token.trim().to_string();
                if token.is_empty() {
                    return 0;
                }
                let _ = self.event_sender.send(BrowserEvent::DisplayMediaPickerRequested {
                    tab_id: self.tab_id,
                    token,
                });
                // This is an internal control message, not useful page logging.
                return 1;
            }

            if message.starts_with(FIND_RESULT_CONSOLE_PREFIX) {
                let Some(payload) = parse_find_result(&message) else {
                    return 0;
                };
                let _ = self.event_sender.send(BrowserEvent::FindResultRich {
                    tab_id: self.tab_id,
                    request_id: payload.request_id,
                    count: payload.count,
                    active_ordinal: payload.active_ordinal,
                    snippets: payload.snippets,
                    error: payload.error,
                    cleared: payload.cleared,
                });
                return 1;
            }

            if message.starts_with(NOTIFICATION_PERMISSION_CONSOLE_PREFIX) {
                let Some(payload) = parse_permission_message(&message) else {
                    return 0;
                };
                let _ =
                    self.event_sender
                        .send(BrowserEvent::NotificationPermissionRequested {
                            request_id: next_notification_permission_request_id(),
                            tab_id: self.tab_id,
                            origin: payload.origin,
                            token: payload.token,
                        });
                return 1;
            }

            if message.starts_with(NOTIFICATION_SHOW_CONSOLE_PREFIX) {
                let Some(payload) = parse_show_message(&message) else {
                    return 0;
                };
                let _ = self.event_sender.send(BrowserEvent::WebNotificationShown(
                    WebNotification {
                        id: payload.id,
                        tab_id: self.tab_id,
                        origin: payload.origin,
                        title: payload.title,
                        body: payload.body,
                        tag: payload.tag,
                        require_interaction: payload.require_interaction,
                    },
                ));
                return 1;
            }

            if message.starts_with(NOTIFICATION_CLOSE_CONSOLE_PREFIX) {
                let Some(payload) = parse_close_message(&message) else {
                    return 0;
                };
                let _ = self.event_sender.send(BrowserEvent::WebNotificationClosed {
                    tab_id: self.tab_id,
                    id: payload.id,
                });
                return 1;
            }

            0
        }
    }
}

/// CEF event flags. Values come from `cef_event_flags_t`.
const SHIFT_MODIFIER_FLAG: u32 = 2;
const PRIMARY_MODIFIER_FLAGS: u32 = 4 | 128; // EVENTFLAG_CONTROL_DOWN | EVENTFLAG_COMMAND_DOWN
const ALT_MODIFIER_FLAG: u32 = 8;
const REPEAT_EVENT_FLAG: u32 = 8_192;

/// Virtual key codes, using CEF's platform-independent Windows key codes.
const VK_B: i32 = 0x42;
const VK_C: i32 = 0x43;
const VK_F: i32 = 0x46;
const VK_J: i32 = 0x4A;
const VK_K: i32 = 0x4B;
const VK_L: i32 = 0x4C;
const VK_N: i32 = 0x4E;
const VK_P: i32 = 0x50;
const VK_T: i32 = 0x54;
const VK_W: i32 = 0x57;
const VK_Y: i32 = 0x59;
const VK_Z: i32 = 0x5A;
const VK_OEM_COMMA: i32 = 0xBC;
const VK_OEM_PERIOD: i32 = 0xBE;
const VK_F12: i32 = 0x7B;

/// URLs rendered by Chromium's own tool windows. These must stay in CEF-owned
/// popups instead of becoming ordinary Kasumi tabs.
pub(crate) fn is_internal_view_url(url: &str) -> bool {
    url.starts_with("devtools://")
        || url.starts_with("chrome-devtools://")
        || url.starts_with("view-source:")
}

/// Translate a Chromium link-activation disposition into a Kasumi tab request.
///
/// `Some(true)` means a new background tab, `Some(false)` a new foreground
/// tab, and `None` leaves the navigation in the source tab to CEF.
pub(crate) fn new_tab_disposition(disposition: WindowOpenDisposition) -> Option<bool> {
    match disposition {
        WindowOpenDisposition::NEW_BACKGROUND_TAB => Some(true),
        WindowOpenDisposition::NEW_FOREGROUND_TAB
        | WindowOpenDisposition::NEW_POPUP
        | WindowOpenDisposition::NEW_WINDOW => Some(false),
        _ => None,
    }
}

/// Classify a key event forwarded from the CEF web view.
///
/// Native child views do not deliver key events to the winit parent, so the
/// browser has to mirror the shell shortcuts itself. Keeping the mapping in a
/// pure helper makes the cross-platform behavior unit-testable.
pub(crate) fn browser_shortcut(
    key_down: bool,
    key_code: i32,
    modifiers: u32,
) -> Option<BrowserShortcut> {
    let is_repeat = modifiers & REPEAT_EVENT_FLAG != 0;
    if !key_down || is_repeat {
        return None;
    }

    let primary = modifiers & PRIMARY_MODIFIER_FLAGS != 0;
    let shift = modifiers & SHIFT_MODIFIER_FLAG != 0;
    let alt = modifiers & ALT_MODIFIER_FLAG != 0;

    if key_code == VK_F12 {
        return Some(BrowserShortcut::OpenDevTools);
    }

    if !primary || alt {
        return None;
    }

    match key_code {
        VK_T if shift => Some(BrowserShortcut::ReopenClosedTab),
        VK_N if shift => Some(BrowserShortcut::NewPrivateWindow),
        VK_T => Some(BrowserShortcut::NewTab),
        VK_W if !shift => Some(BrowserShortcut::CloseTab),
        VK_F if !shift => Some(BrowserShortcut::FindInPage),
        VK_P if !shift => Some(BrowserShortcut::Print),
        VK_L | VK_K if !shift => Some(BrowserShortcut::OpenCommandPalette),
        VK_B if shift => Some(BrowserShortcut::OpenBookmarkManager),
        VK_C if shift => Some(BrowserShortcut::CopyUrl),
        VK_J if !shift => Some(BrowserShortcut::OpenDownloads),
        VK_Y if !shift => Some(BrowserShortcut::OpenHistory),
        VK_OEM_COMMA | VK_OEM_PERIOD if !shift => Some(BrowserShortcut::OpenSettings),
        VK_Z if !shift => Some(BrowserShortcut::ReopenClosedTab),
        _ => None,
    }
}

/// Fallback shortcut classification for platforms where CEF's
/// `windows_key_code` is not populated reliably (notably `Cmd+,` inside a
/// native macOS child view). CEF still supplies UTF-16 character data, so
/// matching that as well makes the shell shortcuts work while a page has
/// keyboard focus.
fn browser_shortcut_from_char(
    key_down: bool,
    key_char: u16,
    modifiers: u32,
) -> Option<BrowserShortcut> {
    let is_repeat = modifiers & REPEAT_EVENT_FLAG != 0;
    if !key_down || is_repeat {
        return None;
    }

    let primary = modifiers & PRIMARY_MODIFIER_FLAGS != 0;
    let shift = modifiers & SHIFT_MODIFIER_FLAG != 0;
    let alt = modifiers & ALT_MODIFIER_FLAG != 0;
    if !primary || alt {
        return None;
    }

    match key_char {
        c if c == ',' as u16 || c == '<' as u16 || c == '.' as u16 || c == '>' as u16 => {
            Some(BrowserShortcut::OpenSettings)
        }
        c if c == 'T' as u16 || c == 't' as u16 => {
            if shift {
                Some(BrowserShortcut::ReopenClosedTab)
            } else {
                Some(BrowserShortcut::NewTab)
            }
        }
        c if c == 'N' as u16 || c == 'n' as u16 => {
            if shift {
                Some(BrowserShortcut::NewPrivateWindow)
            } else {
                None
            }
        }
        c if c == 'W' as u16 || c == 'w' as u16 => Some(BrowserShortcut::CloseTab),
        c if c == 'F' as u16 || c == 'f' as u16 => Some(BrowserShortcut::FindInPage),
        c if (c == 'P' as u16 || c == 'p' as u16) && !shift => Some(BrowserShortcut::Print),
        c if c == 'L' as u16 || c == 'l' as u16 || c == 'K' as u16 || c == 'k' as u16 => {
            Some(BrowserShortcut::OpenCommandPalette)
        }
        c if c == 'B' as u16 || c == 'b' as u16 => {
            if shift {
                Some(BrowserShortcut::OpenBookmarkManager)
            } else {
                None
            }
        }
        c if c == 'C' as u16 || c == 'c' as u16 => {
            if shift {
                Some(BrowserShortcut::CopyUrl)
            } else {
                None
            }
        }
        c if c == 'J' as u16 || c == 'j' as u16 => Some(BrowserShortcut::OpenDownloads),
        c if (c == 'Y' as u16 || c == 'y' as u16) && !shift => Some(BrowserShortcut::OpenHistory),
        c if c == 'Z' as u16 || c == 'z' as u16 => Some(BrowserShortcut::ReopenClosedTab),
        _ => None,
    }
}

fn handle_pre_key_event(
    tab_id: u64,
    event_sender: &Sender<BrowserEvent>,
    event: Option<&KeyEvent>,
) -> i32 {
    let Some(event) = event else {
        return 0;
    };
    let is_key_down =
        event.type_ == KeyEventType::KEYDOWN || event.type_ == KeyEventType::RAWKEYDOWN;
    let shortcut = browser_shortcut(is_key_down, event.windows_key_code, event.modifiers)
        .or_else(|| {
            browser_shortcut_from_char(is_key_down, event.unmodified_character, event.modifiers)
        })
        .or_else(|| browser_shortcut_from_char(is_key_down, event.character, event.modifiers));
    if let Some(shortcut) = shortcut {
        let _ = event_sender.send(BrowserEvent::BrowserShortcut { tab_id, shortcut });
        return 1;
    }
    0
}

/// Convert CEF's Windows-epoch microsecond timestamp into Unix seconds. A
/// zero/invalid value falls back to the current time so the native list always
/// has a usable value.
fn basetime_unix_secs(value: cef::Basetime) -> u64 {
    const WINDOWS_TO_UNIX_MICROS: i64 = 11_644_473_600_000_000;
    if value.val <= WINDOWS_TO_UNIX_MICROS {
        return now_unix_secs();
    }
    ((value.val - WINDOWS_TO_UNIX_MICROS) / 1_000_000) as u64
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn userfree_string(value: cef::CefStringUserfree) -> String {
    CefString::from(&value).to_string()
}

fn download_state(item: &DownloadItem) -> DownloadState {
    if item.is_canceled() != 0 {
        DownloadState::Canceled
    } else if item.is_interrupted() != 0 {
        DownloadState::Interrupted
    } else if item.is_complete() != 0 {
        DownloadState::Completed
    } else if item.is_paused() != 0 {
        DownloadState::Paused
    } else {
        DownloadState::InProgress
    }
}

/// Build the absolute path CEF should hand to the native save dialog. CEF
/// expects a full file path (not just a directory); without the filename it
/// can lose the suggested extension and fall back to an extensionless name.
fn download_target_path(directory: &Path, suggested_name: &str) -> Option<PathBuf> {
    if suggested_name.trim().is_empty() {
        return None;
    }
    let file_name = crate::browser::parallel_download::safe_file_name(suggested_name);
    if file_name.is_empty() || file_name == "download" {
        return None;
    }
    Some(directory.join(file_name))
}

/// Snapshot only owned values. CEF forbids retaining the `DownloadItem`
/// outside its callback, so native chrome never sees the CEF object itself.
fn snapshot_download(
    tab_id: u64,
    item: &DownloadItem,
    suggested_name: Option<&CefString>,
) -> DownloadInfo {
    let name = suggested_name
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| userfree_string(item.suggested_file_name()));
    let full_path = userfree_string(item.full_path());
    let started_at = basetime_unix_secs(item.start_time());
    let updated_at = if download_state(item).is_finished() {
        basetime_unix_secs(item.end_time())
    } else {
        now_unix_secs()
    };

    DownloadInfo {
        id: item.id(),
        tab_id,
        url: userfree_string(item.url()),
        suggested_name: name,
        full_path: if full_path.is_empty() {
            String::new()
        } else {
            full_path
        },
        received_bytes: item.received_bytes(),
        total_bytes: item.total_bytes(),
        percent_complete: item.percent_complete(),
        current_speed: item.current_speed(),
        state: download_state(item),
        engine: DownloadEngine::Cef,
        segments: 1,
        temp_path: String::new(),
        started_at,
        updated_at,
    }
}

wrap_download_handler! {
    pub struct TabDownloadHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
        download_preferences: SharedDownloadPreferences,
    }

    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> i32 {
            // Downloads are accepted into Chromium's configured directory and
            // presented in Kasumi's native downloads bubble. Returning 0 here
            // would cancel every download before `on_before_download` ran.
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> i32 {
            let (Some(item), Some(callback)) = (download_item, callback) else {
                return 0;
            };
            let info = snapshot_download(self.tab_id, item, suggested_name);
            let suggested_name = suggested_name
                .map(|name| name.to_string())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| info.suggested_name.clone());
            let _ = self.event_sender.send(BrowserEvent::DownloadStarted(info));

            // "Ask where to save" delegates the destination to CEF's native
            // save dialog. Otherwise seed CEF with the configured Downloads
            // folder so non-segmentable downloads (blob:, data:, file:) also
            // land there instead of CEF's temp directory. CEF's `download_path`
            // must include the filename so the suggested extension survives.
            let ask = self.download_preferences.ask_where_to_save();
            let configured = self.download_preferences.download_directory();
            let directory = if configured.trim().is_empty() {
                crate::config::default_download_directory()
            } else {
                PathBuf::from(&configured)
            };
            let show_dialog = if ask { 1 } else { 0 };
            if let Some(path) = download_target_path(&directory, &suggested_name) {
                let path_text = path.to_string_lossy().to_string();
                let cef_path = CefString::from(path_text.as_str());
                callback.cont(Some(&cef_path), show_dialog);
            } else {
                // No usable server-provided name: let CEF derive one from the
                // URL and Content-Disposition instead of forcing "download".
                callback.cont(None, show_dialog);
            }
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else {
                return;
            };
            let info = snapshot_download(self.tab_id, item, None);
            let item_callback = callback.map(|callback| (*callback).clone());
            let _ = self.event_sender.send(BrowserEvent::DownloadUpdated {
                info,
                callback: item_callback,
            });
        }
    }
}

wrap_keyboard_handler! {
    pub struct TabKeyboardHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl KeyboardHandler {
        #[cfg(target_os = "macos")]
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> i32 {
            handle_pre_key_event(self.tab_id, &self.event_sender, event)
        }

        #[cfg(windows)]
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: Option<&mut cef::sys::MSG>,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> i32 {
            handle_pre_key_event(self.tab_id, &self.event_sender, event)
        }

        #[cfg(target_os = "linux")]
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: Option<&mut cef::sys::XEvent>,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> i32 {
            handle_pre_key_event(self.tab_id, &self.event_sender, event)
        }
    }
}

wrap_context_menu_handler! {
    pub struct TabContextMenuHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl ContextMenuHandler {
        fn on_context_menu_command(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            command_id: ::std::os::raw::c_int,
            _event_flags: EventFlags,
        ) -> i32 {
            let view_source_id = cef::sys::cef_menu_id_t::MENU_ID_VIEW_SOURCE as i32;
            if command_id != view_source_id {
                return 0;
            }

            let Some(params) = params else {
                return 0;
            };
            let page_url_userfree = params.page_url();
            let page_url = CefStringUtf16::from(&page_url_userfree).to_string();
            if page_url.is_empty() {
                return 0;
            }

            let _ = self.event_sender.send(BrowserEvent::ViewSource {
                tab_id: self.tab_id,
                url: page_url,
            });
            1
        }
    }
}

wrap_client! {
    pub struct TabClient {
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
        display_handler: DisplayHandler,
        find_handler: FindHandler,
        request_handler: RequestHandler,
        download_handler: DownloadHandler,
        keyboard_handler: KeyboardHandler,
        context_menu_handler: ContextMenuHandler,
        permission_handler: PermissionHandler,
        // Linux overrides CEF's missing platform dialog; macOS/Windows keep
        // the default handler by leaving this `None`.
        print_handler: Option<PrintHandler>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(self.find_handler.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request_handler.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download_handler.clone())
        }
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(self.keyboard_handler.clone())
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu_handler.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permission_handler.clone())
        }
        fn print_handler(&self) -> Option<PrintHandler> {
            self.print_handler.clone()
        }
    }
}

pub fn create_tab_client(
    tab_id: u64,
    event_sender: Sender<BrowserEvent>,
    adblock_engine: std::sync::Arc<adblock::Engine>,
    adblock_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    download_preferences: SharedDownloadPreferences,
    print_options_receiver: Receiver<PrintOptions>,
) -> Client {
    let life_span_handler = TabLifeSpanHandler::new(tab_id, event_sender.clone());
    let load_handler = TabLoadHandler::new(tab_id, event_sender.clone());
    let display_handler = TabDisplayHandler::new(tab_id, event_sender.clone());
    let find_handler = TabFindHandler::new(tab_id, event_sender.clone());
    let keyboard_handler = TabKeyboardHandler::new(tab_id, event_sender.clone());
    let context_menu_handler = TabContextMenuHandler::new(tab_id, event_sender.clone());
    let permission_handler = TabPermissionHandler::new(tab_id, event_sender.clone());
    let print_handler =
        super::print::create_print_handler(tab_id, event_sender.clone(), print_options_receiver);
    let download_handler =
        TabDownloadHandler::new(tab_id, event_sender.clone(), download_preferences);
    let request_handler = super::adblocker::create_tab_request_handler(
        tab_id,
        event_sender,
        adblock_engine,
        adblock_enabled,
    );

    TabClient::new(
        life_span_handler.into(),
        load_handler.into(),
        display_handler.into(),
        find_handler.into(),
        request_handler.into(),
        download_handler.into(),
        keyboard_handler.into(),
        context_menu_handler.into(),
        permission_handler.into(),
        print_handler,
    )
}

#[cfg(test)]
mod tests {
    use super::download_target_path;
    use super::media_permissions_for_prompt_types;
    use super::new_tab_disposition;
    use super::{browser_shortcut, browser_shortcut_from_char, is_internal_view_url};
    use crate::events::{BrowserShortcut, MediaAccessPermissions};
    use cef::{PermissionRequestTypes, WindowOpenDisposition};
    use std::path::Path;

    #[test]
    fn chrome_runtime_prompt_types_map_to_media_permissions() {
        let camera = PermissionRequestTypes::CAMERA_STREAM.get_raw() as u32;
        let camera_ptz = PermissionRequestTypes::CAMERA_PAN_TILT_ZOOM.get_raw() as u32;
        let microphone = PermissionRequestTypes::MIC_STREAM.get_raw() as u32;
        assert_eq!(
            media_permissions_for_prompt_types(camera),
            Some(MediaAccessPermissions::VIDEO)
        );
        assert_eq!(
            media_permissions_for_prompt_types(camera_ptz),
            Some(MediaAccessPermissions::VIDEO)
        );
        assert_eq!(
            media_permissions_for_prompt_types(microphone),
            Some(MediaAccessPermissions::AUDIO)
        );
        assert_eq!(
            media_permissions_for_prompt_types(camera | camera_ptz | microphone),
            Some(MediaAccessPermissions::AUDIO | MediaAccessPermissions::VIDEO)
        );
    }

    #[test]
    fn non_media_and_mixed_prompts_are_left_to_cef() {
        let camera = PermissionRequestTypes::CAMERA_STREAM.get_raw() as u32;
        assert_eq!(media_permissions_for_prompt_types(0), None);
        assert_eq!(
            media_permissions_for_prompt_types(
                PermissionRequestTypes::GEOLOCATION.get_raw() as u32,
            ),
            None
        );
        assert_eq!(
            media_permissions_for_prompt_types(
                camera | PermissionRequestTypes::GEOLOCATION.get_raw() as u32
            ),
            None
        );
    }

    #[test]
    fn download_target_keeps_suggested_filename_and_extension() {
        let directory = Path::new("/tmp/kasumi-downloads");
        assert_eq!(
            download_target_path(directory, "report (1).pdf"),
            Some(directory.join("report (1).pdf"))
        );
        assert_eq!(
            download_target_path(directory, "../../etc/passwd"),
            Some(directory.join("passwd"))
        );
        assert_eq!(download_target_path(directory, "   "), None);
    }

    #[test]
    fn middle_click_disposition_opens_a_background_tab() {
        assert_eq!(
            new_tab_disposition(WindowOpenDisposition::NEW_BACKGROUND_TAB),
            Some(true)
        );
    }

    #[test]
    fn explicit_new_window_dispositions_open_foreground_tabs() {
        for disposition in [
            WindowOpenDisposition::NEW_FOREGROUND_TAB,
            WindowOpenDisposition::NEW_POPUP,
            WindowOpenDisposition::NEW_WINDOW,
        ] {
            assert_eq!(new_tab_disposition(disposition), Some(false));
        }
    }

    #[test]
    fn ordinary_navigations_stay_in_the_source_tab() {
        for disposition in [
            WindowOpenDisposition::UNKNOWN,
            WindowOpenDisposition::CURRENT_TAB,
            WindowOpenDisposition::SINGLETON_TAB,
            WindowOpenDisposition::SWITCH_TO_TAB,
        ] {
            assert_eq!(new_tab_disposition(disposition), None);
        }
    }

    #[test]
    fn chromium_tool_views_are_not_routed_to_kasumi_tabs() {
        for url in [
            "devtools://devtools/bundled/inspector.html",
            "chrome-devtools://devtools/bundled/inspector.html",
            "view-source:https://example.com/",
        ] {
            assert!(is_internal_view_url(url), "{url} should stay in CEF");
        }
        assert!(!is_internal_view_url("https://example.com/"));
    }

    #[test]
    fn web_view_forwards_all_shell_shortcuts() {
        assert_eq!(
            browser_shortcut(true, 0x54, 128),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x54, 4),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x54, 128 | 2),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x4E, 128 | 2),
            Some(BrowserShortcut::NewPrivateWindow)
        );
        assert_eq!(browser_shortcut(true, 0x4E, 128), None);
        assert_eq!(
            browser_shortcut(true, 0x5A, 128),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x57, 128),
            Some(BrowserShortcut::CloseTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x4B, 128),
            Some(BrowserShortcut::OpenCommandPalette)
        );
        assert_eq!(
            browser_shortcut(true, 0x4C, 4),
            Some(BrowserShortcut::OpenCommandPalette)
        );
        assert_eq!(
            browser_shortcut(true, 0x46, 128),
            Some(BrowserShortcut::FindInPage)
        );
        assert_eq!(
            browser_shortcut(true, 0x46, 4),
            Some(BrowserShortcut::FindInPage)
        );
        assert_eq!(browser_shortcut(true, 0x46, 128 | 2), None);
        assert_eq!(
            browser_shortcut(true, 0x50, 128),
            Some(BrowserShortcut::Print)
        );
        assert_eq!(
            browser_shortcut(true, 0x50, 4),
            Some(BrowserShortcut::Print)
        );
        assert_eq!(browser_shortcut(true, 0x50, 128 | 2), None);
        assert_eq!(
            browser_shortcut(true, 0x4A, 128),
            Some(BrowserShortcut::OpenDownloads)
        );
        assert_eq!(
            browser_shortcut(true, 0x59, 128),
            Some(BrowserShortcut::OpenHistory)
        );
        assert_eq!(
            browser_shortcut(true, 0x59, 4),
            Some(BrowserShortcut::OpenHistory)
        );
        // Shift+Y stays with the page so text shortcuts keep working.
        assert_eq!(browser_shortcut(true, 0x59, 128 | 2), None);
        assert_eq!(
            browser_shortcut(true, 0x7B, 0),
            Some(BrowserShortcut::OpenDevTools)
        );
        assert_eq!(
            browser_shortcut(true, 0x43, 128 | 2),
            Some(BrowserShortcut::CopyUrl)
        );
        assert_eq!(
            browser_shortcut(true, 0xBC, 128),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            browser_shortcut(true, 0xBE, 128),
            Some(BrowserShortcut::OpenSettings)
        );
    }

    #[test]
    fn web_view_falls_back_to_utf16_characters_for_settings_shortcut() {
        // CEF's Windows key code is not always populated for `Cmd+,` in a
        // native macOS child view, so the UTF-16 character must also work.
        assert_eq!(
            browser_shortcut_from_char(true, ',' as u16, 128),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            browser_shortcut_from_char(true, '<' as u16, 128),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            browser_shortcut_from_char(true, '.' as u16, 4),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            browser_shortcut_from_char(true, 'T' as u16, 128 | 2),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            browser_shortcut_from_char(true, 'N' as u16, 128 | 2),
            Some(BrowserShortcut::NewPrivateWindow)
        );
        assert_eq!(browser_shortcut_from_char(true, 'n' as u16, 128), None);
        assert_eq!(
            browser_shortcut_from_char(true, 't' as u16, 128),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            browser_shortcut_from_char(true, 'j' as u16, 128),
            Some(BrowserShortcut::OpenDownloads)
        );
        assert_eq!(
            browser_shortcut_from_char(true, 'y' as u16, 128),
            Some(BrowserShortcut::OpenHistory)
        );
        assert_eq!(browser_shortcut_from_char(true, 'Y' as u16, 128 | 2), None);
        assert_eq!(
            browser_shortcut_from_char(true, 'p' as u16, 128),
            Some(BrowserShortcut::Print)
        );
        assert_eq!(browser_shortcut_from_char(true, 'p' as u16, 128 | 2), None);
        assert_eq!(
            browser_shortcut_from_char(true, 'C' as u16, 128 | 2),
            Some(BrowserShortcut::CopyUrl)
        );
        // Plain Cmd/Ctrl+C stays with the page so text selection copies.
        assert_eq!(browser_shortcut_from_char(true, 'c' as u16, 128), None);
        assert_eq!(browser_shortcut_from_char(false, ',' as u16, 128), None);
        assert_eq!(
            browser_shortcut_from_char(true, ',' as u16, 128 | 8_192),
            None
        );
    }

    #[test]
    fn web_view_ignores_repeats_and_unrelated_modifiers() {
        assert_eq!(browser_shortcut(false, 0x54, 128), None);
        assert_eq!(browser_shortcut(true, 0x54, 128 | 8_192), None);
        assert_eq!(browser_shortcut(true, 0x54, 128 | 8), None);
        assert_eq!(browser_shortcut(true, 0x54, 0), None);
        assert_eq!(browser_shortcut(true, 0x54, 2), None);
        assert_eq!(browser_shortcut(true, 0x57, 128 | 2), None);
        // Plain Cmd/Ctrl+C belongs to the page selection, not the shell.
        assert_eq!(browser_shortcut(true, 0x43, 128), None);
        assert_eq!(browser_shortcut(true, 0x58, 128), None);
        // Quit belongs to the shell so CEF must leave Cmd/Ctrl+Q alone; the
        // native event monitor routes it through graceful winit shutdown.
        assert_eq!(browser_shortcut(true, 0x51, 128), None);
        assert_eq!(browser_shortcut_from_char(true, 'q' as u16, 128), None);
    }
}
