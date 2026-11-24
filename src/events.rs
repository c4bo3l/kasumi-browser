#![allow(dead_code)]

use crate::config::{BrowserPreferences, WorkspaceColor, WorkspaceIcon};

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
    /// Apply preferences changed in the native Settings overlay.
    ApplyPreferences(BrowserPreferences),
}
