#![allow(dead_code)]

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
    /// F12 was pressed while the CEF web view had focus.
    OpenDevTools {
        tab_id: u64,
    },
    /// Cmd/Ctrl+K was pressed while the CEF web view had focus.
    OpenCommandPalette {
        tab_id: u64,
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
            Self::OpenDevTools { tab_id } => f
                .debug_struct("OpenDevTools")
                .field("tab_id", tab_id)
                .finish(),
            Self::OpenCommandPalette { tab_id } => f
                .debug_struct("OpenCommandPalette")
                .field("tab_id", tab_id)
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
#[derive(Debug, Clone)]
pub enum UiAction {
    NewTab { url: Option<String> },
    CloseTab { tab_id: u64 },
    SelectTab { tab_id: u64 },
    Navigate { tab_id: u64, url: String },
    GoBack { tab_id: u64 },
    GoForward { tab_id: u64 },
    Reload { tab_id: u64, ignore_cache: bool },
    Stop { tab_id: u64 },
    HibernateAllBackground,
    OpenDevTools { tab_id: u64 },
}
