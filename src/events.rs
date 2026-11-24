#![allow(dead_code)]

use crate::config::{BrowserPreferences, WorkspaceColor, WorkspaceIcon};

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
/// into the Downloads Settings page without exposing CEF objects to the UI.
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
    /// UI thread opens a native Kasumi tab instead.
    OpenUrlFromTab {
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
}

/// Native browser shortcuts mirrored from CEF's `on_pre_key_event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserShortcut {
    NewTab,
    CloseTab,
    ReopenClosedTab,
    OpenCommandPalette,
    OpenDevTools,
    OpenSettings,
    OpenDownloads,
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
            Self::OpenUrlFromTab { url, background } => f
                .debug_struct("OpenUrlFromTab")
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
    /// Switch the visible native workspace and its active tab.
    SwitchWorkspace {
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
    /// Open the native Downloads page.
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
    /// Apply preferences changed in the native Settings overlay.
    ApplyPreferences(BrowserPreferences),
}

#[cfg(test)]
mod tests {
    use super::{DownloadEngine, DownloadInfo, DownloadState};

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
}
