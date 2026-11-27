#![allow(dead_code)]

use crate::config::{BrowserPreferences, WorkspaceColor, WorkspaceIcon};
use cef::{ImplMediaAccessCallback, ImplPermissionPromptCallback};

/// CEF media-access bit values (`cef_media_access_permission_types_t`).
///
/// Keeping them here lets the UI and persistence layer describe a WebRTC
/// request without depending on CEF's generated enum in every module.
pub const MEDIA_PERMISSION_AUDIO: u32 = 1;
pub const MEDIA_PERMISSION_VIDEO: u32 = 2;
pub const MEDIA_PERMISSION_DESKTOP_AUDIO: u32 = 4;
pub const MEDIA_PERMISSION_DESKTOP_VIDEO: u32 = 8;

/// Console marker used by the renderer shim to request Kasumi's native
/// screen/window picker before CEF builds a `getDisplayMedia` stream.
pub const DISPLAY_MEDIA_CONSOLE_PREFIX: &str = "__KASUMI_DISPLAY_MEDIA_REQUEST__:";

/// A CEF media permission bitmask with small helpers shared by the permission
/// handler, the native prompt, and SQLite persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaAccessPermissions(pub u32);

impl MediaAccessPermissions {
    pub const AUDIO: u32 = MEDIA_PERMISSION_AUDIO;
    pub const VIDEO: u32 = MEDIA_PERMISSION_VIDEO;
    pub const DESKTOP_AUDIO: u32 = MEDIA_PERMISSION_DESKTOP_AUDIO;
    pub const DESKTOP_VIDEO: u32 = MEDIA_PERMISSION_DESKTOP_VIDEO;
    const KNOWN_BITS: u32 = Self::AUDIO | Self::VIDEO | Self::DESKTOP_AUDIO | Self::DESKTOP_VIDEO;

    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    /// Remove CEF bits this browser does not know how to describe or persist.
    /// Unknown bits are never granted from a saved decision.
    pub fn recognized_bits(self) -> u32 {
        self.0 & Self::KNOWN_BITS
    }

    /// True when a newer CEF milestone requested a bit Kasumi cannot describe.
    /// Such a request is always shown to the user instead of auto-resolving.
    pub fn has_unrecognized_bits(self) -> bool {
        self.0 & !Self::KNOWN_BITS != 0
    }

    /// Storage keys for every recognized requested permission, in a stable
    /// order. Unknown bits are deliberately ignored: they cannot be described
    /// in the prompt and therefore must not be auto-granted from SQLite.
    pub fn kinds(self) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        for (bit, kind) in [
            (Self::AUDIO, "microphone"),
            (Self::VIDEO, "camera"),
            (Self::DESKTOP_AUDIO, "desktop-audio"),
            (Self::DESKTOP_VIDEO, "desktop-video"),
        ] {
            if self.contains(bit) {
                kinds.push(kind);
            }
        }
        kinds
    }

    /// Bit value belonging to a persisted permission key.
    pub fn bits_for_kind(kind: &str) -> u32 {
        match kind {
            "microphone" => Self::AUDIO,
            "camera" => Self::VIDEO,
            "desktop-audio" => Self::DESKTOP_AUDIO,
            "desktop-video" => Self::DESKTOP_VIDEO,
            _ => 0,
        }
    }

    /// Human-readable prompt text, for example "microphone and camera".
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.contains(Self::AUDIO) {
            parts.push("microphone");
        }
        if self.contains(Self::VIDEO) {
            parts.push("camera");
        }
        if self.contains(Self::DESKTOP_AUDIO) {
            parts.push("system audio");
        }
        if self.contains(Self::DESKTOP_VIDEO) {
            parts.push("screen recording");
        }
        match parts.len() {
            0 => "media access".to_string(),
            1 => parts[0].to_string(),
            _ => {
                let last = parts.pop().unwrap_or("");
                format!("{} and {last}", parts.join(", "))
            }
        }
    }
}

/// One-shot CEF callback owned by the UI thread until a native prompt resolves.
///
/// Chrome-style CEF windows deliver camera/microphone requests through
/// `on_show_permission_prompt`; Kasumi's Alloy-style embedded tabs use
/// `on_request_media_access_permission` for all media bits. Both paths feed the
/// same Kasumi prompt, so the callback variant is kept next to the event.
#[derive(Clone)]
pub enum MediaPermissionCallback {
    MediaAccess(cef::MediaAccessCallback),
    PermissionPrompt(cef::PermissionPromptCallback),
    #[cfg(test)]
    Test(TestPermissionCallback),
}

/// Test-only callback whose invocations can be counted without loading CEF.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct TestPermissionCallback {
    pub granted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub denied: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl TestPermissionCallback {
    pub fn calls(&self) -> usize {
        use std::sync::atomic::Ordering;
        self.granted.load(Ordering::SeqCst) + self.denied.load(Ordering::SeqCst)
    }
}

/// Result used for Chrome-runtime permission prompts.
///
/// CEF's `PermissionPromptCallback` routes ACCEPT/DENY through
/// `PermissionRequestManager::Accept/Deny`, which unconditionally moves every
/// request's one-shot callback and traps when a prompt has already been
/// decided (navigation, preemption, duplicate request, or a content-setting
/// grant). `DISMISS` uses the guarded cancellation path instead. Kasumi grants
/// the actual camera/microphone access through `MediaAccessCallback` and keeps
/// Chromium's content settings in sync separately, so dismissing this prompt
/// cannot leave WebRTC blocked.
fn permission_prompt_result_for_decision(_granted: bool) -> cef::PermissionRequestResult {
    cef::PermissionRequestResult::DISMISS
}

impl MediaPermissionCallback {
    /// Resolve a one-shot callback. Callers must remove it from pending state
    /// before invoking this so CEF never sees the same callback twice.
    pub fn resolve(self, granted: bool, permissions: u32) {
        match self {
            Self::MediaAccess(callback) => {
                if granted {
                    callback.cont(permissions);
                } else {
                    callback.cancel();
                }
            }
            // Dismiss the prompt wrapper instead of accepting/denying it.
            // The real WebRTC grant comes from `MediaAccessCallback`, and
            // `TabManager` updates the request context's content settings so
            // future requests are auto-approved by Chromium itself.
            Self::PermissionPrompt(callback) => {
                callback.cont(permission_prompt_result_for_decision(granted))
            }
            #[cfg(test)]
            Self::Test(callback) => {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let counter: &AtomicUsize = if granted {
                    &callback.granted
                } else {
                    &callback.denied
                };
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

impl std::fmt::Debug for MediaPermissionCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MediaAccess(_) => f.write_str("MediaAccessCallback"),
            Self::PermissionPrompt(_) => f.write_str("PermissionPromptCallback"),
            #[cfg(test)]
            Self::Test(_) => f.write_str("TestPermissionCallback"),
        }
    }
}

/// Normalize a CEF requesting origin into the key used by `site_permissions`.
/// CEF may supply either `https://example.com` or `https://example.com/`; both
/// must resolve to the same saved decision.
pub fn normalize_permission_origin(origin: &str) -> String {
    let trimmed = origin.trim();
    if let Ok(parsed) = url::Url::parse(trimmed) {
        if matches!(parsed.scheme(), "http" | "https") {
            let serialized = parsed.origin().ascii_serialization();
            if serialized != "null" && !serialized.is_empty() {
                return serialized;
            }
        }
    }
    trimmed.trim_end_matches('/').to_ascii_lowercase()
}

/// Which engine owns a download. CEF handles non-HTTP downloads and the brief
/// path-resolution handshake; Kasumi's native segmented engine owns HTTP(S)
/// downloads so pause/resume/parallel transfers stay in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadEngine {
    Cef,
    Parallel,
}

impl DownloadEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cef => "cef",
            Self::Parallel => "parallel",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "parallel" => Self::Parallel,
            _ => Self::Cef,
        }
    }
}

/// Lifecycle state of a native download. Stored as text in SQLite and mirrored
/// into the native downloads bubble without exposing CEF objects to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    InProgress,
    Paused,
    Completed,
    Canceled,
    Interrupted,
}

impl DownloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "canceled" => Self::Canceled,
            "interrupted" => Self::Interrupted,
            _ => Self::InProgress,
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled | Self::Interrupted)
    }
}

/// Owned snapshot of a CEF download item. Only plain data crosses the CEF/UI
/// boundary; the CEF `DownloadItem` itself must never be retained outside its
/// callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadInfo {
    pub id: u32,
    pub tab_id: u64,
    pub url: String,
    pub suggested_name: String,
    pub full_path: String,
    pub received_bytes: i64,
    pub total_bytes: i64,
    pub percent_complete: i32,
    pub current_speed: i64,
    pub state: DownloadState,
    pub engine: DownloadEngine,
    /// Number of simultaneous HTTP Range segments. `0` before probing, `1` when
    /// the server cannot do ranges, otherwise the configured parallel fan-out.
    pub segments: u8,
    /// Scratch file used by the native engine while a download is active.
    pub temp_path: String,
    pub started_at: u64,
    pub updated_at: u64,
}

impl DownloadInfo {
    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    /// File name used in native completion/error notifications.
    pub fn display_name(&self) -> String {
        let name = self.suggested_name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
        std::path::Path::new(self.full_path.trim())
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".to_string())
    }
}

/// Outcome of a download that should be surfaced as a native notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadNotificationKind {
    Completed,
    Failed,
}

/// Plain-data notification emitted when a transfer reaches a terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadNotification {
    pub download_id: u32,
    pub file_name: String,
    pub kind: DownloadNotificationKind,
}

/// Paper size used by Kasumi's Linux print dialog. CEF only needs the physical
/// dimensions in device units, so the UI keeps this as plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintPaperSize {
    Letter,
    A4,
}

impl PrintPaperSize {
    /// Width and height in inches in portrait orientation.
    pub fn inches(self) -> (f64, f64) {
        match self {
            Self::Letter => (8.5, 11.0),
            Self::A4 => (8.27, 11.69),
        }
    }
}

/// Plain-data print settings shared between the native print dialog, the CEF
/// `PrintHandler`, and the CUPS spooler. No CEF objects are retained outside the
/// main UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintOptions {
    /// CUPS destination name. Empty means the system default printer.
    pub printer: String,
    pub paper: PrintPaperSize,
    pub copies: u32,
    pub landscape: bool,
    pub color: bool,
    pub selection_only: bool,
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            printer: String::new(),
            paper: PrintPaperSize::Letter,
            copies: 1,
            landscape: false,
            color: true,
            selection_only: false,
        }
    }
}

impl PrintOptions {
    /// Clamp UI input before it reaches CEF or `lp`.
    pub fn sanitized(&self) -> Self {
        Self {
            printer: self.printer.trim().to_string(),
            copies: self.copies.clamp(1, 999),
            selection_only: self.selection_only,
            ..*self
        }
    }
}

/// A Web Notification API notification mirrored from the renderer shim.
/// Kept as plain owned data so CEF handler threads never retain page objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebNotification {
    pub id: String,
    pub tab_id: u64,
    pub origin: String,
    pub title: String,
    pub body: String,
    pub tag: String,
    pub require_interaction: bool,
}

/// Progress and control results from Kasumi's native segmented downloader.
/// These are plain owned values so a CEF `UrlRequestClient` callback never
/// retains a request or performs disk I/O on a Chromium thread.
#[derive(Clone)]
pub enum ParallelDownloadEvent {
    ProbeResult {
        download_id: u32,
        generation: u64,
        status: i32,
        supports_ranges: bool,
        total_bytes: i64,
        final_url: String,
    },
    SegmentData {
        download_id: u32,
        generation: u64,
        segment_index: u8,
        offset: i64,
        bytes: std::sync::Arc<[u8]>,
    },
    SegmentComplete {
        download_id: u32,
        generation: u64,
        segment_index: u8,
        received: i64,
        success: bool,
    },
    SegmentFailed {
        download_id: u32,
        generation: u64,
        segment_index: u8,
    },
}

impl std::fmt::Debug for ParallelDownloadEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProbeResult {
                download_id,
                generation,
                status,
                supports_ranges,
                total_bytes,
                final_url,
            } => f
                .debug_struct("ProbeResult")
                .field("download_id", download_id)
                .field("generation", generation)
                .field("status", status)
                .field("supports_ranges", supports_ranges)
                .field("total_bytes", total_bytes)
                .field("final_url", final_url)
                .finish(),
            Self::SegmentData {
                download_id,
                generation,
                segment_index,
                offset,
                bytes,
            } => f
                .debug_struct("SegmentData")
                .field("download_id", download_id)
                .field("generation", generation)
                .field("segment_index", segment_index)
                .field("offset", offset)
                .field("bytes", &bytes.len())
                .finish(),
            Self::SegmentComplete {
                download_id,
                generation,
                segment_index,
                received,
                success,
            } => f
                .debug_struct("SegmentComplete")
                .field("download_id", download_id)
                .field("generation", generation)
                .field("segment_index", segment_index)
                .field("received", received)
                .field("success", success)
                .finish(),
            Self::SegmentFailed {
                download_id,
                generation,
                segment_index,
            } => f
                .debug_struct("SegmentFailed")
                .field("download_id", download_id)
                .field("generation", generation)
                .field("segment_index", segment_index)
                .finish(),
        }
    }
}

/// Events sent from CEF handler callbacks (TID_UI / TID_IO) to the main UI thread
#[derive(Clone)]
pub enum BrowserEvent {
    TitleChange {
        tab_id: u64,
        title: String,
    },
    AddressChange {
        tab_id: u64,
        url: String,
    },
    LoadingStateChange {
        tab_id: u64,
        is_loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    LoadingProgressChange {
        tab_id: u64,
        progress: f64,
    },
    RequestBlocked {
        tab_id: u64,
    },
    /// A CEF browser instance finished closing (from `on_before_close`).
    /// Distinct from a UI tab close: the tab metadata may still be alive
    /// because the browser was hibernated or the whole app is shutting down.
    BrowserClosed {
        tab_id: u64,
    },
    NewWindow {
        url: String,
    },
    /// Chromium asked to open a link in another tab (middle-click, Ctrl+click,
    /// or a popup disposition). The source navigation is cancelled and the main
    /// UI thread opens a native Kasumi tab instead. `tab_id` is the originating
    /// browser so incognito popups inherit private isolation.
    OpenUrlFromTab {
        tab_id: u64,
        url: String,
        background: bool,
    },
    /// A browser shortcut was pressed while the CEF web view had focus. It is
    /// forwarded here because child web views do not propagate key events to
    /// the native winit parent window.
    BrowserShortcut {
        tab_id: u64,
        shortcut: BrowserShortcut,
    },
    /// Context menu "View page source" was selected.
    ViewSource {
        tab_id: u64,
        url: String,
    },
    /// CEF discovered one or more favicon URLs for a page.
    FaviconUrlChanged {
        tab_id: u64,
        urls: Vec<String>,
    },
    /// A favicon image finished downloading on CEF's UI thread.
    FaviconLoaded {
        tab_id: u64,
        url: String,
        rgba: std::sync::Arc<[u8]>,
        width: u32,
        height: u32,
    },
    BrowserCreated {
        tab_id: u64,
        browser: cef::Browser,
    },
    /// A CEF download was accepted and is about to start.
    DownloadStarted(DownloadInfo),
    /// A download item changed state, bytes, speed, or destination. The CEF
    /// callback is retained only on the UI thread so pause/resume/cancel can be
    /// dispatched from native chrome without touching CEF handler threads.
    DownloadUpdated {
        info: DownloadInfo,
        callback: Option<cef::DownloadItemCallback>,
    },
    /// Native segmented downloader event. Always consumed on the main UI thread.
    ParallelDownload(ParallelDownloadEvent),
    /// A page asked for camera/microphone/desktop capture access. The CEF
    /// callback is retained only on the main UI thread until the native prompt
    /// is resolved, so the decision can never be made on a Chromium thread.
    MediaAccessPermissionRequested {
        request_id: u64,
        tab_id: u64,
        origin: String,
        permissions: u32,
        callback: MediaPermissionCallback,
        /// Present for Chrome-runtime `on_show_permission_prompt` requests so
        /// CEF-initiated dismissals can cancel the matching Kasumi prompt.
        prompt_id: Option<u64>,
    },
    /// CEF dismissed a Chrome-runtime permission prompt before Kasumi resolved
    /// it. No site rule is saved for this case.
    PermissionPromptDismissed {
        tab_id: u64,
        prompt_id: u64,
    },
    /// Renderer shim reported a `getDisplayMedia` call. The main UI thread owns
    /// the native source picker and later resolves the page promise by token.
    DisplayMediaPickerRequested {
        tab_id: u64,
        token: String,
    },
    /// A page called `Notification.requestPermission()`. `token` identifies the
    /// renderer promise that the native prompt resolves.
    NotificationPermissionRequested {
        request_id: u64,
        tab_id: u64,
        origin: String,
        token: String,
    },
    /// A page constructed a notification and the renderer shim forwarded its
    /// content for native display.
    WebNotificationShown(WebNotification),
    /// A page called `notification.close()` for a notification that may still be
    /// visible in the native notification window.
    WebNotificationClosed {
        tab_id: u64,
        id: String,
    },
    /// Result of a CEF `find()` operation on a tab.
    FindResult {
        tab_id: u64,
        identifier: i32,
        count: i32,
        active_match_ordinal: i32,
        final_update: bool,
    },
    /// Result from the rich find shim installed in the renderer. This carries
    /// context snippets and advanced-mode match metadata that CEF's native
    /// `FindHandler` does not expose.
    FindResultRich {
        tab_id: u64,
        request_id: u64,
        count: i32,
        active_ordinal: i32,
        snippets: Vec<FindSnippet>,
        error: String,
        cleared: bool,
    },
    /// CEF supplied the default Linux print settings. The UI merges them with
    /// the last used options before opening Kasumi's native print panel.
    PrintDefaultsLoaded {
        tab_id: u64,
        options: PrintOptions,
    },
    /// Linux `PrintHandler` needs a native print panel. The callback is kept on
    /// the main UI thread until the user confirms or cancels.
    PrintDialogRequested {
        tab_id: u64,
        has_selection: bool,
        callback: cef::PrintDialogCallback,
    },
    /// Available CUPS destinations discovered off the UI thread.
    PrintersDiscovered {
        printers: Vec<String>,
        default_printer: String,
    },
    /// The CUPS worker finished a print job. `callback` is continued on the
    /// main UI thread so no Chromium callback fires from the worker.
    PrintJobFinished {
        tab_id: u64,
        success: bool,
        message: String,
        callback: cef::PrintJobCallback,
    },
}

/// Options for a find request shared by the native CEF path and the rich
/// JavaScript highlighter path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FindOptions {
    pub regex: bool,
    pub whole_word: bool,
    pub multi_term: bool,
    pub all_tabs: bool,
}

impl FindOptions {
    /// True when the query needs the rich renderer shim instead of CEF's plain
    /// substring matcher.
    pub fn is_advanced(self) -> bool {
        self.regex || self.whole_word || self.multi_term
    }
}

/// One context snippet emitted by the rich find shim. All text is bounded by
/// the renderer before it crosses the CEF bridge.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSnippet {
    #[serde(default)]
    pub ordinal: i32,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub matched: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub href: String,
    #[serde(default)]
    pub line: String,
    #[serde(default)]
    pub sentence: String,
    #[serde(default)]
    pub term_index: usize,
}

/// Native browser shortcuts mirrored from CEF's `on_pre_key_event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserShortcut {
    NewTab,
    /// Open an isolated incognito space in the native window.
    NewPrivateWindow,
    CloseTab,
    ReopenClosedTab,
    OpenCommandPalette,
    OpenDevTools,
    FindInPage,
    Print,
    OpenSettings,
    OpenDownloads,
    OpenBookmarkManager,
    /// Copy the active page URL to the system clipboard. The clipboard is owned
    /// by egui, so the shell defers the write to the next chrome frame.
    CopyUrl,
    /// Gracefully close all CEF browsers and exit. Handled by the shell; CEF
    /// key forwarding deliberately never emits it because TabManager cannot
    /// exit the winit event loop.
    Quit,
}

impl std::fmt::Debug for BrowserEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TitleChange { tab_id, title } => f
                .debug_struct("TitleChange")
                .field("tab_id", tab_id)
                .field("title", title)
                .finish(),
            Self::AddressChange { tab_id, url } => f
                .debug_struct("AddressChange")
                .field("tab_id", tab_id)
                .field("url", url)
                .finish(),
            Self::LoadingStateChange {
                tab_id,
                is_loading,
                can_go_back,
                can_go_forward,
            } => f
                .debug_struct("LoadingStateChange")
                .field("tab_id", tab_id)
                .field("is_loading", is_loading)
                .field("can_go_back", can_go_back)
                .field("can_go_forward", can_go_forward)
                .finish(),
            Self::LoadingProgressChange { tab_id, progress } => f
                .debug_struct("LoadingProgressChange")
                .field("tab_id", tab_id)
                .field("progress", progress)
                .finish(),
            Self::RequestBlocked { tab_id } => f
                .debug_struct("RequestBlocked")
                .field("tab_id", tab_id)
                .finish(),
            Self::BrowserClosed { tab_id } => f
                .debug_struct("BrowserClosed")
                .field("tab_id", tab_id)
                .finish(),
            Self::NewWindow { url } => f.debug_struct("NewWindow").field("url", url).finish(),
            Self::OpenUrlFromTab {
                tab_id,
                url,
                background,
            } => f
                .debug_struct("OpenUrlFromTab")
                .field("tab_id", tab_id)
                .field("url", url)
                .field("background", background)
                .finish(),
            Self::BrowserShortcut { tab_id, shortcut } => f
                .debug_struct("BrowserShortcut")
                .field("tab_id", tab_id)
                .field("shortcut", shortcut)
                .finish(),
            Self::ViewSource { tab_id, url } => f
                .debug_struct("ViewSource")
                .field("tab_id", tab_id)
                .field("url", url)
                .finish(),
            Self::FaviconUrlChanged { tab_id, urls } => f
                .debug_struct("FaviconUrlChanged")
                .field("tab_id", tab_id)
                .field("urls", urls)
                .finish(),
            Self::FaviconLoaded {
                tab_id,
                url,
                rgba,
                width,
                height,
            } => f
                .debug_struct("FaviconLoaded")
                .field("tab_id", tab_id)
                .field("url", url)
                .field("bytes", &rgba.len())
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::BrowserCreated { tab_id, .. } => f
                .debug_struct("BrowserCreated")
                .field("tab_id", tab_id)
                .finish(),
            Self::DownloadStarted(info) => f
                .debug_struct("DownloadStarted")
                .field("id", &info.id)
                .field("tab_id", &info.tab_id)
                .field("suggested_name", &info.suggested_name)
                .finish(),
            Self::DownloadUpdated { info, .. } => f
                .debug_struct("DownloadUpdated")
                .field("id", &info.id)
                .field("tab_id", &info.tab_id)
                .field("state", &info.state)
                .field("percent_complete", &info.percent_complete)
                .finish(),
            Self::MediaAccessPermissionRequested {
                request_id,
                tab_id,
                origin,
                permissions,
                callback: _,
                prompt_id,
            } => f
                .debug_struct("MediaAccessPermissionRequested")
                .field("request_id", request_id)
                .field("tab_id", tab_id)
                .field("origin", origin)
                .field("permissions", permissions)
                .field("prompt_id", prompt_id)
                .finish(),
            Self::PermissionPromptDismissed { tab_id, prompt_id } => f
                .debug_struct("PermissionPromptDismissed")
                .field("tab_id", tab_id)
                .field("prompt_id", prompt_id)
                .finish(),
            Self::DisplayMediaPickerRequested { tab_id, token } => f
                .debug_struct("DisplayMediaPickerRequested")
                .field("tab_id", tab_id)
                .field("token", token)
                .finish(),
            Self::NotificationPermissionRequested {
                request_id,
                tab_id,
                origin,
                token,
            } => f
                .debug_struct("NotificationPermissionRequested")
                .field("request_id", request_id)
                .field("tab_id", tab_id)
                .field("origin", origin)
                .field("token", token)
                .finish(),
            Self::WebNotificationShown(notification) => f
                .debug_struct("WebNotificationShown")
                .field("tab_id", &notification.tab_id)
                .field("id", &notification.id)
                .field("title", &notification.title)
                .finish(),
            Self::WebNotificationClosed { tab_id, id } => f
                .debug_struct("WebNotificationClosed")
                .field("tab_id", tab_id)
                .field("id", id)
                .finish(),
            Self::FindResult {
                tab_id,
                identifier,
                count,
                active_match_ordinal,
                final_update,
            } => f
                .debug_struct("FindResult")
                .field("tab_id", tab_id)
                .field("identifier", identifier)
                .field("count", count)
                .field("active_match_ordinal", active_match_ordinal)
                .field("final_update", final_update)
                .finish(),
            Self::FindResultRich {
                tab_id,
                request_id,
                count,
                active_ordinal,
                snippets,
                error,
                cleared,
            } => f
                .debug_struct("FindResultRich")
                .field("tab_id", tab_id)
                .field("request_id", request_id)
                .field("count", count)
                .field("active_ordinal", active_ordinal)
                .field("snippets", &snippets.len())
                .field("error", error)
                .field("cleared", cleared)
                .finish(),
            Self::PrintDefaultsLoaded { tab_id, options } => f
                .debug_struct("PrintDefaultsLoaded")
                .field("tab_id", tab_id)
                .field("options", options)
                .finish(),
            Self::PrintDialogRequested {
                tab_id,
                has_selection,
                callback: _,
            } => f
                .debug_struct("PrintDialogRequested")
                .field("tab_id", tab_id)
                .field("has_selection", has_selection)
                .finish(),
            Self::PrintersDiscovered {
                printers,
                default_printer,
            } => f
                .debug_struct("PrintersDiscovered")
                .field("printers", &printers.len())
                .field("default_printer", default_printer)
                .finish(),
            Self::PrintJobFinished {
                tab_id,
                success,
                message,
                callback: _,
            } => f
                .debug_struct("PrintJobFinished")
                .field("tab_id", tab_id)
                .field("success", success)
                .field("message", message)
                .finish(),
            Self::ParallelDownload(event) => {
                f.debug_tuple("ParallelDownload").field(event).finish()
            }
        }
    }
}

/// Actions initiated by the native Rust UI or keyboard shortcuts, dispatched to TabManager
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    NewTab {
        url: Option<String>,
    },
    /// Open an incognito space. Private tabs share one ephemeral CEF request
    /// context and are excluded from every SQLite write.
    NewPrivateWindow,
    ReopenClosedTab,
    CloseTab {
        tab_id: u64,
    },
    CloseOtherTabs {
        tab_id: u64,
    },
    CloseTabsToRight {
        tab_id: u64,
    },
    DuplicateTab {
        tab_id: u64,
    },
    TogglePinTab {
        tab_id: u64,
    },
    SelectTab {
        tab_id: u64,
    },
    Navigate {
        tab_id: u64,
        url: String,
    },
    GoBack {
        tab_id: u64,
    },
    GoForward {
        tab_id: u64,
    },
    Reload {
        tab_id: u64,
        ignore_cache: bool,
    },
    Stop {
        tab_id: u64,
    },
    HibernateAllBackground,
    OpenDevTools {
        tab_id: u64,
    },
    /// Open the platform print dialog for one tab's live CEF view.
    Print {
        tab_id: u64,
    },
    /// Resolve Kasumi's native Linux print panel. `accept: false` cancels the
    /// pending CEF callback without sending a job to CUPS.
    RespondPrintDialog {
        tab_id: u64,
        accept: bool,
        options: PrintOptions,
    },
    /// Switch the visible native workspace and its active tab.
    SwitchWorkspace {
        workspace_id: u64,
    },
    /// Move an unpinned tab to another workspace without reloading it.
    MoveTabToWorkspace {
        tab_id: u64,
        workspace_id: u64,
    },
    /// Create a workspace with the user's chosen name, icon, and accent colour.
    CreateWorkspace {
        name: String,
        icon: WorkspaceIcon,
        color: WorkspaceColor,
    },
    /// Update the name, icon, and accent colour of an existing workspace.
    UpdateWorkspace {
        workspace_id: u64,
        name: String,
        icon: WorkspaceIcon,
        color: WorkspaceColor,
    },
    /// Delete a workspace. Its tabs move to another workspace and its bookmark
    /// folder is archived rather than discarded.
    DeleteWorkspace {
        workspace_id: u64,
    },
    /// Choose the workspace that opens at startup; the current session stays put.
    SetDefaultWorkspace {
        workspace_id: u64,
    },
    /// Save the active tab in a folder named after the active workspace.
    AddBookmark {
        tab_id: u64,
    },
    /// Open a bookmark in the current workspace, regardless of its folder.
    OpenBookmark {
        url: String,
    },
    /// Edit a bookmark's metadata and optionally move it to another folder.
    /// The original workspace/URL pair identifies the row being changed.
    SaveBookmark {
        original_workspace_id: u64,
        original_url: String,
        workspace_id: u64,
        url: String,
        title: String,
        folder: String,
    },
    /// Remove a bookmark identified by its workspace and URL.
    DeleteBookmark {
        workspace_id: u64,
        url: String,
    },
    /// Open the native downloads bubble.
    OpenDownloads,
    /// Pause an active download through its CEF item callback.
    PauseDownload {
        download_id: u32,
    },
    /// Resume a paused download through its CEF item callback.
    ResumeDownload {
        download_id: u32,
    },
    /// Cancel an active or paused download through its CEF item callback.
    CancelDownload {
        download_id: u32,
    },
    /// Forget a finished download entry without touching the downloaded file.
    RemoveDownload {
        download_id: u32,
    },
    /// Remove every finished download entry from the native history.
    ClearCompletedDownloads,
    /// Retry an interrupted or canceled download by reopening its source URL.
    RetryDownload {
        download_id: u32,
    },
    /// Open a completed download with the operating system's default handler.
    OpenDownloadFile {
        download_id: u32,
    },
    /// Reveal a completed download in the operating system's file manager.
    RevealDownload {
        download_id: u32,
    },
    /// Persist a live rail expand/collapse change so the next launch restores it.
    SetRailExpanded(bool),
    /// Apply preferences changed in the native Settings overlay.
    ApplyPreferences(BrowserPreferences),
    /// Resolve a native media-access prompt. `TabManager` owns the one-shot CEF
    /// callback; this action carries only its request id so UI state stays Clone.
    /// `persist` is false for Escape/click-outside dismissal so a transient
    /// dismissal never becomes a permanent site rule.
    ResolveMediaAccessPermission {
        request_id: u64,
        granted: bool,
        persist: bool,
    },
    /// Resolve the native source picker. `device_id` is a Chromium
    /// `DesktopMediaID`; `None` rejects the pending `getDisplayMedia` promise.
    ResolveDisplayMediaPicker {
        token: String,
        device_id: Option<String>,
    },
    /// Resolve a native Web Notification permission prompt. `persist` is false
    /// for Escape/click-outside dismissal so a transient close never becomes a
    /// permanent site rule.
    ResolveNotificationPermission {
        request_id: u64,
        granted: bool,
        persist: bool,
    },
    /// Activate the notification's source tab and dispatch its `click` handler.
    ActivateWebNotification {
        tab_id: u64,
        notification_id: String,
    },
    /// Dismiss a native web notification and dispatch its `close` handler.
    DismissWebNotification {
        tab_id: u64,
        notification_id: String,
    },
    /// Run find on a tab (or every live tab when `all_tabs` is set).
    FindInPage {
        tab_id: u64,
        query: String,
        forward: bool,
        match_case: bool,
        find_next: bool,
        options: FindOptions,
    },
    /// Focus and scroll to one rich find match.
    JumpToFindMatch {
        tab_id: u64,
        ordinal: i32,
    },
    /// Stop an in-page find on a tab.
    StopFinding {
        tab_id: u64,
        clear_selection: bool,
    },
    /// Stop every find session, used when the shared find bar closes.
    StopFindingAll {
        clear_selection: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_permission_origin, permission_prompt_result_for_decision, DownloadEngine,
        DownloadInfo, DownloadState, FindOptions, MediaAccessPermissions,
    };

    fn info(state: DownloadState) -> DownloadInfo {
        DownloadInfo {
            id: 7,
            tab_id: 3,
            url: "https://example.com/file.zip".to_string(),
            suggested_name: "file.zip".to_string(),
            full_path: "/Downloads/file.zip".to_string(),
            received_bytes: 5,
            total_bytes: 10,
            percent_complete: 50,
            current_speed: 1024,
            state,
            engine: DownloadEngine::Parallel,
            segments: 8,
            temp_path: "/Downloads/file.zip.kasumi.part".to_string(),
            started_at: 100,
            updated_at: 200,
        }
    }

    #[test]
    fn download_state_round_trips_through_storage_keys() {
        for state in [
            DownloadState::InProgress,
            DownloadState::Paused,
            DownloadState::Completed,
            DownloadState::Canceled,
            DownloadState::Interrupted,
        ] {
            assert_eq!(DownloadState::from_str(state.as_str()), state);
        }
        assert_eq!(
            DownloadState::from_str("unknown"),
            DownloadState::InProgress
        );
    }

    #[test]
    fn download_engine_round_trips_through_storage_keys() {
        for engine in [DownloadEngine::Cef, DownloadEngine::Parallel] {
            assert_eq!(DownloadEngine::from_str(engine.as_str()), engine);
        }
        assert_eq!(DownloadEngine::from_str("unknown"), DownloadEngine::Cef);
    }

    #[test]
    fn only_finished_downloads_are_cleared() {
        assert!(!info(DownloadState::InProgress).is_finished());
        assert!(!info(DownloadState::Paused).is_finished());
        assert!(info(DownloadState::Completed).is_finished());
        assert!(info(DownloadState::Canceled).is_finished());
        assert!(info(DownloadState::Interrupted).is_finished());
    }

    #[test]
    fn media_permission_labels_and_storage_keys_match_cef_bits() {
        assert_eq!(MediaAccessPermissions::AUDIO, 1);
        assert_eq!(MediaAccessPermissions::VIDEO, 2);
        assert_eq!(MediaAccessPermissions::DESKTOP_AUDIO, 4);
        assert_eq!(MediaAccessPermissions::DESKTOP_VIDEO, 8);

        let combined = MediaAccessPermissions::from_bits(3);
        assert_eq!(combined.label(), "microphone and camera");
        assert_eq!(combined.kinds(), vec!["microphone", "camera"]);
        assert_eq!(
            MediaAccessPermissions::from_bits(MediaAccessPermissions::DESKTOP_VIDEO).label(),
            "screen recording"
        );
        assert_eq!(MediaAccessPermissions::bits_for_kind("camera"), 2);
        assert_eq!(MediaAccessPermissions::bits_for_kind("unknown"), 0);
        assert!(MediaAccessPermissions::from_bits(0).kinds().is_empty());
        assert!(!combined.has_unrecognized_bits());
        assert!(MediaAccessPermissions::from_bits(1 << 12).has_unrecognized_bits());
        assert_eq!(
            MediaAccessPermissions::from_bits(1 << 12).recognized_bits(),
            0
        );
    }

    #[test]
    fn permission_prompt_callback_is_always_dismissed() {
        // Accept/Deny run Chromium's unconditional callback path and have
        // already trapped on a consumed one-shot callback. Dismiss is the only
        // guarded result; real media access is granted separately.
        assert_eq!(
            permission_prompt_result_for_decision(true),
            cef::PermissionRequestResult::DISMISS
        );
        assert_eq!(
            permission_prompt_result_for_decision(false),
            cef::PermissionRequestResult::DISMISS
        );
    }

    #[test]
    fn scope_alone_stays_on_the_native_find_path() {
        assert!(!FindOptions::default().is_advanced());
        assert!(!FindOptions {
            all_tabs: true,
            ..FindOptions::default()
        }
        .is_advanced());
        for options in [
            FindOptions {
                regex: true,
                ..FindOptions::default()
            },
            FindOptions {
                whole_word: true,
                ..FindOptions::default()
            },
            FindOptions {
                multi_term: true,
                ..FindOptions::default()
            },
        ] {
            assert!(options.is_advanced());
        }
    }

    #[test]
    fn permission_origins_normalize_to_a_stable_key() {
        assert_eq!(
            normalize_permission_origin("https://Example.com/meeting/"),
            "https://example.com"
        );
        assert_eq!(
            normalize_permission_origin("https://example.com"),
            "https://example.com"
        );
        assert_eq!(normalize_permission_origin("  "), "");
    }
}
