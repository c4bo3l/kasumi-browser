//! Tab lifecycle state machine and RAM-pressure-based hibernation engine.
//!
//! Enforces:
//! - Default idle timeout: 600s (10 min).
//! - High RAM pressure (>= 80%): 30s idle timeout.
//! - Critical RAM pressure (>= 90%): hibernate ALL background tabs immediately.
//! - Never hibernate the active tab.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cef::{
    CefString, ImplBrowser, ImplBrowserHost, ImplDownloadItemCallback, ImplFrame, ImplUrlrequest,
};
use crossbeam_channel::{Receiver, Sender};
use sysinfo::{ProcessesToUpdate, System};

use crate::browser::favicon::create_favicon_download_callback;
use crate::browser::parallel_download::{
    self, part_path_for, safe_file_name, DownloadWriter, SegmentRange,
};
use crate::browser::{adblocker, client};
use crate::config::{
    BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon, WorkspaceStartupBehavior,
    PARALLEL_DOWNLOAD_SEGMENTS, RAM_CHECK_INTERVAL, RAM_CRITICAL_PRESSURE_THRESHOLD,
    RAM_HIGH_PRESSURE_THRESHOLD, TAB_IDLE_TIMEOUT_PRESSURE, TAB_WATCHDOG_TICK_INTERVAL,
};
use crate::events::{
    BrowserEvent, BrowserShortcut, DownloadEngine, DownloadInfo, DownloadNotification,
    DownloadNotificationKind, DownloadState, ParallelDownloadEvent, UiAction,
};
use crate::storage::db::{BookmarkItem, SessionTabSnapshot, StorageHandle};
use crate::ui::chrome::{
    BookmarkInfo, ChromeState, Favicon, TabDisplayState, TabInfo, WorkspaceInfo,
};
use crate::ui::platform_view::{self, ContentRect};

/// Derive the conventional favicon URL from a page URL. This is the fallback
/// when CEF does not report icon links (some sites only provide them after
/// async hydration, and restored tabs may never fire the callback).
fn fallback_favicon_url(page_url: &str) -> Option<String> {
    let page = page_url.strip_prefix("view-source:").unwrap_or(page_url);
    let parsed = url::Url::parse(page).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" || origin.is_empty() {
        return None;
    }
    Some(format!("{origin}/favicon.ico"))
}

/// Prefer a real favicon over Apple touch and oversized decorative icons, all
/// of which CEF may report in the same `on_favicon_urlchange` batch.
fn preferred_favicon_url(urls: &[String]) -> Option<String> {
    let mut best: Option<(i32, usize, String)> = None;
    for (index, url) in urls.iter().enumerate() {
        let lower = url.to_ascii_lowercase();
        let score = if lower.contains("/favicon.ico") || lower.ends_with("/favicon.ico") {
            1000
        } else if lower.contains("favicon") {
            900
        } else if lower.contains(".ico") {
            800
        } else if lower.contains("apple-touch-icon") {
            200
        } else {
            100
        };

        let better = best
            .as_ref()
            .map(|(best_score, best_index, _)| {
                score > *best_score || (score == *best_score && index < *best_index)
            })
            .unwrap_or(true);
        if better {
            best = Some((score, index, url.clone()));
        }
    }
    best.map(|(_, _, url)| url)
}

/// Shared-memory pressure ratio. Zero total memory is treated as no pressure.
pub fn ram_pressure(used_bytes: u64, total_bytes: u64) -> f32 {
    if total_bytes == 0 {
        0.0
    } else {
        used_bytes as f32 / total_bytes as f32
    }
}

pub fn is_critical_pressure(ratio: f32) -> bool {
    ratio >= RAM_CRITICAL_PRESSURE_THRESHOLD
}

pub fn idle_timeout_for_pressure(ratio: f32, normal_timeout: Duration) -> Duration {
    if ratio >= RAM_HIGH_PRESSURE_THRESHOLD {
        // Pressure must never make the timeout longer than the user's choice.
        TAB_IDLE_TIMEOUT_PRESSURE.min(normal_timeout)
    } else {
        normal_timeout
    }
}

/// Decide whether the omnibox should be replaced with the active tab URL.
/// Never clobber text while the user is editing.
pub fn should_sync_omnibox(
    omnibox_focused: bool,
    last_synced_tab_id: Option<u64>,
    last_synced_url: &str,
    current_tab_id: Option<u64>,
    current_url: &str,
    current_text: &str,
) -> bool {
    if omnibox_focused {
        return false;
    }
    current_tab_id.is_none()
        || last_synced_tab_id != current_tab_id
        || last_synced_url != current_url
        || current_text != current_url
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    Active,
    Background { since: Instant },
    Hibernated,
}

/// Metadata preserved when a tab is closed so `Cmd/Ctrl+Z` can undo the close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedTab {
    pub workspace_id: u64,
    pub url: String,
    pub title: String,
    /// Position the tab occupied before it was closed.
    pub index: usize,
    /// Whether the tab belonged to the compact pinned section.
    pub pinned: bool,
}

/// Clamp a restored tab's original position to the current tab count.
pub fn restore_insert_index(original_index: usize, tab_count: usize) -> usize {
    original_index.min(tab_count)
}

/// Number of pinned tabs that remain after excluding one tab. This is the
/// boundary where a pinned tab is inserted (or where an unpinned tab lands
/// when it leaves the pinned section).
pub fn pinned_insert_index(tabs: &[Tab], workspace_id: u64, exclude_index: Option<usize>) -> usize {
    tabs.iter()
        .enumerate()
        .filter(|(index, tab)| {
            Some(*index) != exclude_index && tab.workspace_id == workspace_id && tab.pinned
        })
        .count()
}

/// Choose an insertion index that preserves the workspace-grouped,
/// pinned-before-unpinned invariant used by the tab model and native rail.
pub fn insertion_index(
    tabs: &[Tab],
    workspace_id: u64,
    requested: Option<usize>,
    pinned: bool,
) -> usize {
    let workspace_end = tabs
        .iter()
        .rposition(|tab| tab.workspace_id == workspace_id)
        .map_or(tabs.len(), |index| index + 1);
    let workspace_start = tabs
        .iter()
        .position(|tab| tab.workspace_id == workspace_id)
        .unwrap_or(workspace_end);
    let boundary = workspace_start + pinned_insert_index(tabs, workspace_id, None);
    let requested = requested.unwrap_or(workspace_end);
    if pinned {
        requested.clamp(workspace_start, boundary)
    } else {
        requested.clamp(boundary, workspace_end)
    }
}

/// Capture the metadata needed to undo a tab close. Keeping this pure makes
/// the closed-tab stack's title, index, and workspace behavior testable.
pub fn closed_tab_from(tab: &Tab, index: usize) -> ClosedTab {
    ClosedTab {
        workspace_id: tab.workspace_id,
        url: tab.url.clone(),
        title: tab.title.clone(),
        index,
        pinned: tab.pinned,
    }
}

/// CEF occasionally reports `about:blank` in `OnAfterCreated` before the
/// initial navigation commits. Reissuing the load in that case prevents a
/// newly spawned or restored browser from staying on an empty document.
pub fn is_blank_navigation_url(url: &str) -> bool {
    let url = url.trim();
    url.is_empty() || url.eq_ignore_ascii_case("about:blank")
}

pub fn needs_initial_navigation(current_url: &str, target_url: &str) -> bool {
    !is_blank_navigation_url(target_url) && is_blank_navigation_url(current_url)
}

/// Grace periods for CEF's own initial navigation before a blank frame is
/// retried. The first is short enough to feel instantaneous; later attempts
/// cover CEF dropping a retry while its render frame is still initializing.
const INITIAL_NAVIGATION_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(750),
    Duration::from_millis(1_500),
    Duration::from_millis(3_000),
];

/// One-shot navigation watchdog state for a freshly spawned browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNavigationCheck {
    pub target_url: String,
    pub deadline: Instant,
    pub attempt: usize,
}

/// Delay before the next blank-frame retry. `None` means all retries are spent.
pub fn next_navigation_retry_delay(attempt: usize) -> Option<Duration> {
    INITIAL_NAVIGATION_RETRY_DELAYS.get(attempt).copied()
}

/// Why a browser spawn was queued. Active-only spawns come from focusing a tab;
/// background spawns come from middle-clicking a link and must load even while
/// another tab owns the visible child view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserSpawnMode {
    ActiveOnly,
    Background,
}

impl BrowserSpawnMode {
    fn allows(self, is_active: bool) -> bool {
        match self {
            Self::ActiveOnly => is_active,
            Self::Background => true,
        }
    }
}

/// Pure hibernation decision used by the watchdog. The active tab is never
/// eligible, and only idle background tabs past the timeout are hibernated.
pub fn should_hibernate_tab(
    state: &TabState,
    is_active: bool,
    now: Instant,
    idle_timeout: Duration,
) -> bool {
    if is_active {
        return false;
    }
    match state {
        TabState::Background { since } => now.saturating_duration_since(*since) >= idle_timeout,
        _ => false,
    }
}

pub struct Tab {
    pub id: u64,
    pub workspace_id: u64,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub blocked_requests: u32,
    pub pinned: bool,
    pub state: TabState,
    pub browser: Option<cef::Browser>,
    pub favicon: Option<Favicon>,
}

impl std::fmt::Debug for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tab")
            .field("id", &self.id)
            .field("workspace_id", &self.workspace_id)
            .field("url", &self.url)
            .field("title", &self.title)
            .field("is_loading", &self.is_loading)
            .field("can_go_back", &self.can_go_back)
            .field("can_go_forward", &self.can_go_forward)
            .field("blocked_requests", &self.blocked_requests)
            .field("pinned", &self.pinned)
            .field("state", &self.state)
            .field("has_browser", &self.browser.is_some())
            .field("has_favicon", &self.favicon.is_some())
            .finish()
    }
}

struct ParallelSegment {
    range: SegmentRange,
    received: i64,
    complete: bool,
    request: Option<cef::Urlrequest>,
}

struct ParallelRuntime {
    tab_id: u64,
    /// URL shown in history and used by Retry.
    display_url: String,
    /// URL used by CEF URLRequests (may be the post-redirect response URL).
    request_url: String,
    final_path: PathBuf,
    part_path: PathBuf,
    total_bytes: i64,
    ranged: bool,
    /// Incremented whenever a new batch of URLRequests starts. Late callbacks
    /// from a canceled batch carry the old generation and are discarded.
    generation: u64,
    segments: Vec<ParallelSegment>,
    probe: Option<cef::Urlrequest>,
    last_bytes: i64,
    last_speed_at: Instant,
    started_at: u64,
}

pub struct TabManager {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<u64>,
    pub workspaces: Vec<Workspace>,
    pub active_workspace_id: u64,
    /// Remembers the selected tab in each workspace so switching back restores context.
    active_tab_by_workspace: HashMap<u64, u64>,
    /// Globally visible bookmarks, grouped by workspace name in `folder`.
    pub bookmarks: Vec<BookmarkItem>,
    /// Native download snapshots mirrored from CEF into the downloads bubble.
    downloads: Vec<DownloadInfo>,
    /// Set when a genuinely new transfer starts so the shell can reveal the
    /// native downloads bubble without reopening it for restored history.
    download_bubble_requested: bool,
    /// Terminal download outcomes waiting to be shown as native notifications.
    download_notifications: Vec<DownloadNotification>,
    /// CEF item callbacks retained only on the UI thread for native controls.
    download_callbacks: HashMap<u32, cef::DownloadItemCallback>,
    /// CEF ids that Kasumi took over with the native segmented engine. Their
    /// later CEF updates are discarded so they cannot overwrite native progress.
    superseded_cef_downloads: HashSet<u32>,
    /// Live native segmented transfers, keyed by the same id shown in the UI.
    parallel_downloads: HashMap<u32, ParallelRuntime>,
    /// Background filesystem actor. CEF and winit callbacks only queue writes.
    download_writer: DownloadWriter,
    /// Monotonic token for native request batches. Stale CEF URLRequest callbacks
    /// are ignored after pause, cancel, retry, or shutdown.
    next_download_generation: u64,
    next_tab_id: u64,

    // Lock-free event channels for CEF callbacks -> UI
    event_sender: Sender<BrowserEvent>,
    event_receiver: Receiver<BrowserEvent>,

    // Native window embedding coordinates
    parent_window_view: *mut std::ffi::c_void,
    content_rect: ContentRect,

    // System resource watchdog
    sys: System,
    last_ram_check: Instant,
    last_tick: Instant,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub ram_process_mb: u64,

    // Brave adblock engine shared across all tabs. The enabled flag is shared
    // with CEF handlers so Settings can toggle blocking without recreating
    // browsers or touching CEF from the UI thread.
    adblock_engine: Arc<adblock::Engine>,
    adblock_enabled: Arc<AtomicBool>,

    // Download preferences shared with CEF handlers. The "ask where to save"
    // flag is an atomic; the directory is only cloned from a short-lived lock.
    download_preferences: client::SharedDownloadPreferences,

    // User-configurable idle timeout for the normal-pressure watchdog.
    idle_timeout: Duration,

    // Landing page for fresh tabs, derived from the selected search provider.
    home_url: String,

    // SQLite persistence worker & closed tab stack
    pub storage: Option<StorageHandle>,
    pub recently_closed: Vec<ClosedTab>,

    // Number of `host.close_browser(1)` calls awaiting an `on_before_close` event.
    pending_browser_closes: u32,

    // Browser creation is deferred out of winit window-event callbacks. CEF's
    // external message pump re-enters the run loop on macOS, so creating a
    // browser directly from a key/redraw event can intermittently lose the
    // initial navigation. `tick()` drains this queue from `about_to_wait`.
    pending_browser_spawns: VecDeque<(u64, String, BrowserSpawnMode)>,

    // Retry state for browsers that have not committed their initial URL.
    pending_navigation_checks: HashMap<u64, PendingNavigationCheck>,

    // Set when CEF forwards Cmd/Ctrl+K; consumed by the winit handler.
    command_palette_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+,; consumed by the winit handler.
    settings_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+J; consumed by the winit handler.
    downloads_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+Shift+B; consumed by the winit handler.
    bookmark_manager_requested: bool,

    // Favicon URLs already requested, and URLs seen before the browser exists.
    requested_favicons: HashSet<(u64, String)>,
    pending_favicon_urls: HashMap<u64, Vec<String>>,
    // Only the first successful favicon per navigation wins. Sites often list
    // several icons (apple-touch-icon, large PNGs) and all of them must not
    // overwrite the tab's actual site icon.
    resolved_favicons: HashSet<u64>,
    last_favicon_urls: HashMap<u64, Vec<String>>,
    favicon_preferences: HashMap<u64, String>,
}

/// Choose the startup workspace from the persisted preference, then normalize
/// both flags so later snapshots always carry a single active/default space.
fn normalize_workspace_selection(
    workspaces: &mut [Workspace],
    behavior: WorkspaceStartupBehavior,
) -> u64 {
    let default_id = workspaces
        .iter()
        .find(|workspace| workspace.is_default)
        .map_or(Workspace::DEFAULT_ID, |workspace| workspace.id);
    let last_active_id = workspaces
        .iter()
        .find(|workspace| workspace.is_active)
        .map(|workspace| workspace.id);
    let active_id = match behavior {
        WorkspaceStartupBehavior::LastActive => last_active_id
            .or(Some(default_id))
            .or_else(|| workspaces.first().map(|workspace| workspace.id))
            .unwrap_or(Workspace::DEFAULT_ID),
        WorkspaceStartupBehavior::DefaultWorkspace => Some(default_id)
            .or(last_active_id)
            .or_else(|| workspaces.first().map(|workspace| workspace.id))
            .unwrap_or(Workspace::DEFAULT_ID),
    };
    for workspace in workspaces {
        workspace.is_active = workspace.id == active_id;
        workspace.is_default = workspace.id == default_id;
    }
    active_id
}

impl TabManager {
    pub fn new(
        event_sender: Sender<BrowserEvent>,
        event_receiver: Receiver<BrowserEvent>,
        parent_window_view: *mut std::ffi::c_void,
        content_rect: ContentRect,
        storage: Option<StorageHandle>,
        preferences: &BrowserPreferences,
        workspaces: Vec<Workspace>,
        bookmarks: Vec<BookmarkItem>,
    ) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        let total = sys.total_memory();
        let used = sys.used_memory();
        let adblock_engine = adblocker::create_adblock_engine();
        let adblock_enabled = Arc::new(AtomicBool::new(preferences.adblock_enabled));
        let download_preferences = client::SharedDownloadPreferences::from_preferences(preferences);

        let mut workspaces = workspaces;
        if workspaces.is_empty() {
            workspaces.push(Workspace::default_workspace());
        }
        // Reopen the last active space by default; the configurable default
        // workspace is used when the user prefers a fixed landing space.
        let active_workspace_id =
            normalize_workspace_selection(&mut workspaces, preferences.workspace_startup);

        Self {
            tabs: Vec::new(),
            active_tab_id: None,
            workspaces,
            active_workspace_id,
            active_tab_by_workspace: HashMap::new(),
            bookmarks,
            downloads: Vec::new(),
            download_bubble_requested: false,
            download_notifications: Vec::new(),
            download_callbacks: HashMap::new(),
            superseded_cef_downloads: HashSet::new(),
            parallel_downloads: HashMap::new(),
            download_writer: DownloadWriter::new(),
            next_download_generation: 1,
            next_tab_id: 1,
            event_sender,
            event_receiver,
            parent_window_view,
            content_rect,
            sys,
            last_ram_check: Instant::now() - RAM_CHECK_INTERVAL,
            last_tick: Instant::now(),
            ram_used_bytes: used,
            ram_total_bytes: total,
            ram_process_mb: 0,
            adblock_engine,
            adblock_enabled,
            download_preferences,
            idle_timeout: preferences.idle_timeout(),
            home_url: preferences.home_url().to_string(),
            storage,
            recently_closed: Vec::new(),
            pending_browser_closes: 0,
            pending_browser_spawns: VecDeque::new(),
            pending_navigation_checks: HashMap::new(),
            command_palette_requested: false,
            settings_requested: false,
            downloads_requested: false,
            bookmark_manager_requested: false,
            requested_favicons: HashSet::new(),
            pending_favicon_urls: HashMap::new(),
            resolved_favicons: HashSet::new(),
            last_favicon_urls: HashMap::new(),
            favicon_preferences: HashMap::new(),
        }
    }

    /// Apply live preference changes. Shared CEF state uses atomics so no CEF
    /// handler ever waits on a UI lock.
    pub fn apply_preferences(&mut self, preferences: &BrowserPreferences) {
        let preferences = preferences.clone().normalized();
        self.adblock_enabled
            .store(preferences.adblock_enabled, Ordering::Release);
        self.download_preferences.apply(&preferences);
        self.idle_timeout = preferences.idle_timeout();
        self.home_url = preferences.home_url().to_string();
    }

    /// Replace startup metadata from SQLite. Active CEF callbacks never survive
    /// a restart, so restored entries are history only until retried.
    pub fn seed_downloads(&mut self, mut downloads: Vec<DownloadInfo>) {
        for download in &mut downloads {
            if !download.is_finished() {
                download.state = DownloadState::Interrupted;
                download.current_speed = 0;
            }
        }
        downloads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        self.downloads = downloads;
    }

    /// Copy a download snapshot for shell actions that must not borrow
    /// `TabManager` across a filesystem or navigation call.
    pub fn download_snapshot(&self, download_id: u32) -> Option<DownloadInfo> {
        self.downloads
            .iter()
            .find(|download| download.id == download_id)
            .cloned()
    }

    fn queue_download_notification(&mut self, info: &DownloadInfo, kind: DownloadNotificationKind) {
        if self
            .download_notifications
            .iter()
            .any(|notification| notification.download_id == info.id)
        {
            return;
        }
        self.download_notifications.push(DownloadNotification {
            download_id: info.id,
            file_name: info.display_name(),
            kind,
        });
    }

    fn record_download(&mut self, info: DownloadInfo, callback: Option<cef::DownloadItemCallback>) {
        // After the native engine takes over, CEF may deliver one canceled
        // update from the copy we just stopped. It must not be persisted.
        if info.engine == DownloadEngine::Cef && self.superseded_cef_downloads.contains(&info.id) {
            return;
        }

        let is_new = !self.downloads.iter().any(|download| download.id == info.id);
        let previous_state = self
            .downloads
            .iter()
            .find(|download| download.id == info.id)
            .map(|download| download.state);
        if let Some(existing) = self
            .downloads
            .iter_mut()
            .find(|download| download.id == info.id)
        {
            *existing = info.clone();
        } else {
            self.downloads.push(info.clone());
        }
        self.downloads
            .sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));

        // A transfer can be first observed already in a terminal state (for
        // example, CEF reports a short download as completed on its first
        // update). Treat that like a transition so the outcome is never lost.
        let reached_terminal_state = matches!(
            info.state,
            DownloadState::Completed | DownloadState::Interrupted
        ) && previous_state
            .map_or(true, |previous| previous != info.state);
        if reached_terminal_state {
            match info.state {
                DownloadState::Completed => {
                    self.queue_download_notification(&info, DownloadNotificationKind::Completed);
                }
                DownloadState::Interrupted => {
                    self.queue_download_notification(&info, DownloadNotificationKind::Failed);
                }
                DownloadState::InProgress | DownloadState::Paused | DownloadState::Canceled => {}
            }
        }

        if is_new
            && matches!(
                info.state,
                DownloadState::InProgress | DownloadState::Paused
            )
        {
            self.download_bubble_requested = true;
        }

        if let Some(callback) = callback {
            if info.is_finished() {
                self.download_callbacks.remove(&info.id);
            } else {
                self.download_callbacks.insert(info.id, callback);
            }
        } else if info.is_finished() {
            self.download_callbacks.remove(&info.id);
        }

        if let Some(ref storage) = self.storage {
            storage.save_download(info);
        }
    }

    fn set_download_state(&mut self, download_id: u32, state: DownloadState) {
        let Some(info) = self
            .downloads
            .iter_mut()
            .find(|download| download.id == download_id)
        else {
            return;
        };
        info.state = state;
        if state != DownloadState::InProgress {
            info.current_speed = 0;
        }
        info.updated_at = Self::now_unix_secs();
        let info = info.clone();
        if let Some(ref storage) = self.storage {
            storage.save_download(info);
        }
    }

    fn now_unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default()
    }

    fn request_context_for_tab(&self, tab_id: u64) -> Option<cef::RequestContext> {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.host())
            .and_then(|host| host.request_context())
    }

    fn unique_download_path(directory: &Path, suggested_name: &str) -> PathBuf {
        let name = safe_file_name(suggested_name);
        let candidate = directory.join(&name);
        if !candidate.exists() {
            return candidate;
        }
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("download");
        let extension = Path::new(&name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        for suffix in 1..10_000 {
            let file_name = if extension.is_empty() {
                format!("{stem}-{suffix}")
            } else {
                format!("{stem}-{suffix}.{extension}")
            };
            let candidate = directory.join(file_name);
            if !candidate.exists() {
                return candidate;
            }
        }
        directory.join(format!("{name}-{}", Self::now_unix_secs()))
    }

    fn resolve_download_path(&self, info: &DownloadInfo) -> PathBuf {
        // A path chosen in CEF's native "Save As" dialog must be preserved.
        // Otherwise always save into the configured/default Downloads folder
        // rather than CEF's temp location, which can disappear mid-transfer.
        if self.download_preferences.ask_where_to_save() && !info.full_path.trim().is_empty() {
            return PathBuf::from(&info.full_path);
        }
        let configured = self.download_preferences.download_directory();
        let directory = if configured.trim().is_empty() {
            crate::config::default_download_directory()
        } else {
            PathBuf::from(&configured)
        };
        Self::unique_download_path(&directory, &info.suggested_name)
    }

    fn cancel_native_requests(&mut self, download_id: u32) {
        if let Some(runtime) = self.parallel_downloads.get_mut(&download_id) {
            if let Some(probe) = runtime.probe.take() {
                probe.cancel();
            }
            for segment in &mut runtime.segments {
                if let Some(request) = segment.request.take() {
                    request.cancel();
                }
            }
        }
    }

    /// Start a fresh native transfer. CEF has already resolved the destination
    /// path; the actual bytes are fetched through parallel CEF URLRequests.
    fn start_native_download(&mut self, mut info: DownloadInfo) {
        let final_path = self.resolve_download_path(&info);
        let part_path = part_path_for(&final_path);
        self.cancel_native_requests(info.id);
        self.download_writer.cancel(info.id);

        info.engine = DownloadEngine::Parallel;
        info.full_path = final_path.to_string_lossy().to_string();
        info.temp_path = part_path.to_string_lossy().to_string();
        info.state = DownloadState::InProgress;
        info.received_bytes = 0;
        info.total_bytes = 0;
        info.percent_complete = 0;
        info.current_speed = 0;
        info.segments = 0;
        info.updated_at = Self::now_unix_secs();

        let generation = self.next_download_generation;
        self.next_download_generation = self.next_download_generation.wrapping_add(1);
        let display_url = info.url.clone();
        let runtime = ParallelRuntime {
            tab_id: info.tab_id,
            request_url: display_url.clone(),
            display_url,
            final_path,
            part_path,
            total_bytes: 0,
            ranged: false,
            generation,
            segments: Vec::new(),
            probe: None,
            last_bytes: 0,
            last_speed_at: Instant::now(),
            started_at: info.started_at,
        };
        self.parallel_downloads.insert(info.id, runtime);
        self.superseded_cef_downloads.insert(info.id);

        let context = self.request_context_for_tab(info.tab_id);
        let probe = parallel_download::start_probe(
            &self.event_sender,
            info.id,
            generation,
            &info.url,
            context,
        );
        if let Some(runtime) = self.parallel_downloads.get_mut(&info.id) {
            runtime.probe = probe.clone();
        }
        self.record_download(info.clone(), None);
        if probe.is_none() {
            tracing::warn!(
                "HEAD probe unavailable for download {}; using one GET",
                info.id
            );
            self.begin_single_segment(info.id, 0, false);
        }
    }

    fn begin_single_segment(&mut self, download_id: u32, total_bytes: i64, ranged: bool) {
        let paths = self.parallel_downloads.get(&download_id).map(|runtime| {
            (
                runtime.part_path.clone(),
                runtime.final_path.clone(),
                runtime.request_url.clone(),
                runtime.ranged,
            )
        });
        let Some((part_path, final_path, _, _)) = paths else {
            return;
        };
        self.download_writer
            .start(download_id, part_path, final_path, total_bytes, true);
        let range = if total_bytes > 0 {
            SegmentRange {
                start: 0,
                end: total_bytes - 1,
            }
        } else {
            SegmentRange { start: 0, end: -1 }
        };
        self.start_segment_requests(download_id, vec![range], ranged);
    }

    /// Some servers advertise `Accept-Ranges: bytes` but answer every range
    /// request with the full body. Restart that transfer as one plain GET so
    /// overlapping bytes can never corrupt the output file.
    fn restart_single_non_ranged(&mut self, download_id: u32) {
        let total_bytes = self
            .parallel_downloads
            .get(&download_id)
            .map(|runtime| runtime.total_bytes)
            .unwrap_or(0);
        self.cancel_native_requests(download_id);
        self.download_writer.cancel(download_id);

        let generation = self.next_download_generation;
        self.next_download_generation = self.next_download_generation.wrapping_add(1);
        if let Some(runtime) = self.parallel_downloads.get_mut(&download_id) {
            runtime.generation = generation;
            runtime.ranged = false;
            runtime.segments.clear();
        }
        if let Some(info) = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
        {
            info.segments = 1;
            info.total_bytes = total_bytes;
            info.received_bytes = 0;
            info.percent_complete = 0;
            info.state = DownloadState::InProgress;
            info.updated_at = Self::now_unix_secs();
            let snapshot = info.clone();
            if let Some(ref storage) = self.storage {
                storage.save_download(snapshot);
            }
        }
        self.begin_single_segment(download_id, total_bytes, false);
    }

    fn start_segment_requests(
        &mut self,
        download_id: u32,
        ranges: Vec<SegmentRange>,
        ranged: bool,
    ) {
        let tab_id = self
            .parallel_downloads
            .get(&download_id)
            .map(|runtime| runtime.tab_id);
        let context = tab_id.and_then(|tab_id| self.request_context_for_tab(tab_id));
        let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
            return;
        };

        // The probe and its segments are one batch. Resume creates a new batch
        // with a fresh generation before calling this again.
        let generation = runtime.generation;
        runtime.ranged = ranged;
        runtime.total_bytes = ranges
            .last()
            .map(|range| (range.end + 1).max(0))
            .unwrap_or(runtime.total_bytes);
        runtime.segments = ranges
            .iter()
            .map(|range| ParallelSegment {
                range: *range,
                received: 0,
                complete: false,
                request: None,
            })
            .collect();
        let url = runtime.request_url.clone();

        for (index, range) in ranges.iter().enumerate() {
            let request = parallel_download::start_segment(
                &self.event_sender,
                download_id,
                generation,
                &url,
                *range,
                index as u8,
                context.clone(),
                ranged,
            );
            runtime.segments[index].request = request;
        }

        if let Some(info) = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
        {
            info.segments = ranges.len().min(u8::MAX as usize) as u8;
            info.total_bytes = runtime.total_bytes;
            info.updated_at = Self::now_unix_secs();
            let snapshot = info.clone();
            if let Some(ref storage) = self.storage {
                storage.save_download(snapshot);
            }
        }
    }

    /// Resume incomplete segments from their last flushed byte. Servers without
    /// Range support restart the whole file rather than corrupting the output.
    fn resume_segment_requests(&mut self, download_id: u32) {
        let (tab_id, url, ranged, ranges, restart_from_scratch) = {
            let Some(runtime) = self.parallel_downloads.get(&download_id) else {
                return;
            };
            // Pausing before the HEAD probe returned leaves no segments to
            // resume; restart cleanly instead of stranding the download.
            let restart_from_scratch = runtime.segments.is_empty()
                || (!runtime.ranged && runtime.segments.iter().any(|segment| segment.received > 0));
            let ranges: Vec<(u8, SegmentRange)> = runtime
                .segments
                .iter()
                .enumerate()
                .filter(|(_, segment)| !segment.complete)
                .map(|(index, segment)| {
                    (
                        index as u8,
                        SegmentRange {
                            start: segment.range.start + segment.received,
                            end: segment.range.end,
                        },
                    )
                })
                .collect();
            (
                runtime.tab_id,
                runtime.request_url.clone(),
                runtime.ranged,
                ranges,
                restart_from_scratch,
            )
        };
        if restart_from_scratch {
            if let Some(info) = self.download_snapshot(download_id) {
                self.start_native_download(info);
            }
            return;
        }

        let context = self.request_context_for_tab(tab_id);

        let generation = self.next_download_generation;
        self.next_download_generation = self.next_download_generation.wrapping_add(1);
        let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
            return;
        };
        runtime.generation = generation;
        for (index, range) in ranges {
            if range.start > range.end && range.end >= 0 {
                if let Some(segment) = runtime.segments.get_mut(index as usize) {
                    segment.complete = true;
                }
                continue;
            }
            let request = parallel_download::start_segment(
                &self.event_sender,
                download_id,
                generation,
                &url,
                range,
                index,
                context.clone(),
                ranged,
            );
            if let Some(segment) = runtime.segments.get_mut(index as usize) {
                segment.request = request;
            }
        }
    }

    /// CEF chooses the destination under `callback.cont`. Once the path is
    /// known, cancel that copy and replace it with native segmented requests.
    fn takeover_cef_download(
        &mut self,
        info: DownloadInfo,
        callback: Option<cef::DownloadItemCallback>,
    ) {
        if !parallel_download::is_segmentable_url(&info.url) {
            return;
        }
        self.superseded_cef_downloads.insert(info.id);
        if let Some(callback) = callback {
            callback.cancel();
        }
        self.download_callbacks.remove(&info.id);
        self.start_native_download(info);
    }

    fn handle_parallel_event(&mut self, event: ParallelDownloadEvent) {
        let download_id = match &event {
            ParallelDownloadEvent::ProbeResult { download_id, .. }
            | ParallelDownloadEvent::SegmentData { download_id, .. }
            | ParallelDownloadEvent::SegmentComplete { download_id, .. }
            | ParallelDownloadEvent::SegmentFailed { download_id, .. } => *download_id,
        };
        let is_active = self
            .downloads
            .iter()
            .find(|download| download.id == download_id)
            .map(|download| download.state == DownloadState::InProgress)
            .unwrap_or(false);
        if !is_active {
            return;
        }

        match event {
            ParallelDownloadEvent::ProbeResult {
                download_id,
                generation,
                status,
                supports_ranges,
                total_bytes,
                final_url,
            } => {
                let plan = {
                    let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
                        return;
                    };
                    if runtime.generation != generation {
                        return;
                    }
                    runtime.probe = None;
                    if !final_url.is_empty() {
                        runtime.request_url = final_url;
                    }
                    runtime.total_bytes = total_bytes.max(0);

                    if !(200..300).contains(&status) {
                        // Many servers reject HEAD. Fall back to one plain GET;
                        // this still goes through the native writer and controls.
                        tracing::warn!(
                            "HEAD probe returned {status} for download {download_id}; using one GET"
                        );
                        Some((vec![SegmentRange { start: 0, end: -1 }], false))
                    } else if parallel_download::should_parallelize(
                        status,
                        supports_ranges,
                        total_bytes,
                    ) {
                        Some((
                            parallel_download::plan_segments(
                                total_bytes,
                                PARALLEL_DOWNLOAD_SEGMENTS,
                            ),
                            true,
                        ))
                    } else if total_bytes > 0 {
                        Some((
                            vec![SegmentRange {
                                start: 0,
                                end: total_bytes - 1,
                            }],
                            supports_ranges,
                        ))
                    } else {
                        Some((vec![SegmentRange { start: 0, end: -1 }], false))
                    }
                };
                let Some((ranges, ranged)) = plan else {
                    tracing::warn!("Download probe failed for {download_id} (status {status})");
                    self.fail_native_download(download_id);
                    return;
                };
                let paths = self
                    .parallel_downloads
                    .get(&download_id)
                    .map(|runtime| (runtime.part_path.clone(), runtime.final_path.clone()));
                let Some((part_path, final_path)) = paths else {
                    return;
                };
                self.download_writer.start(
                    download_id,
                    part_path,
                    final_path,
                    total_bytes.max(0),
                    true,
                );
                self.start_segment_requests(download_id, ranges, ranged);
            }
            ParallelDownloadEvent::SegmentData {
                download_id,
                generation,
                segment_index,
                offset,
                bytes,
            } => {
                let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
                    return;
                };
                if runtime.generation != generation {
                    return;
                }
                let Some(segment) = runtime.segments.get_mut(segment_index as usize) else {
                    return;
                };
                let allowed = if segment.range.end >= 0 {
                    (segment.range.end + 1 - offset)
                        .max(0)
                        .min(bytes.len() as i64) as usize
                } else {
                    bytes.len()
                };
                if allowed == 0 {
                    return;
                }
                segment.received += allowed as i64;
                let chunk: Arc<[u8]> = bytes[..allowed].to_vec().into();
                self.download_writer.write(download_id, offset, chunk);
            }
            ParallelDownloadEvent::SegmentComplete {
                download_id,
                generation,
                segment_index,
                received,
                success,
            } => {
                let all_complete = {
                    let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
                        return;
                    };
                    if runtime.generation != generation {
                        return;
                    }
                    let Some(segment) = runtime.segments.get_mut(segment_index as usize) else {
                        return;
                    };
                    segment.received = segment.received.max(received);
                    segment.complete = success;
                    segment.request = None;
                    runtime.segments.iter().all(|segment| segment.complete)
                };
                let can_fallback = self
                    .parallel_downloads
                    .get(&download_id)
                    .map(|runtime| runtime.ranged && runtime.segments.len() > 1)
                    .unwrap_or(false);
                if !success && can_fallback {
                    tracing::warn!(
                        "Server ignored Range for download {download_id}; restarting as one GET"
                    );
                    self.restart_single_non_ranged(download_id);
                } else if !success {
                    self.fail_native_download(download_id);
                } else if all_complete {
                    self.finish_native_download(download_id);
                } else {
                    self.sync_native_download_info(download_id, true);
                }
            }
            ParallelDownloadEvent::SegmentFailed {
                download_id,
                generation,
                ..
            } => {
                let stale = self
                    .parallel_downloads
                    .get(&download_id)
                    .map(|runtime| runtime.generation != generation)
                    .unwrap_or(true);
                if !stale {
                    self.fail_native_download(download_id);
                }
            }
        }
    }

    fn sync_native_download_info(&mut self, download_id: u32, persist: bool) {
        let runtime = self.parallel_downloads.get(&download_id);
        let Some(info) = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
        else {
            return;
        };
        let Some(runtime) = runtime else {
            return;
        };
        let received = runtime
            .segments
            .iter()
            .map(|segment| segment.received)
            .sum::<i64>();
        let total = if runtime.total_bytes > 0 {
            runtime.total_bytes
        } else {
            received
        };
        info.received_bytes = received;
        info.total_bytes = total;
        info.segments = runtime.segments.len().min(u8::MAX as usize) as u8;
        info.percent_complete = if total > 0 {
            ((received.saturating_mul(100)) / total).clamp(0, 100) as i32
        } else {
            0
        };
        info.updated_at = Self::now_unix_secs();
        let snapshot = info.clone();
        if persist {
            if let Some(ref storage) = self.storage {
                storage.save_download(snapshot);
            }
        }
    }

    fn tick_native_downloads(&mut self, now: Instant) {
        let ids: Vec<u32> = self.parallel_downloads.keys().copied().collect();
        for download_id in ids {
            let received = self.parallel_downloads.get(&download_id).map(|runtime| {
                runtime
                    .segments
                    .iter()
                    .map(|segment| segment.received)
                    .sum::<i64>()
            });
            let Some(received) = received else {
                continue;
            };
            let (speed, last_bytes, last_speed_at) = {
                let Some(runtime) = self.parallel_downloads.get_mut(&download_id) else {
                    continue;
                };
                let elapsed = runtime.last_speed_at.elapsed().as_secs_f64();
                if elapsed <= 0.0 {
                    continue;
                }
                let speed = ((received - runtime.last_bytes).max(0) as f64 / elapsed) as i64;
                (speed, received, now)
            };
            if let Some(runtime) = self.parallel_downloads.get_mut(&download_id) {
                runtime.last_bytes = last_bytes;
                runtime.last_speed_at = last_speed_at;
            }
            self.sync_native_download_info(download_id, false);
            if let Some(info) = self
                .downloads
                .iter_mut()
                .find(|item| item.id == download_id)
            {
                info.current_speed = speed;
                let snapshot = info.clone();
                if let Some(ref storage) = self.storage {
                    storage.save_download(snapshot);
                }
            }
        }
    }

    fn pause_native_download(&mut self, download_id: u32) {
        self.cancel_native_requests(download_id);
        self.download_writer.flush(download_id);
        self.sync_native_download_info(download_id, true);
        self.set_download_state(download_id, DownloadState::Paused);
    }

    fn resume_native_download(&mut self, download_id: u32) {
        if self.parallel_downloads.contains_key(&download_id) {
            self.resume_segment_requests(download_id);
            self.set_download_state(download_id, DownloadState::InProgress);
        }
    }

    fn fail_native_download(&mut self, download_id: u32) {
        self.cancel_native_requests(download_id);
        self.download_writer.flush(download_id);
        self.sync_native_download_info(download_id, true);
        self.set_download_state(download_id, DownloadState::Interrupted);
        if let Some(info) = self.download_snapshot(download_id) {
            self.queue_download_notification(&info, DownloadNotificationKind::Failed);
        }
    }

    fn finish_native_download(&mut self, download_id: u32) {
        let total = self
            .parallel_downloads
            .remove(&download_id)
            .map(|runtime| runtime.total_bytes)
            .unwrap_or(0);
        self.cancel_native_requests(download_id);
        self.download_writer.flush(download_id);
        self.download_writer.finish(download_id);
        if let Some(info) = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
        {
            info.state = DownloadState::Completed;
            info.received_bytes = if total > 0 {
                total
            } else {
                info.received_bytes
            };
            info.total_bytes = if total > 0 { total } else { info.total_bytes };
            info.percent_complete = 100;
            info.current_speed = 0;
            info.updated_at = Self::now_unix_secs();
            let snapshot = info.clone();
            if let Some(ref storage) = self.storage {
                storage.save_download(snapshot);
            }
        }
        if let Some(info) = self.download_snapshot(download_id) {
            self.queue_download_notification(&info, DownloadNotificationKind::Completed);
        }
    }

    fn cancel_native_download(&mut self, download_id: u32) {
        self.cancel_native_requests(download_id);
        self.download_writer.cancel(download_id);
        self.parallel_downloads.remove(&download_id);
        self.set_download_state(download_id, DownloadState::Canceled);
    }

    /// Cancel live transfers and let the writer flush queued bytes before exit.
    pub fn shutdown_downloads(&mut self) {
        let ids: Vec<u32> = self.parallel_downloads.keys().copied().collect();
        for download_id in ids {
            self.cancel_native_requests(download_id);
            self.download_writer.flush(download_id);
        }
        self.download_writer.shutdown();
    }

    /// Restore previous session tabs (instant startup: only the active tab of
    /// the active workspace is live; everything else starts hibernated).
    pub fn restore_session(&mut self, restored: Vec<SessionTabSnapshot>) {
        if restored.is_empty() {
            self.create_tab(None, true);
            self.sync_session();
            return;
        }

        let workspace_rank: HashMap<u64, usize> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| (workspace.id, index))
            .collect();
        let mut ordered = restored;
        // Group by workspace first, then keep pinned tabs at the top of each group.
        ordered.sort_by_key(|snapshot| {
            (
                workspace_rank
                    .get(&snapshot.workspace_id)
                    .copied()
                    .unwrap_or(usize::MAX),
                !snapshot.is_pinned,
            )
        });

        let mut active_by_workspace: HashMap<u64, u64> = HashMap::new();
        for snapshot in ordered {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            // A stale session can reference a workspace that no longer exists.
            // Fold those tabs into the active workspace so focus_tab can never
            // activate a workspace id with no metadata behind it.
            let workspace_id = if workspace_rank.contains_key(&snapshot.workspace_id) {
                snapshot.workspace_id
            } else {
                self.active_workspace_id
            };

            let tab = Tab {
                id,
                workspace_id,
                url: snapshot.url.clone(),
                title: if snapshot.title.is_empty() {
                    "Restored Tab".to_string()
                } else {
                    snapshot.title
                },
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: snapshot.is_pinned,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            };

            self.tabs.push(tab);
            if snapshot.is_active {
                active_by_workspace.entry(workspace_id).or_insert(id);
            }
        }

        self.active_tab_by_workspace = active_by_workspace;

        let first_in_active_workspace = self
            .tabs
            .iter()
            .find(|tab| tab.workspace_id == self.active_workspace_id)
            .map(|tab| tab.id);
        let target_active = self
            .active_tab_by_workspace
            .get(&self.active_workspace_id)
            .copied()
            .or(first_in_active_workspace);

        if let Some(id) = target_active {
            self.focus_tab(id);
        } else {
            // The default workspace may have no saved tabs. Open a fresh tab
            // there instead of focusing another workspace and overriding the
            // user's startup choice.
            self.create_tab(None, true);
        }

        self.sync_session();
    }

    /// Re-open the most recently closed tab in its original position.
    /// Bound to both `Ctrl/Cmd+Shift+T` and `Ctrl/Cmd+Z`.
    pub fn reopen_closed_tab(&mut self) {
        let Some(closed) = self.recently_closed.pop() else {
            return;
        };
        let workspace_id = if self.workspaces.iter().any(|w| w.id == closed.workspace_id) {
            closed.workspace_id
        } else {
            self.active_workspace_id
        };
        let index = restore_insert_index(closed.index, self.tabs.len());
        tracing::info!(
            "Re-opening closed tab at {} in workspace {}: {}",
            index,
            workspace_id,
            closed.url
        );
        self.insert_tab_in_workspace(
            Some(closed.url),
            Some(closed.title),
            Some(index),
            true,
            closed.pinned,
            workspace_id,
        );
    }

    /// Whether there is a closed tab available to restore.
    pub fn can_reopen_closed_tab(&self) -> bool {
        !self.recently_closed.is_empty()
    }

    /// Generate session snapshot for SQLite persistence.
    pub fn session_snapshots(&self) -> Vec<SessionTabSnapshot> {
        self.tabs
            .iter()
            .map(|t| SessionTabSnapshot {
                workspace_id: t.workspace_id,
                url: t.url.clone(),
                title: t.title.clone(),
                is_active: self.active_tab_by_workspace.get(&t.workspace_id).copied() == Some(t.id),
                is_hibernated: t.state == TabState::Hibernated,
                is_pinned: t.pinned,
            })
            .collect()
    }

    fn workspace_snapshots(&self) -> Vec<Workspace> {
        if self.workspaces.is_empty() {
            return vec![Workspace::default_workspace()];
        }
        self.workspaces
            .iter()
            .map(|workspace| Workspace {
                is_active: workspace.id == self.active_workspace_id,
                ..workspace.clone()
            })
            .collect()
    }

    /// Sync open tabs and workspace metadata to SQLite.
    pub fn sync_session(&self) {
        if let Some(ref storage) = self.storage {
            storage.save_session(self.session_snapshots());
            storage.save_workspaces(self.workspace_snapshots());
        }
    }

    /// Number of CEF browsers still waiting for their `on_before_close` event.
    pub fn pending_browser_closes(&self) -> u32 {
        self.pending_browser_closes
    }

    /// Close every live browser while preserving tab metadata for session restore.
    pub fn close_all_browsers(&mut self) {
        for tab in &mut self.tabs {
            if let Some(browser) = tab.browser.take() {
                if let Some(host) = browser.host() {
                    self.pending_browser_closes += 1;
                    tracing::info!("Closing CEF browser for tab {}", tab.id);
                    host.close_browser(1);
                }
            }
        }
    }

    /// Set or update the parent window view handle (e.g. on window recreation).
    pub fn set_parent_window_view(&mut self, parent_view: *mut std::ffi::c_void) {
        self.parent_window_view = parent_view;
    }

    /// Create a new tab in the active workspace. If `make_active` is true,
    /// focuses the new tab immediately.
    pub fn create_tab(&mut self, url: Option<String>, make_active: bool) -> u64 {
        self.insert_tab(url, None, None, make_active, false)
    }

    /// Open a link in a background tab and start loading it immediately.
    /// Unlike restored tabs, the browser is live; unlike the active tab, its
    /// native child view stays hidden until the user selects it.
    pub fn create_background_tab(&mut self, url: String) -> u64 {
        let id = self.insert_tab(Some(url.clone()), None, None, false, false);
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.state = TabState::Background {
                since: Instant::now(),
            };
        }
        self.queue_browser_spawn(id, url, BrowserSpawnMode::Background);
        self.sync_session();
        id
    }

    /// Insert a tab in the active workspace.
    fn insert_tab(
        &mut self,
        url: Option<String>,
        title: Option<String>,
        index: Option<usize>,
        make_active: bool,
        pinned: bool,
    ) -> u64 {
        self.insert_tab_in_workspace(
            url,
            title,
            index,
            make_active,
            pinned,
            self.active_workspace_id,
        )
    }

    /// Insert a tab in a specific workspace with optional restored title,
    /// position, and pin state.
    fn insert_tab_in_workspace(
        &mut self,
        url: Option<String>,
        title: Option<String>,
        index: Option<usize>,
        make_active: bool,
        pinned: bool,
        workspace_id: u64,
    ) -> u64 {
        let workspace_id = if self.workspaces.iter().any(|w| w.id == workspace_id) {
            workspace_id
        } else {
            self.active_workspace_id
        };
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let target_url = url.unwrap_or_else(|| self.home_url.clone());
        let at = insertion_index(&self.tabs, workspace_id, index, pinned);

        let tab = Tab {
            id,
            workspace_id,
            url: target_url.clone(),
            title: title.unwrap_or_else(|| "New Tab".to_string()),
            is_loading: false,
            can_go_back: false,
            can_go_forward: false,
            blocked_requests: 0,
            pinned,
            state: if make_active {
                TabState::Active
            } else {
                TabState::Hibernated
            },
            browser: None,
            favicon: None,
        };

        self.tabs.insert(at, tab);

        if make_active {
            self.focus_tab(id);
        } else {
            self.sync_session();
        }

        id
    }

    /// Toggle a tab between the compact pinned section and the normal tabs of
    /// its own workspace.
    pub fn toggle_pin_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let workspace_id = self.tabs[index].workspace_id;
        let pinned = !self.tabs[index].pinned;
        let insert_at = pinned_insert_index(&self.tabs, workspace_id, Some(index));
        let mut tab = self.tabs.remove(index);
        tab.pinned = pinned;
        self.tabs.insert(insert_at, tab);
        self.sync_session();
    }

    /// Move an unpinned tab into another workspace while preserving its live
    /// CEF browser and its position in the target workspace's normal tab group.
    pub fn move_tab_to_workspace(&mut self, tab_id: u64, workspace_id: u64) {
        if !self
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            return;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return;
        };
        if self.tabs[index].pinned || self.tabs[index].workspace_id == workspace_id {
            return;
        }

        let old_workspace = self.tabs[index].workspace_id;
        let was_active = self.active_tab_id == Some(tab_id);
        let mut tab = self.tabs.remove(index);
        tab.workspace_id = workspace_id;
        let at = insertion_index(&self.tabs, workspace_id, None, false);
        self.tabs.insert(at, tab);

        if self.active_tab_by_workspace.get(&old_workspace) == Some(&tab_id) {
            self.active_tab_by_workspace.remove(&old_workspace);
            if let Some(fallback) = self
                .tabs
                .iter()
                .find(|candidate| candidate.workspace_id == old_workspace)
                .map(|candidate| candidate.id)
            {
                self.active_tab_by_workspace.insert(old_workspace, fallback);
            }
        }

        if was_active {
            self.active_workspace_id = workspace_id;
            self.active_tab_by_workspace.insert(workspace_id, tab_id);
            self.show_active_tab_view();
        }
        self.sync_session();
    }

    /// Focus an existing tab by ID. Focusing a tab in another workspace also
    /// switches the visible workspace.
    pub fn focus_tab(&mut self, id: u64) {
        if self.active_tab_id == Some(id) {
            // Already active; ensure child view is positioned
            self.show_active_tab_view();
            return;
        }

        // 1. Hide the old active tab's native view and remember its selection.
        if let Some(old_id) = self.active_tab_id {
            if let Some(old_tab) = self.tabs.iter_mut().find(|t| t.id == old_id) {
                let old_workspace = old_tab.workspace_id;
                self.active_tab_by_workspace.insert(old_workspace, old_id);
                if let Some(ref browser) = old_tab.browser {
                    if let Some(host) = browser.host() {
                        let view_handle = host.window_handle();
                        if !platform_view::cef_handle_is_null(view_handle) {
                            let view_ptr = platform_view::cef_handle_as_ptr(view_handle);
                            if let Err(e) = unsafe { platform_view::hide_view(view_ptr) } {
                                tracing::warn!("Could not hide old tab view: {e}");
                            }
                        }
                    }
                }
                old_tab.state = TabState::Background {
                    since: Instant::now(),
                };
            }
        }

        // 2. Activate target tab and its workspace.
        let target_workspace = {
            let tab = match self.tabs.iter().find(|t| t.id == id) {
                Some(tab) => tab,
                None => return,
            };
            tab.workspace_id
        };

        self.active_tab_id = Some(id);
        self.active_workspace_id = target_workspace;
        self.active_tab_by_workspace.insert(target_workspace, id);

        let (target_url, is_hibernated, has_browser) = {
            let tab = match self.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t,
                None => return,
            };
            let hib = tab.state == TabState::Hibernated;
            let has_b = tab.browser.is_some();
            tab.state = TabState::Active;
            (tab.url.clone(), hib, has_b)
        };

        if is_hibernated || !has_browser {
            // Wake or spawn CEF browser instance. The actual CEF call is
            // deferred to `tick()` so it never runs inside a winit window
            // event callback (see `pending_browser_spawns`).
            self.queue_browser_spawn(id, target_url, BrowserSpawnMode::ActiveOnly);
        } else {
            // Browser already running — make its view visible and resized
            self.show_active_tab_view();
        }

        self.sync_session();
    }

    /// Close a tab by ID.
    pub fn close_tab(&mut self, id: u64) {
        let index = match self.tabs.iter().position(|t| t.id == id) {
            Some(i) => i,
            None => return,
        };

        let workspace_id = self.tabs[index].workspace_id;
        let mut tab = self.tabs.remove(index);
        self.recently_closed.push(closed_tab_from(&tab, index));
        if self.active_tab_by_workspace.get(&tab.workspace_id) == Some(&id) {
            self.active_tab_by_workspace.remove(&tab.workspace_id);
        }
        self.pending_navigation_checks.remove(&id);
        self.requested_favicons.retain(|(tab_id, _)| *tab_id != id);
        self.pending_favicon_urls.remove(&id);
        self.resolved_favicons.remove(&id);
        self.last_favicon_urls.remove(&id);
        self.favicon_preferences.remove(&id);

        if let Some(browser) = tab.browser.take() {
            if let Some(host) = browser.host() {
                self.pending_browser_closes += 1;
                host.close_browser(1);
            }
        }

        if self.active_tab_id == Some(id) {
            self.active_tab_id = None;
            if self.tabs.is_empty() {
                // All tabs closed — open a default tab in the current workspace.
                self.create_tab(None, true);
            } else {
                // Stay in the same workspace when possible. Tabs are grouped by
                // workspace, so the global nearest index can otherwise land in
                // the next workspace after closing that workspace's last tab.
                let same_workspace: Vec<(usize, u64)> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| tab.workspace_id == workspace_id)
                    .map(|(i, tab)| (i, tab.id))
                    .collect();
                if same_workspace.is_empty() {
                    // The workspace still exists but has no tabs left; open a
                    // fresh one there instead of silently jumping workspaces.
                    self.create_tab(None, true);
                } else {
                    let new_id = same_workspace
                        .into_iter()
                        .min_by_key(|(i, _)| i.abs_diff(index))
                        .map(|(_, tab_id)| tab_id)
                        .unwrap_or_else(|| self.tabs[0].id);
                    self.focus_tab(new_id);
                }
            }
        } else {
            self.sync_session();
        }
    }

    /// Hibernate a single background tab (rule: never hibernate active_tab_id).
    pub fn hibernate_tab(&mut self, id: u64) {
        if self.active_tab_id == Some(id) {
            return;
        }

        if self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .is_none_or(|tab| tab.state == TabState::Hibernated)
        {
            return;
        }

        self.pending_navigation_checks.remove(&id);
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tracing::info!("Hibernating tab {} ({})", id, tab.title);
            if let Some(browser) = tab.browser.take() {
                if let Some(host) = browser.host() {
                    self.pending_browser_closes += 1;
                    host.close_browser(1);
                }
            }

            tab.state = TabState::Hibernated;
            tab.blocked_requests = 0;
            tab.is_loading = false;
            self.sync_session();
        }
    }

    /// Hibernate all background tabs immediately.
    pub fn hibernate_all_background(&mut self) {
        let active = self.active_tab_id;
        let to_hibernate: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| Some(t.id) != active && t.state != TabState::Hibernated)
            .map(|t| t.id)
            .collect();

        for id in to_hibernate {
            self.hibernate_tab(id);
        }
    }

    /// Name of a workspace, falling back to the default if it was removed.
    pub fn workspace_name(&self, workspace_id: u64) -> String {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.name.clone())
            .unwrap_or_else(|| Workspace::DEFAULT_NAME.to_string())
    }

    /// Keep the flat tab vector grouped by workspace and pinned-first inside
    /// each group after a workspace is deleted.
    fn regroup_tabs(&mut self) {
        let rank: HashMap<u64, usize> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| (workspace.id, index))
            .collect();
        self.tabs.sort_by_key(|tab| {
            (
                rank.get(&tab.workspace_id).copied().unwrap_or(usize::MAX),
                !tab.pinned,
            )
        });
    }

    /// Switch the visible workspace, preserving each workspace's selected tab.
    pub fn switch_workspace(&mut self, workspace_id: u64) {
        if !self
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            return;
        }
        if self.active_workspace_id == workspace_id {
            if self.active_tab_id.is_some() {
                self.show_active_tab_view();
            }
            return;
        }

        self.active_workspace_id = workspace_id;
        let target = self
            .active_tab_by_workspace
            .get(&workspace_id)
            .copied()
            .or_else(|| {
                self.tabs
                    .iter()
                    .find(|tab| tab.workspace_id == workspace_id)
                    .map(|tab| tab.id)
            });

        match target {
            Some(tab_id) => self.focus_tab(tab_id),
            None => {
                self.create_tab(None, true);
            }
        }
    }

    /// Choose the workspace that opens at startup. This does not change which
    /// workspace is visible in the current session.
    pub fn set_default_workspace(&mut self, workspace_id: u64) {
        if !self
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            return;
        }
        for workspace in &mut self.workspaces {
            workspace.is_default = workspace.id == workspace_id;
        }
        self.sync_session();
    }

    /// Choose a unique workspace name. Names double as bookmark folder names,
    /// so collisions would merge two workspaces' bookmarks in the UI.
    fn unique_workspace_name(&self, requested: &str, exclude_id: Option<u64>) -> String {
        let base = Workspace::normalize_name(requested);
        let collides = |candidate: &str| {
            self.workspaces.iter().any(|workspace| {
                Some(workspace.id) != exclude_id && workspace.name.eq_ignore_ascii_case(candidate)
            })
        };
        if !collides(&base) {
            return base;
        }

        let mut suffix = 2_u32;
        loop {
            let suffix_text = suffix.to_string();
            let room = Workspace::MAX_NAME_LEN.saturating_sub(suffix_text.len() + 1);
            let root: String = base.chars().take(room).collect();
            let candidate = format!("{root} {suffix_text}");
            if !collides(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Create a workspace and make it active.
    pub fn create_workspace(&mut self, name: String, icon: WorkspaceIcon, color: WorkspaceColor) {
        let name = self.unique_workspace_name(&name, None);

        let id = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .max()
            .unwrap_or(Workspace::DEFAULT_ID)
            + 1;
        self.workspaces.push(Workspace {
            id,
            name,
            icon,
            color,
            is_default: false,
            is_active: false,
        });
        self.switch_workspace(id);
        self.sync_session();
    }

    /// Rename, re-icon, or recolour a workspace. Renaming also updates its
    /// bookmark folder so saved pages stay grouped under the new name.
    pub fn update_workspace(
        &mut self,
        workspace_id: u64,
        name: String,
        icon: WorkspaceIcon,
        color: WorkspaceColor,
    ) {
        let name = self.unique_workspace_name(&name, Some(workspace_id));
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        let old_name = workspace.name.clone();
        let name_changed = old_name != name;
        workspace.name = name.clone();
        workspace.icon = icon;
        workspace.color = color;

        if name_changed {
            if let Some(storage) = self.storage.as_ref() {
                storage.rename_bookmark_folder(workspace_id, old_name.clone(), name.clone());
            }
            for bookmark in &mut self.bookmarks {
                if bookmark.workspace_id == workspace_id && bookmark.folder == old_name {
                    bookmark.folder = name.clone();
                }
            }
        }
        self.sync_session();
    }

    /// Delete a workspace without losing its tabs or bookmarks. Tabs move to
    /// the first remaining workspace; bookmarks move to an archived folder.
    pub fn delete_workspace(&mut self, workspace_id: u64) {
        if self.workspaces.len() <= 1
            || !self
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
        {
            return;
        }

        let target_id = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id != workspace_id)
            .map(|workspace| workspace.id)
            .unwrap_or(Workspace::DEFAULT_ID);
        let old_name = self.workspace_name(workspace_id);
        let archived = Workspace::archived_folder(&old_name);
        let default_was_deleted = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .is_some_and(|workspace| workspace.is_default);
        let active_was_deleted = self.active_workspace_id == workspace_id;
        let active_tab_in_deleted = self
            .active_tab_id
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| tab.workspace_id == workspace_id);

        for tab in &mut self.tabs {
            if tab.workspace_id == workspace_id {
                tab.workspace_id = target_id;
            }
        }
        self.active_tab_by_workspace.remove(&workspace_id);
        if active_tab_in_deleted {
            if let Some(active_id) = self.active_tab_id {
                self.active_tab_by_workspace.insert(target_id, active_id);
            }
        }
        self.workspaces
            .retain(|workspace| workspace.id != workspace_id);
        self.regroup_tabs();

        if active_was_deleted {
            self.active_workspace_id = target_id;
            if self.active_tab_id.is_some() {
                self.show_active_tab_view();
            }
        }
        if default_was_deleted {
            // Never leave the browser without a startup workspace.
            for workspace in &mut self.workspaces {
                workspace.is_default = workspace.id == target_id;
            }
        }

        if let Some(storage) = self.storage.as_ref() {
            storage.rename_bookmark_folder(workspace_id, old_name.clone(), archived.clone());
        }
        for bookmark in &mut self.bookmarks {
            if bookmark.workspace_id == workspace_id && bookmark.folder == old_name {
                bookmark.folder = archived.clone();
            }
        }
        self.sync_session();
    }

    /// Bookmark a tab in a folder named after the tab's workspace.
    pub fn add_bookmark(&mut self, tab_id: u64) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let workspace_id = tab.workspace_id;
        let folder = self.workspace_name(workspace_id);
        let url = tab.url.clone();
        let title = tab.title.clone();

        if let Some(existing) = self
            .bookmarks
            .iter_mut()
            .find(|bookmark| bookmark.workspace_id == workspace_id && bookmark.url == url)
        {
            existing.title = title.clone();
            existing.folder = folder.clone();
        } else {
            self.bookmarks.push(BookmarkItem {
                id: -1,
                workspace_id,
                url: url.clone(),
                title: title.clone(),
                folder: folder.clone(),
            });
        }

        if let Some(storage) = self.storage.as_ref() {
            storage.add_bookmark(workspace_id, url, title, folder);
        }
    }

    /// Apply edits to an existing bookmark, including moving it to a folder.
    pub fn save_bookmark(
        &mut self,
        original_workspace_id: u64,
        original_url: String,
        workspace_id: u64,
        url: String,
        title: String,
        folder: String,
    ) {
        let Some(index) = self.bookmarks.iter().position(|bookmark| {
            bookmark.workspace_id == original_workspace_id && bookmark.url == original_url
        }) else {
            return;
        };
        // The row being edited is allowed to already have this URL; only a
        // *different* row in the target workspace would violate the
        // (workspace_id, url) uniqueness constraint in SQLite.
        let moves_to_existing = self
            .bookmarks
            .iter()
            .enumerate()
            .any(|(other_index, bookmark)| {
                other_index != index && bookmark.workspace_id == workspace_id && bookmark.url == url
            });
        if moves_to_existing {
            tracing::warn!(
                "Bookmark {url} already exists in workspace {workspace_id}; keeping the original"
            );
            return;
        }

        let id = self.bookmarks[index].id;
        self.bookmarks[index] = BookmarkItem {
            id,
            workspace_id,
            url: url.clone(),
            title: title.clone(),
            folder: folder.clone(),
        };
        if let Some(storage) = self.storage.as_ref() {
            storage.save_bookmark(
                original_workspace_id,
                original_url,
                workspace_id,
                url,
                title,
                folder,
            );
        }
    }

    /// Remove a bookmark without touching the page it points to.
    pub fn delete_bookmark(&mut self, workspace_id: u64, url: String) {
        let Some(index) = self
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.workspace_id == workspace_id && bookmark.url == url)
        else {
            return;
        };
        self.bookmarks.remove(index);
        if let Some(storage) = self.storage.as_ref() {
            storage.delete_bookmark(workspace_id, url);
        }
    }

    /// Open a bookmark in whichever workspace is currently visible.
    pub fn open_bookmark(&mut self, url: String) {
        self.create_tab(Some(url), true);
    }

    /// Resize outer bounds and adjust CEF child view.
    pub fn resize(&mut self, rect: ContentRect) {
        self.content_rect = rect;
        self.resize_active_tab_view();
    }

    /// Queue a browser creation request for the next `about_to_wait` tick.
    fn queue_browser_spawn(&mut self, tab_id: u64, url: String, mode: BrowserSpawnMode) {
        if self
            .pending_browser_spawns
            .iter()
            .any(|(id, _, _)| *id == tab_id)
        {
            return;
        }
        tracing::info!("Queued {mode:?} browser spawn for tab {tab_id} ({url})");
        self.pending_browser_spawns.push_back((tab_id, url, mode));
    }

    /// Create queued browsers on the UI thread, outside winit window events.
    fn drain_browser_spawns(&mut self) {
        while let Some((tab_id, url, mode)) = self.pending_browser_spawns.pop_front() {
            let still_needed = self.tabs.iter().any(|tab| {
                tab.id == tab_id
                    && tab.browser.is_none()
                    && mode.allows(Some(tab_id) == self.active_tab_id)
            });
            if !still_needed {
                tracing::info!("Skipping stale browser spawn for tab {tab_id}");
                continue;
            }
            self.pending_navigation_checks.insert(
                tab_id,
                PendingNavigationCheck {
                    target_url: url.clone(),
                    deadline: Instant::now()
                        + next_navigation_retry_delay(0).unwrap_or(Duration::from_secs(1)),
                    attempt: 0,
                },
            );
            self.spawn_browser(tab_id, &url, mode == BrowserSpawnMode::Background);
        }
    }

    /// Reissue an initial navigation only when no commit was observed and the
    /// main frame is still blank. This avoids racing CEF's own navigation.
    fn retry_blank_initial_navigations(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .pending_navigation_checks
            .iter()
            .filter_map(|(tab_id, check)| (check.deadline <= now).then_some(*tab_id))
            .collect();

        for tab_id in due {
            let Some(check) = self.pending_navigation_checks.remove(&tab_id) else {
                continue;
            };
            let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
                continue;
            };
            if Some(tab_id) != self.active_tab_id {
                continue;
            }
            let Some(frame) = tab
                .browser
                .as_ref()
                .and_then(|browser| browser.main_frame())
            else {
                continue;
            };
            let current_url = CefString::from(&frame.url()).to_string();
            if !needs_initial_navigation(&current_url, &check.target_url) {
                tracing::info!("Initial navigation committed for tab {tab_id}: {current_url}");
                continue;
            }

            let target = CefString::from(check.target_url.as_str());
            frame.load_url(Some(&target));
            tracing::warn!(
                "Reissued initial navigation for tab {tab_id} (attempt {}, frame was '{current_url}')",
                check.attempt + 1
            );

            let next_attempt = check.attempt + 1;
            if let Some(delay) = next_navigation_retry_delay(next_attempt) {
                self.pending_navigation_checks.insert(
                    tab_id,
                    PendingNavigationCheck {
                        target_url: check.target_url,
                        deadline: now + delay,
                        attempt: next_attempt,
                    },
                );
            } else {
                tracing::error!(
                    "Initial navigation still blank for tab {tab_id} after {} retries",
                    check.attempt + 1
                );
            }
        }
    }

    /// Spawns a CEF browser for a specific tab ID. `hidden` keeps a
    /// middle-clicked background tab from flashing over the active page.
    fn spawn_browser(&self, tab_id: u64, url: &str, hidden: bool) {
        let mut client = client::create_tab_client(
            tab_id,
            self.event_sender.clone(),
            self.adblock_engine.clone(),
            Arc::clone(&self.adblock_enabled),
            self.download_preferences.clone(),
        );
        let bounds = cef::Rect {
            x: self.content_rect.x as i32,
            y: self.content_rect.y as i32,
            width: self.content_rect.width as i32,
            height: self.content_rect.height as i32,
        };
        // CEF exposes different parent fields and handle types per platform.
        // `set_as_child` writes `parent_window` on Windows/Linux and
        // `parent_view` on macOS; Linux uses an XID instead of a pointer.
        let parent_handle = platform_view::ptr_as_cef_handle(self.parent_window_view);
        let mut window_info = cef::WindowInfo::default().set_as_child(parent_handle, &bounds);
        window_info.windowless_rendering_enabled = 0;

        #[cfg(target_os = "macos")]
        {
            window_info.hidden = if hidden { 1 } else { 0 };
        }
        #[cfg(target_os = "windows")]
        {
            if hidden {
                use windows_sys::Win32::UI::WindowsAndMessaging::WS_VISIBLE;
                window_info.style &= !WS_VISIBLE;
            }
        }
        #[cfg(target_os = "linux")]
        {
            // CEF's Linux WindowInfo has no hidden flag. The BrowserCreated
            // handler unmaps the X11 child before it can become visible.
            let _ = hidden;
        }

        let cef_url = cef::CefString::from(url);
        let browser_settings = cef::BrowserSettings::default();

        let success = cef::browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&cef_url),
            Some(&browser_settings),
            None,
            None,
        );

        if success == 1 {
            tracing::info!("Spawned browser for tab {} (url: {})", tab_id, url);
        } else {
            tracing::error!("cef::browser_host_create_browser failed for tab {}", tab_id);
        }
    }

    /// Ask CEF to download favicon bitmaps. Called on the main UI thread only;
    /// the callback sends the decoded pixels back over the existing channel.
    fn request_favicon_downloads(&mut self, tab_id: u64, urls: Vec<String>) {
        let host = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.browser.as_ref())
            .and_then(|browser| browser.host());

        let Some(host) = host else {
            self.pending_favicon_urls
                .entry(tab_id)
                .or_default()
                .extend(urls);
            return;
        };

        for url in urls {
            if url.is_empty() || !self.requested_favicons.insert((tab_id, url.clone())) {
                continue;
            }

            let cef_url = cef::CefString::from(url.as_str());
            let mut callback = create_favicon_download_callback(tab_id, self.event_sender.clone());
            host.download_image(Some(&cef_url), 1, 64, 0, Some(&mut callback));
        }
    }

    /// Open or focus DevTools for a tab.
    ///
    /// CEF focuses an existing DevTools window when `show_dev_tools` is called
    /// again, so the `has_dev_tools` result is only used for logging. Returning
    /// early when it is true leaves a resized/reordered DevTools window stuck
    /// behind the main window with no way to bring it forward.
    fn show_dev_tools(&self, tab_id: u64) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) else {
            return;
        };

        let was_open = host.has_dev_tools() != 0;
        let mut window_info = cef::WindowInfo::default();
        window_info.windowless_rendering_enabled = 0;
        window_info.bounds = cef::Rect {
            x: 100,
            y: 100,
            width: 960,
            height: 720,
        };
        host.show_dev_tools(Some(&window_info), None, None, None);
        tracing::info!(
            "{} DevTools for tab {}",
            if was_open { "Focused" } else { "Opened" },
            tab_id
        );
    }

    /// Position and show the active tab's native view.
    pub fn show_active_tab_view(&self) {
        let Some(id) = self.active_tab_id else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) else {
            return;
        };
        let view_handle = host.window_handle();
        if platform_view::cef_handle_is_null(view_handle) || self.parent_window_view.is_null() {
            return;
        }
        let view_ptr = platform_view::cef_handle_as_ptr(view_handle);
        if let Err(e) = unsafe {
            platform_view::show_view(self.parent_window_view.cast(), view_ptr, self.content_rect)
        } {
            tracing::warn!("Could not show active tab view: {e}");
        }
    }

    /// Resize the active tab's native view.
    pub fn resize_active_tab_view(&self) {
        let Some(id) = self.active_tab_id else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) else {
            return;
        };
        let view_handle = host.window_handle();
        if platform_view::cef_handle_is_null(view_handle) {
            return;
        }
        let view_ptr = platform_view::cef_handle_as_ptr(view_handle);
        if let Err(e) = unsafe { platform_view::resize_view(view_ptr, self.content_rect) } {
            tracing::warn!("Could not resize active tab view: {e}");
        }
    }

    /// Drain incoming CEF callbacks and update tab states.
    pub fn pump_events(&mut self) {
        while let Ok(event) = self.event_receiver.try_recv() {
            self.handle_browser_event(event);
        }
    }

    /// Consume a Cmd/Ctrl+K request forwarded from the CEF web view.
    pub fn consume_command_palette_request(&mut self) -> bool {
        std::mem::take(&mut self.command_palette_requested)
    }

    /// Consume a Cmd/Ctrl+, request forwarded from the CEF web view.
    pub fn consume_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    /// Consume a Cmd/Ctrl+J request forwarded from the CEF web view.
    pub fn consume_downloads_request(&mut self) -> bool {
        std::mem::take(&mut self.downloads_requested)
    }

    /// Consume a Cmd/Ctrl+Shift+B request forwarded from the CEF web view.
    pub fn consume_bookmark_manager_request(&mut self) -> bool {
        std::mem::take(&mut self.bookmark_manager_requested)
    }

    /// Consume a request to reveal the downloads bubble after a new transfer.
    pub fn consume_download_bubble_request(&mut self) -> bool {
        std::mem::take(&mut self.download_bubble_requested)
    }

    /// Consume terminal download notifications for the native toast surface.
    pub fn consume_download_notifications(&mut self) -> Vec<DownloadNotification> {
        std::mem::take(&mut self.download_notifications)
    }

    fn handle_browser_event(&mut self, event: BrowserEvent) {
        match event {
            BrowserEvent::DownloadStarted(info) => {
                // A navigation that turns into a download never commits, so
                // the blank-frame watchdog must stop retrying it. Reissuing the
                // URL would start duplicate transfers and make CEF cancel the
                // second "save as" dialog.
                self.pending_navigation_checks.remove(&info.tab_id);
                self.record_download(info, None);
            }
            BrowserEvent::DownloadUpdated { mut info, callback } => {
                // CEF's first update can omit the suggested filename even when
                // `on_before_download` supplied a good one. Keep that name so
                // the native segmented writer never falls back to "download".
                if info.suggested_name.trim().is_empty() {
                    if let Some(existing) = self
                        .downloads
                        .iter()
                        .find(|download| download.id == info.id)
                    {
                        if !existing.suggested_name.trim().is_empty() {
                            info.suggested_name = existing.suggested_name.clone();
                        }
                    }
                }
                let should_take_over = info.engine == DownloadEngine::Cef
                    && !self.superseded_cef_downloads.contains(&info.id)
                    && !info.full_path.trim().is_empty()
                    && !info.url.trim().is_empty()
                    && parallel_download::is_segmentable_url(&info.url)
                    && matches!(
                        info.state,
                        DownloadState::InProgress | DownloadState::Paused
                    );
                self.record_download(info.clone(), callback.clone());
                if should_take_over {
                    tracing::info!(
                        "Taking over CEF download {} with native segmented engine",
                        info.id
                    );
                    self.takeover_cef_download(info, callback);
                }
            }
            BrowserEvent::ParallelDownload(event) => self.handle_parallel_event(event),
            BrowserEvent::BrowserCreated { tab_id, browser } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.browser = Some(browser);
                }
                if self.active_tab_id == Some(tab_id) {
                    self.show_active_tab_view();
                } else {
                    // A middle-clicked tab loads in the background. Hide its
                    // native view until `focus_tab` makes it active.
                    let view_handle = self
                        .tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .and_then(|tab| tab.browser.as_ref())
                        .and_then(|browser| browser.host())
                        .map(|host| host.window_handle());
                    if let Some(view_handle) = view_handle {
                        if !platform_view::cef_handle_is_null(view_handle) {
                            let view_ptr = platform_view::cef_handle_as_ptr(view_handle);
                            if let Err(e) = unsafe { platform_view::hide_view(view_ptr) } {
                                tracing::warn!("Could not hide background tab view: {e}");
                            }
                        }
                    }
                }
                if let Some(urls) = self.pending_favicon_urls.remove(&tab_id) {
                    if !urls.is_empty() {
                        self.request_favicon_downloads(tab_id, urls);
                        return;
                    }
                }

                // Do not download favicons here. `host.download_image()` can
                // touch a speculative frame before CEF installs its
                // URLLoaderFactoryBundle, which intermittently breaks the
                // initial navigation. `AddressChange` requests the fallback
                // icon only after a real URL has committed.
            }
            BrowserEvent::TitleChange { tab_id, title } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.title = title.clone();
                    if let Some(ref storage) = self.storage {
                        storage.record_visit(tab.url.clone(), title);
                    }
                }
                self.sync_session();
            }
            BrowserEvent::AddressChange { tab_id, url } => {
                let committed_initial = !is_blank_navigation_url(&url)
                    && self.pending_navigation_checks.remove(&tab_id).is_some();
                if committed_initial {
                    tracing::info!("Initial navigation committed for tab {tab_id}: {url}");
                }
                let mut fallback = None;
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if tab.url != url {
                        // A new site needs a fresh favicon; stale pixels must not
                        // linger in the tab strip.
                        tab.favicon = None;
                        self.requested_favicons.retain(|(id, _)| *id != tab_id);
                        self.pending_favicon_urls.remove(&tab_id);
                        self.resolved_favicons.remove(&tab_id);
                        self.last_favicon_urls.remove(&tab_id);
                        self.favicon_preferences.remove(&tab_id);
                    }
                    fallback = fallback_favicon_url(&url);
                    tab.url = url.clone();
                    if let Some(ref storage) = self.storage {
                        storage.record_visit(url, tab.title.clone());
                    }
                }
                self.sync_session();
                if let Some(fallback) = fallback {
                    self.favicon_preferences.entry(tab_id).or_default();
                    self.request_favicon_downloads(tab_id, vec![fallback]);
                }
            }
            BrowserEvent::LoadingStateChange {
                tab_id,
                is_loading,
                can_go_back,
                can_go_forward,
            } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.is_loading = is_loading;
                    tab.can_go_back = can_go_back;
                    tab.can_go_forward = can_go_forward;
                }
            }
            BrowserEvent::LoadingProgressChange {
                tab_id: _,
                progress: _,
            } => {}
            BrowserEvent::RequestBlocked { tab_id } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.blocked_requests += 1;
                }
            }
            BrowserEvent::BrowserClosed { tab_id } => {
                self.pending_navigation_checks.remove(&tab_id);
                // CEF browser is gone (user tab close, hibernation, or app shutdown).
                // Never remove the tab metadata here — only release the handle.
                tracing::info!(
                    "Received BrowserClosed for tab {} (pending before: {})",
                    tab_id,
                    self.pending_browser_closes
                );
                self.pending_browser_closes = self.pending_browser_closes.saturating_sub(1);
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.browser = None;
                }
            }
            BrowserEvent::BrowserShortcut { tab_id, shortcut } => match shortcut {
                BrowserShortcut::NewTab => {
                    self.create_tab(None, true);
                }
                BrowserShortcut::CloseTab => {
                    self.close_tab(tab_id);
                }
                BrowserShortcut::ReopenClosedTab => {
                    self.reopen_closed_tab();
                }
                BrowserShortcut::OpenCommandPalette => {
                    self.command_palette_requested = true;
                }
                BrowserShortcut::OpenDevTools => {
                    self.show_dev_tools(tab_id);
                }
                BrowserShortcut::OpenSettings => {
                    self.settings_requested = true;
                }
                BrowserShortcut::OpenDownloads => {
                    // The shell owns the shared overlay and downloads bubble.
                    // This flag makes all shortcut sources converge on one path.
                    self.downloads_requested = true;
                }
                BrowserShortcut::OpenBookmarkManager => {
                    self.bookmark_manager_requested = true;
                }
                BrowserShortcut::Quit => {
                    // The shell exits the event loop from its own shortcut
                    // path; TabManager must never call process::exit().
                }
            },
            BrowserEvent::ViewSource { url, .. } => {
                if url.is_empty() {
                    return;
                }
                let source_url = if url.starts_with("view-source:") {
                    url
                } else {
                    format!("view-source:{}", url)
                };
                let existing = self.tabs.iter().find(|t| t.url == source_url).map(|t| t.id);
                if let Some(id) = existing {
                    self.focus_tab(id);
                } else {
                    self.create_tab(Some(source_url), true);
                }
            }
            BrowserEvent::FaviconUrlChanged { tab_id, urls } => {
                if self.last_favicon_urls.get(&tab_id) == Some(&urls) {
                    return;
                }
                let preferred = preferred_favicon_url(&urls);
                let mut candidates: Vec<String> = preferred.clone().into_iter().collect();
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(fallback) = fallback_favicon_url(&tab.url) {
                        if !candidates.contains(&fallback) {
                            candidates.push(fallback);
                        }
                    }
                }
                self.favicon_preferences
                    .insert(tab_id, preferred.unwrap_or_default());
                self.last_favicon_urls.insert(tab_id, urls);
                self.requested_favicons.retain(|(id, _)| *id != tab_id);
                self.resolved_favicons.remove(&tab_id);
                self.request_favicon_downloads(tab_id, candidates);
            }
            BrowserEvent::FaviconLoaded {
                tab_id,
                url,
                rgba,
                width,
                height,
            } => {
                if !self.requested_favicons.contains(&(tab_id, url.clone())) {
                    return;
                }

                let preferred = self
                    .favicon_preferences
                    .get(&tab_id)
                    .cloned()
                    .unwrap_or_default();
                if !preferred.is_empty() {
                    let current_is_preferred = self
                        .tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .and_then(|t| t.favicon.as_ref())
                        .map(|favicon| favicon.url == preferred)
                        .unwrap_or(false);
                    if current_is_preferred && url != preferred {
                        return;
                    }
                    if url != preferred && !self.resolved_favicons.insert(tab_id) {
                        return;
                    }
                    if url == preferred {
                        self.resolved_favicons.insert(tab_id);
                    }
                } else if !self.resolved_favicons.insert(tab_id) {
                    return;
                }

                tracing::info!(
                    "Using favicon for tab {} from {} ({}x{})",
                    tab_id,
                    url,
                    width,
                    height
                );
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.favicon = Some(Favicon {
                        url,
                        rgba,
                        width,
                        height,
                    });
                }
            }
            BrowserEvent::NewWindow { url } => {
                self.create_tab(Some(url), true);
            }
            BrowserEvent::OpenUrlFromTab { url, background } => {
                if url.is_empty() {
                    return;
                }
                if background {
                    self.create_background_tab(url);
                } else {
                    self.create_tab(Some(url), true);
                }
            }
        }
    }

    /// Dispatch user actions from egui chrome or keyboard shortcuts.
    pub fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::NewTab { url } => {
                self.create_tab(url, true);
            }
            UiAction::ReopenClosedTab => {
                self.reopen_closed_tab();
            }
            UiAction::CloseTab { tab_id } => {
                self.close_tab(tab_id);
            }
            UiAction::CloseOtherTabs { tab_id } => {
                let Some(workspace_id) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| tab.workspace_id)
                else {
                    return;
                };
                let ids: Vec<u64> = self
                    .tabs
                    .iter()
                    .rev()
                    .filter(|tab| tab.workspace_id == workspace_id && tab.id != tab_id)
                    .map(|tab| tab.id)
                    .collect();
                for id in ids {
                    self.close_tab(id);
                }
            }
            UiAction::CloseTabsToRight { tab_id } => {
                let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                    return;
                };
                let workspace_id = self.tabs[index].workspace_id;
                let ids: Vec<u64> = self.tabs[index + 1..]
                    .iter()
                    .rev()
                    .filter(|tab| tab.workspace_id == workspace_id)
                    .map(|tab| tab.id)
                    .collect();
                for id in ids {
                    self.close_tab(id);
                }
            }
            UiAction::DuplicateTab { tab_id } => {
                if let Some((index, tab)) = self
                    .tabs
                    .iter()
                    .enumerate()
                    .find(|(_, tab)| tab.id == tab_id)
                {
                    let url = tab.url.clone();
                    let title = tab.title.clone();
                    let pinned = tab.pinned;
                    let workspace_id = tab.workspace_id;
                    self.insert_tab_in_workspace(
                        Some(url),
                        Some(title),
                        Some(index + 1),
                        true,
                        pinned,
                        workspace_id,
                    );
                }
            }
            UiAction::TogglePinTab { tab_id } => {
                self.toggle_pin_tab(tab_id);
            }
            UiAction::SelectTab { tab_id } => {
                self.focus_tab(tab_id);
            }
            UiAction::Navigate { tab_id, url } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.url = url.clone();
                    if let Some(ref browser) = tab.browser {
                        if let Some(frame) = browser.main_frame() {
                            let cef_url = cef::CefString::from(url.as_str());
                            frame.load_url(Some(&cef_url));
                        }
                    } else {
                        // If tab was hibernated, waking it triggers navigation to tab.url
                        self.focus_tab(tab_id);
                    }
                }
            }
            UiAction::GoBack { tab_id } => {
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(ref browser) = tab.browser {
                        browser.go_back();
                    }
                }
            }
            UiAction::GoForward { tab_id } => {
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(ref browser) = tab.browser {
                        browser.go_forward();
                    }
                }
            }
            UiAction::Reload {
                tab_id,
                ignore_cache,
            } => {
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(ref browser) = tab.browser {
                        if ignore_cache {
                            browser.reload_ignore_cache();
                        } else {
                            browser.reload();
                        }
                    }
                }
            }
            UiAction::Stop { tab_id } => {
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(ref browser) = tab.browser {
                        browser.stop_load();
                    }
                }
            }
            UiAction::HibernateAllBackground => {
                self.hibernate_all_background();
            }
            UiAction::OpenDevTools { tab_id } => {
                self.show_dev_tools(tab_id);
            }
            UiAction::OpenDownloads => {
                // The shell opens the native downloads bubble; TabManager only
                // owns the download runtime state.
            }
            UiAction::PauseDownload { download_id } => {
                if self.parallel_downloads.contains_key(&download_id) {
                    self.pause_native_download(download_id);
                } else {
                    if let Some(callback) = self.download_callbacks.get(&download_id) {
                        callback.pause();
                    }
                    self.set_download_state(download_id, DownloadState::Paused);
                }
            }
            UiAction::ResumeDownload { download_id } => {
                if self.parallel_downloads.contains_key(&download_id) {
                    self.resume_native_download(download_id);
                } else {
                    if let Some(callback) = self.download_callbacks.get(&download_id) {
                        callback.resume();
                    }
                    self.set_download_state(download_id, DownloadState::InProgress);
                }
            }
            UiAction::CancelDownload { download_id } => {
                if self.parallel_downloads.contains_key(&download_id) {
                    self.cancel_native_download(download_id);
                } else {
                    if let Some(callback) = self.download_callbacks.get(&download_id) {
                        callback.cancel();
                    }
                    self.download_callbacks.remove(&download_id);
                    self.set_download_state(download_id, DownloadState::Canceled);
                }
            }
            UiAction::RemoveDownload { download_id } => {
                self.cancel_native_requests(download_id);
                self.download_writer.cancel(download_id);
                self.parallel_downloads.remove(&download_id);
                self.download_callbacks.remove(&download_id);
                // Keep the superseded id for the whole session: a late CEF
                // cancellation callback must not resurrect a removed download.
                self.downloads.retain(|download| download.id != download_id);
                if let Some(ref storage) = self.storage {
                    storage.remove_download(download_id);
                }
            }
            UiAction::ClearCompletedDownloads => {
                let finished: Vec<u32> = self
                    .downloads
                    .iter()
                    .filter(|download| download.is_finished())
                    .map(|download| download.id)
                    .collect();
                for id in finished {
                    self.cancel_native_requests(id);
                    self.download_writer.cancel(id);
                    self.parallel_downloads.remove(&id);
                    self.download_callbacks.remove(&id);
                }
                self.downloads.retain(|download| !download.is_finished());
                if let Some(ref storage) = self.storage {
                    storage.clear_completed_downloads();
                }
            }
            UiAction::RetryDownload { download_id } => {
                if let Some(info) = self.download_snapshot(download_id) {
                    if info.engine == DownloadEngine::Parallel {
                        self.start_native_download(info);
                    } else {
                        self.create_tab(Some(info.url), true);
                    }
                }
            }
            UiAction::OpenDownloadFile { .. } | UiAction::RevealDownload { .. } => {
                // Handled by the shell, which can launch the OS file manager
                // without blocking CEF callback threads.
            }
            UiAction::SwitchWorkspace { workspace_id } => {
                self.switch_workspace(workspace_id);
            }
            UiAction::MoveTabToWorkspace {
                tab_id,
                workspace_id,
            } => {
                self.move_tab_to_workspace(tab_id, workspace_id);
            }
            UiAction::CreateWorkspace { name, icon, color } => {
                self.create_workspace(name, icon, color);
            }
            UiAction::UpdateWorkspace {
                workspace_id,
                name,
                icon,
                color,
            } => {
                self.update_workspace(workspace_id, name, icon, color);
            }
            UiAction::DeleteWorkspace { workspace_id } => {
                self.delete_workspace(workspace_id);
            }
            UiAction::SetDefaultWorkspace { workspace_id } => {
                self.set_default_workspace(workspace_id);
            }
            UiAction::AddBookmark { tab_id } => {
                self.add_bookmark(tab_id);
            }
            UiAction::OpenBookmark { url } => {
                self.open_bookmark(url);
            }
            UiAction::SaveBookmark {
                original_workspace_id,
                original_url,
                workspace_id,
                url,
                title,
                folder,
            } => {
                self.save_bookmark(
                    original_workspace_id,
                    original_url,
                    workspace_id,
                    url,
                    title,
                    folder,
                );
            }
            UiAction::DeleteBookmark { workspace_id, url } => {
                self.delete_bookmark(workspace_id, url);
            }
            UiAction::ApplyPreferences(preferences) => {
                self.apply_preferences(&preferences);
            }
            // The winit handler persists the live rail toggle; tab state needs
            // no change for a chrome width update.
            UiAction::SetRailExpanded(_) => {}
        }
    }

    /// Watchdog tick: checks RAM pressure and tab idle durations.
    pub fn tick(&mut self) {
        let now = Instant::now();

        // Create browsers only from `about_to_wait`, never from a window event.
        self.drain_browser_spawns();
        self.retry_blank_initial_navigations(now);

        // A part file can vanish before the writer renames it (for example if
        // a temp directory is cleaned mid-transfer). Never leave the row marked
        // Completed in that case.
        for (download_id, ok) in self.download_writer.take_finalized() {
            if !ok {
                tracing::error!(
                    "Native download {download_id} could not be finalized; marking interrupted"
                );
                if let Some(info) = self
                    .downloads
                    .iter_mut()
                    .find(|item| item.id == download_id)
                {
                    if info.state == DownloadState::Completed {
                        info.state = DownloadState::Interrupted;
                        info.current_speed = 0;
                        info.updated_at = Self::now_unix_secs();
                        if let Some(ref storage) = self.storage {
                            storage.save_download(info.clone());
                        }
                    }
                }
            }
        }

        // 1. Refresh RAM every RAM_CHECK_INTERVAL (5s)
        if now.duration_since(self.last_ram_check) >= RAM_CHECK_INTERVAL {
            self.last_ram_check = now;
            self.sys.refresh_memory();
            self.ram_used_bytes = self.sys.used_memory();
            self.ram_total_bytes = self.sys.total_memory();

            // Refresh current process RAM
            if let Ok(pid) = sysinfo::get_current_pid() {
                self.sys
                    .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
                if let Some(proc) = self.sys.process(pid) {
                    self.ram_process_mb = proc.memory() / (1024 * 1024);
                }
            }
        }

        // 2. Check tab timeouts every TAB_WATCHDOG_TICK_INTERVAL (1s)
        if now.duration_since(self.last_tick) < TAB_WATCHDOG_TICK_INTERVAL {
            return;
        }
        self.last_tick = now;

        // Native transfer speeds are sampled once per watchdog tick and then
        // mirrored into the downloads bubble by `update_chrome_state`.
        self.tick_native_downloads(now);

        let ram_ratio = ram_pressure(self.ram_used_bytes, self.ram_total_bytes);

        // Critical memory pressure: hibernate ALL background tabs immediately
        if is_critical_pressure(ram_ratio) {
            tracing::warn!(
                "Critical RAM pressure ({:.1}%). Hibernating all background tabs.",
                ram_ratio * 100.0
            );
            self.hibernate_all_background();
            return;
        }

        // High memory pressure: shrink timeout to 30s
        let idle_timeout = idle_timeout_for_pressure(ram_ratio, self.idle_timeout);

        let active = self.active_tab_id;
        let expired_ids: Vec<u64> = self
            .tabs
            .iter()
            .filter_map(|t| {
                if should_hibernate_tab(&t.state, Some(t.id) == active, now, idle_timeout) {
                    Some(t.id)
                } else {
                    None
                }
            })
            .collect();

        for id in expired_ids {
            tracing::info!(
                "Tab {} idle timeout reached ({:?}); hibernating.",
                id,
                idle_timeout
            );
            self.hibernate_tab(id);
        }
    }

    /// Sync internal tab metadata into egui ChromeState for rendering.
    pub fn update_chrome_state(&self, state: &mut ChromeState) {
        state.tabs = self
            .tabs
            .iter()
            .map(|t| TabInfo {
                id: t.id,
                workspace_id: t.workspace_id,
                workspace_name: self.workspace_name(t.workspace_id),
                url: t.url.clone(),
                title: t.title.clone(),
                is_loading: t.is_loading,
                display_state: match t.state {
                    TabState::Active => TabDisplayState::Active,
                    TabState::Background { .. } => TabDisplayState::Background,
                    TabState::Hibernated => TabDisplayState::Hibernated,
                },
                blocked_requests: t.blocked_requests,
                can_go_back: t.can_go_back,
                can_go_forward: t.can_go_forward,
                is_pinned: t.pinned,
                favicon: t.favicon.clone(),
            })
            .collect();

        state.active_tab_id = self.active_tab_id;
        state.workspaces = self
            .workspaces
            .iter()
            .map(|workspace| WorkspaceInfo {
                id: workspace.id,
                name: workspace.name.clone(),
                icon: workspace.icon,
                color: workspace.color,
                is_default: workspace.is_default,
                is_active: workspace.id == self.active_workspace_id,
            })
            .collect();
        state.active_workspace_id = self.active_workspace_id;
        state.downloads = self.downloads.clone();
        state.bookmarks = self
            .bookmarks
            .iter()
            .map(|bookmark| BookmarkInfo {
                id: bookmark.id,
                workspace_id: bookmark.workspace_id,
                url: bookmark.url.clone(),
                title: bookmark.title.clone(),
                folder: bookmark.folder.clone(),
            })
            .collect();
        state.sync_workspace_draft();
        state.ram_mb = self.ram_process_mb;
        state.can_reopen_tab = self.can_reopen_closed_tab();

        if let Some(id) = self.active_tab_id {
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                state.is_loading = tab.is_loading;
                // Never clobber text while the user is editing. As soon as the
                // omnibox loses focus, settle back to the active tab's real URL.
                if should_sync_omnibox(
                    state.omnibox_focused,
                    state.last_synced_tab_id,
                    &state.last_synced_url,
                    Some(id),
                    &tab.url,
                    &state.omnibox_text,
                ) {
                    state.omnibox_text = tab.url.clone();
                }
                state.last_synced_tab_id = Some(id);
                state.last_synced_url = tab.url.clone();
            }
        } else {
            state.is_loading = false;
            state.omnibox_text.clear();
            state.last_synced_tab_id = None;
            state.last_synced_url.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        closed_tab_from, fallback_favicon_url, idle_timeout_for_pressure, insertion_index,
        is_blank_navigation_url, is_critical_pressure, needs_initial_navigation,
        next_navigation_retry_delay, normalize_workspace_selection, pinned_insert_index,
        preferred_favicon_url, ram_pressure, restore_insert_index, should_hibernate_tab,
        should_sync_omnibox, BrowserSpawnMode, Tab, TabManager, TabState,
    };
    use crate::config::{
        BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon, WorkspaceStartupBehavior,
        TAB_IDLE_TIMEOUT_NORMAL, TAB_IDLE_TIMEOUT_PRESSURE,
    };
    use crate::events::{DownloadEngine, DownloadInfo, DownloadNotificationKind, DownloadState};
    use crate::storage::db::{BookmarkItem, SessionTabSnapshot};
    use crate::ui::platform_view::ContentRect;
    use std::time::{Duration, Instant};

    fn test_download_info_with_id(id: u32, state: DownloadState) -> DownloadInfo {
        DownloadInfo {
            id,
            tab_id: 1,
            url: "https://example.com/file.zip".to_string(),
            suggested_name: "file.zip".to_string(),
            full_path: "/Downloads/file.zip".to_string(),
            received_bytes: 10,
            total_bytes: 10,
            percent_complete: 100,
            current_speed: 0,
            state,
            engine: DownloadEngine::Cef,
            segments: 0,
            temp_path: String::new(),
            started_at: 1,
            updated_at: 2,
        }
    }

    fn test_download_info(state: DownloadState) -> DownloadInfo {
        test_download_info_with_id(7, state)
    }

    #[test]
    fn terminal_downloads_queue_native_notifications_once() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );

        manager.record_download(test_download_info(DownloadState::InProgress), None);
        assert!(manager.consume_download_notifications().is_empty());

        manager.record_download(test_download_info(DownloadState::Completed), None);
        let notifications = manager.consume_download_notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].download_id, 7);
        assert_eq!(notifications[0].file_name, "file.zip");
        assert_eq!(notifications[0].kind, DownloadNotificationKind::Completed);
        assert!(manager.consume_download_notifications().is_empty());

        manager.record_download(test_download_info(DownloadState::Interrupted), None);
        let notifications = manager.consume_download_notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].kind, DownloadNotificationKind::Failed);
    }

    #[test]
    fn first_observed_terminal_download_still_notifies() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );

        manager.record_download(
            test_download_info_with_id(8, DownloadState::Completed),
            None,
        );
        let notifications = manager.consume_download_notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].download_id, 8);
        assert_eq!(notifications[0].kind, DownloadNotificationKind::Completed);
    }

    #[test]
    fn background_browser_spawns_run_while_another_tab_is_active() {
        assert!(BrowserSpawnMode::Background.allows(false));
        assert!(BrowserSpawnMode::Background.allows(true));
        assert!(!BrowserSpawnMode::ActiveOnly.allows(false));
        assert!(BrowserSpawnMode::ActiveOnly.allows(true));
    }

    #[test]
    fn ram_pressure_handles_normal_and_zero_total_memory() {
        assert_eq!(ram_pressure(50, 100), 0.5);
        assert_eq!(ram_pressure(0, 100), 0.0);
        assert_eq!(ram_pressure(10, 0), 0.0);
        assert_eq!(ram_pressure(120, 100), 1.2);
    }

    #[test]
    fn memory_pressure_thresholds_use_exact_boundaries() {
        assert!(!is_critical_pressure(0.899_999));
        assert!(is_critical_pressure(0.9));
        assert!(is_critical_pressure(1.0));

        assert_eq!(
            idle_timeout_for_pressure(0.0, TAB_IDLE_TIMEOUT_NORMAL),
            TAB_IDLE_TIMEOUT_NORMAL
        );
        assert_eq!(
            idle_timeout_for_pressure(0.799_999, TAB_IDLE_TIMEOUT_NORMAL),
            TAB_IDLE_TIMEOUT_NORMAL
        );
        assert_eq!(
            idle_timeout_for_pressure(0.8, TAB_IDLE_TIMEOUT_NORMAL),
            TAB_IDLE_TIMEOUT_PRESSURE
        );
        assert_eq!(
            idle_timeout_for_pressure(0.95, TAB_IDLE_TIMEOUT_NORMAL),
            TAB_IDLE_TIMEOUT_PRESSURE
        );
        assert_ne!(TAB_IDLE_TIMEOUT_NORMAL, Duration::from_secs(30));

        // A shorter user timeout is never lengthened by RAM pressure.
        let short = Duration::from_secs(20);
        assert_eq!(idle_timeout_for_pressure(0.95, short), short);
        assert_eq!(idle_timeout_for_pressure(0.1, short), short);
    }

    #[test]
    fn omnibox_sync_respects_focus_and_identity_changes() {
        assert!(!should_sync_omnibox(
            true,
            Some(1),
            "https://old.example",
            Some(1),
            "https://new.example",
            "typing"
        ));
        assert!(!should_sync_omnibox(
            false,
            Some(1),
            "https://example.com",
            Some(1),
            "https://example.com",
            "https://example.com"
        ));
        assert!(should_sync_omnibox(
            false,
            Some(1),
            "https://old.example",
            Some(2),
            "https://new.example",
            "https://old.example"
        ));
        assert!(should_sync_omnibox(
            false,
            Some(1),
            "https://old.example",
            Some(1),
            "https://new.example",
            "https://old.example"
        ));
        assert!(should_sync_omnibox(
            false,
            Some(1),
            "https://example.com",
            Some(1),
            "https://example.com",
            "https://typed.example"
        ));
        assert!(should_sync_omnibox(
            false,
            Some(1),
            "https://example.com",
            None,
            "",
            ""
        ));
    }

    #[test]
    fn hibernation_decision_respects_active_state_and_idle_boundary() {
        let now = Instant::now();
        let timeout = Duration::from_secs(30);

        assert!(!should_hibernate_tab(
            &TabState::Active,
            false,
            now,
            timeout
        ));
        assert!(!should_hibernate_tab(
            &TabState::Hibernated,
            false,
            now,
            timeout
        ));

        let fresh = TabState::Background {
            since: now - Duration::from_secs(10),
        };
        assert!(!should_hibernate_tab(&fresh, false, now, timeout));

        let expired = TabState::Background {
            since: now - Duration::from_secs(30),
        };
        assert!(should_hibernate_tab(&expired, false, now, timeout));
        assert!(!should_hibernate_tab(&expired, true, now, timeout));

        let long_idle = TabState::Background {
            since: now - TAB_IDLE_TIMEOUT_NORMAL,
        };
        assert!(!should_hibernate_tab(
            &long_idle,
            false,
            now,
            TAB_IDLE_TIMEOUT_NORMAL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn fallback_favicon_url_only_uses_http_origins() {
        assert_eq!(
            fallback_favicon_url("https://www.youtube.com/watch?v=x"),
            Some("https://www.youtube.com/favicon.ico".to_string())
        );
        assert_eq!(
            fallback_favicon_url("http://example.com:8080/path?q=1"),
            Some("http://example.com:8080/favicon.ico".to_string())
        );
        assert_eq!(
            fallback_favicon_url("view-source:https://example.com/"),
            Some("https://example.com/favicon.ico".to_string())
        );
        assert_eq!(fallback_favicon_url("ftp://example.com"), None);
        assert_eq!(fallback_favicon_url("file:///tmp/page.html"), None);
        assert_eq!(fallback_favicon_url("not a url"), None);
    }

    #[test]
    fn blank_navigation_urls_are_detected_case_insensitively() {
        assert!(is_blank_navigation_url(""));
        assert!(is_blank_navigation_url("   "));
        assert!(is_blank_navigation_url("about:blank"));
        assert!(is_blank_navigation_url(" ABOUT:BLANK "));
        assert!(!is_blank_navigation_url("https://example.com"));
    }

    #[test]
    fn navigation_retry_delays_are_finite_and_increase() {
        assert_eq!(
            next_navigation_retry_delay(0),
            Some(Duration::from_millis(750))
        );
        assert_eq!(
            next_navigation_retry_delay(1),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            next_navigation_retry_delay(2),
            Some(Duration::from_millis(3_000))
        );
        assert_eq!(next_navigation_retry_delay(3), None);
    }

    #[test]
    fn initial_navigation_is_reissued_only_for_blank_frames() {
        assert!(needs_initial_navigation("", "https://example.com"));
        assert!(needs_initial_navigation("   ", "https://example.com"));
        assert!(needs_initial_navigation(
            "about:blank",
            "https://example.com"
        ));
        assert!(needs_initial_navigation(
            "ABOUT:BLANK",
            "https://example.com"
        ));
        assert!(!needs_initial_navigation(
            "https://example.com",
            "https://example.com"
        ));
        assert!(!needs_initial_navigation("about:blank", ""));
    }

    #[test]
    fn restore_insert_index_clamps_to_current_tab_count() {
        assert_eq!(restore_insert_index(0, 0), 0);
        assert_eq!(restore_insert_index(2, 5), 2);
        assert_eq!(restore_insert_index(5, 5), 5);
        assert_eq!(restore_insert_index(99, 3), 3);
    }

    #[test]
    fn closed_tab_captures_url_title_and_original_position() {
        let tab = Tab {
            id: 7,
            workspace_id: 1,
            url: "https://example.com/article".to_string(),
            title: "Example Article".to_string(),
            is_loading: false,
            can_go_back: true,
            can_go_forward: false,
            blocked_requests: 12,
            pinned: true,
            state: TabState::Background {
                since: Instant::now(),
            },
            browser: None,
            favicon: None,
        };

        let closed = closed_tab_from(&tab, 3);
        assert_eq!(closed.url, tab.url);
        assert_eq!(closed.title, tab.title);
        assert_eq!(closed.index, 3);
        assert!(closed.pinned);
    }

    #[test]
    fn pinned_insertion_indexes_preserve_section_order() {
        let tabs = vec![
            Tab {
                id: 1,
                workspace_id: 1,
                url: "https://pinned.example".to_string(),
                title: "Pinned".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: true,
                state: TabState::Active,
                browser: None,
                favicon: None,
            },
            Tab {
                id: 2,
                workspace_id: 1,
                url: "https://pinned-two.example".to_string(),
                title: "Pinned Two".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: true,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
            Tab {
                id: 3,
                workspace_id: 1,
                url: "https://regular.example".to_string(),
                title: "Regular".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: false,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
        ];

        assert_eq!(pinned_insert_index(&tabs, 1, None), 2);
        assert_eq!(pinned_insert_index(&tabs, 1, Some(0)), 1);
        assert_eq!(insertion_index(&tabs, 1, None, true), 2);
        assert_eq!(insertion_index(&tabs, 1, Some(99), true), 2);
        assert_eq!(insertion_index(&tabs, 1, None, false), 3);
        assert_eq!(insertion_index(&tabs, 1, Some(0), false), 2);
    }

    #[test]
    fn insertion_indexes_respect_workspace_groups() {
        let tabs = vec![
            Tab {
                id: 1,
                workspace_id: 1,
                url: "https://one.example".to_string(),
                title: "One".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: true,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
            Tab {
                id: 2,
                workspace_id: 1,
                url: "https://two.example".to_string(),
                title: "Two".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: false,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
            Tab {
                id: 3,
                workspace_id: 2,
                url: "https://three.example".to_string(),
                title: "Three".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: true,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
            Tab {
                id: 4,
                workspace_id: 2,
                url: "https://four.example".to_string(),
                title: "Four".to_string(),
                is_loading: false,
                can_go_back: false,
                can_go_forward: false,
                blocked_requests: 0,
                pinned: false,
                state: TabState::Hibernated,
                browser: None,
                favicon: None,
            },
        ];

        assert_eq!(pinned_insert_index(&tabs, 2, None), 1);
        assert_eq!(insertion_index(&tabs, 2, None, true), 3);
        assert_eq!(insertion_index(&tabs, 2, None, false), 4);
        assert_eq!(insertion_index(&tabs, 1, None, true), 1);
        assert_eq!(insertion_index(&tabs, 1, None, false), 2);
    }

    #[test]
    fn workspaces_own_tabs_and_bookmark_folders() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );

        manager.create_tab(Some("https://personal.example".to_string()), true);
        manager.create_workspace(
            "Work".to_string(),
            WorkspaceIcon::Code,
            WorkspaceColor::Green,
        );

        assert_eq!(manager.active_workspace_id, 2);
        let work_tab_id = manager
            .active_tab_id
            .expect("new workspace has an active tab");
        assert_eq!(
            manager
                .tabs
                .iter()
                .find(|tab| tab.id == work_tab_id)
                .map(|tab| tab.workspace_id),
            Some(2)
        );

        manager.add_bookmark(work_tab_id);
        assert_eq!(manager.bookmarks[0].folder, "Work");

        manager.update_workspace(
            2,
            "Studio".to_string(),
            WorkspaceIcon::Palette,
            WorkspaceColor::Purple,
        );
        assert_eq!(manager.bookmarks[0].folder, "Studio");

        manager.switch_workspace(1);
        assert_eq!(manager.active_workspace_id, 1);

        manager.delete_workspace(2);
        assert!(manager.workspaces.iter().all(|workspace| workspace.id != 2));
        assert_eq!(
            manager
                .tabs
                .iter()
                .filter(|tab| tab.workspace_id == 1)
                .count(),
            manager.tabs.len()
        );
        assert_eq!(manager.bookmarks[0].folder, "Archived Studio");

        let original_url = manager.bookmarks[0].url.clone();
        manager.save_bookmark(
            2,
            original_url,
            1,
            "https://edited.example".to_string(),
            "Edited title".to_string(),
            "Personal".to_string(),
        );
        assert_eq!(manager.bookmarks[0].workspace_id, 1);
        assert_eq!(manager.bookmarks[0].folder, "Personal");
        assert_eq!(manager.bookmarks[0].title, "Edited title");
        assert_eq!(manager.bookmarks[0].url, "https://edited.example");

        manager.delete_bookmark(1, "https://edited.example".to_string());
        assert!(manager.bookmarks.is_empty());
    }

    #[test]
    fn moving_a_bookmark_onto_an_existing_url_is_rejected() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut workspaces = vec![Workspace::default_workspace()];
        let mut work = Workspace::default_workspace();
        work.id = 2;
        work.name = "Work".to_string();
        work.is_default = false;
        work.is_active = false;
        workspaces.push(work);

        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            workspaces,
            vec![],
        );
        // Same URL in two workspaces. Moving the Personal row into Work must
        // be rejected because Work already owns that URL.
        manager.bookmarks.push(BookmarkItem {
            id: -1,
            workspace_id: 1,
            url: "https://shared.example".to_string(),
            title: "Old".to_string(),
            folder: "Personal".to_string(),
        });
        manager.bookmarks.push(BookmarkItem {
            id: -1,
            workspace_id: 2,
            url: "https://shared.example".to_string(),
            title: "Target".to_string(),
            folder: "Work".to_string(),
        });

        manager.save_bookmark(
            1,
            "https://shared.example".to_string(),
            2,
            "https://shared.example".to_string(),
            "Edited".to_string(),
            "Work".to_string(),
        );

        assert_eq!(manager.bookmarks.len(), 2);
        assert_eq!(manager.bookmarks[0].workspace_id, 1);
        assert_eq!(manager.bookmarks[0].url, "https://shared.example");
        assert_eq!(manager.bookmarks[0].title, "Old");
        assert_eq!(manager.bookmarks[1].workspace_id, 2);
        assert_eq!(manager.bookmarks[1].title, "Target");
    }

    #[test]
    fn closing_the_last_tab_stays_in_the_same_workspace() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );

        manager.create_tab(Some("https://personal.example".to_string()), true);
        manager.create_workspace(
            "Work".to_string(),
            WorkspaceIcon::Code,
            WorkspaceColor::Green,
        );
        manager.switch_workspace(1);

        let personal_id = manager.active_tab_id.expect("personal tab is active");
        manager.close_tab(personal_id);

        assert_eq!(manager.active_workspace_id, 1);
        let active = manager
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == manager.active_tab_id)
            .expect("a replacement tab is opened");
        assert_eq!(active.workspace_id, 1);
    }

    #[test]
    fn restore_session_normalizes_unknown_workspace_ids() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );

        manager.restore_session(vec![SessionTabSnapshot {
            workspace_id: 99,
            url: "https://stale.example".to_string(),
            title: "Stale".to_string(),
            is_active: true,
            is_hibernated: true,
            is_pinned: false,
        }]);

        assert_eq!(manager.active_workspace_id, 1);
        assert!(manager.tabs.iter().all(|tab| tab.workspace_id == 1));
    }

    #[test]
    fn workspace_selection_prefers_last_active_over_default() {
        let mut workspaces = vec![
            Workspace {
                id: 1,
                name: "Personal".to_string(),
                icon: WorkspaceIcon::Work,
                color: WorkspaceColor::Blue,
                is_default: true,
                is_active: false,
            },
            Workspace {
                id: 2,
                name: "Studio".to_string(),
                icon: WorkspaceIcon::Palette,
                color: WorkspaceColor::Purple,
                is_default: false,
                is_active: true,
            },
        ];
        assert_eq!(
            normalize_workspace_selection(&mut workspaces, WorkspaceStartupBehavior::LastActive),
            2
        );
        assert!(!workspaces[0].is_active);
        assert!(workspaces[1].is_active);
        assert!(workspaces[0].is_default);
        assert!(!workspaces[1].is_default);
    }

    #[test]
    fn workspace_selection_can_prefer_the_default_space() {
        let mut workspaces = vec![
            Workspace {
                id: 1,
                name: "Personal".to_string(),
                icon: WorkspaceIcon::Work,
                color: WorkspaceColor::Blue,
                is_default: true,
                is_active: false,
            },
            Workspace {
                id: 2,
                name: "Studio".to_string(),
                icon: WorkspaceIcon::Palette,
                color: WorkspaceColor::Purple,
                is_default: false,
                is_active: true,
            },
        ];
        assert_eq!(
            normalize_workspace_selection(
                &mut workspaces,
                WorkspaceStartupBehavior::DefaultWorkspace
            ),
            1
        );
        assert!(workspaces[0].is_active);
        assert!(!workspaces[1].is_active);
        assert!(workspaces[0].is_default);
    }

    #[test]
    fn last_active_workspace_restores_and_default_stays_separate() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![
                Workspace {
                    is_active: false,
                    ..Workspace::default_workspace()
                },
                Workspace {
                    id: 2,
                    name: "Studio".to_string(),
                    icon: WorkspaceIcon::Palette,
                    color: WorkspaceColor::Purple,
                    is_default: false,
                    is_active: true,
                },
            ],
            vec![],
        );

        // The workspace open at shutdown, not the default, opens at startup.
        assert_eq!(manager.active_workspace_id, 2);
        assert!(manager.workspaces[0].is_default);
        assert!(!manager.workspaces[1].is_default);

        manager.set_default_workspace(2);
        assert!(!manager.workspaces[0].is_default);
        assert!(manager.workspaces[1].is_default);
        assert_eq!(manager.active_workspace_id, 2);

        let snapshots = manager.workspace_snapshots();
        assert!(!snapshots[0].is_default);
        assert!(snapshots[1].is_default);
        assert!(!snapshots[0].is_active);
        assert!(snapshots[1].is_active);

        let (restart_tx, restart_rx) = crossbeam_channel::unbounded();
        let restarted = TabManager::new(
            restart_tx,
            restart_rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            snapshots,
            vec![],
        );
        assert_eq!(restarted.active_workspace_id, 2);
    }

    #[test]
    fn restore_keeps_default_workspace_when_it_has_no_saved_tabs() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![
                Workspace {
                    is_default: false,
                    is_active: false,
                    ..Workspace::default_workspace()
                },
                Workspace {
                    id: 2,
                    name: "Studio".to_string(),
                    icon: WorkspaceIcon::Palette,
                    color: WorkspaceColor::Purple,
                    is_default: true,
                    is_active: false,
                },
            ],
            vec![],
        );

        manager.restore_session(vec![SessionTabSnapshot {
            workspace_id: 1,
            url: "https://personal.example".to_string(),
            title: "Personal".to_string(),
            is_active: true,
            is_hibernated: true,
            is_pinned: false,
        }]);

        assert_eq!(manager.active_workspace_id, 2);
        let active = manager
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == manager.active_tab_id)
            .expect("fresh tab is created");
        assert_eq!(active.workspace_id, 2);
    }

    #[test]
    fn deleting_the_default_workspace_reassigns_it() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut manager = TabManager::new(
            tx,
            rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 180,
                y: 0,
                width: 1100,
                height: 800,
                scale_factor: 1.0,
            },
            None,
            &BrowserPreferences::default(),
            vec![
                Workspace::default_workspace(),
                Workspace {
                    id: 2,
                    name: "Studio".to_string(),
                    icon: WorkspaceIcon::Palette,
                    color: WorkspaceColor::Purple,
                    is_default: false,
                    is_active: false,
                },
            ],
            vec![],
        );

        manager.delete_workspace(1);

        assert_eq!(manager.workspaces.len(), 1);
        assert_eq!(manager.workspaces[0].id, 2);
        assert!(manager.workspaces[0].is_default);
    }

    #[test]
    fn preferred_favicon_url_prefers_site_icon_candidates() {
        assert_eq!(preferred_favicon_url(&[]), None);

        let urls = vec![
            "https://example.com/apple-touch-icon.png".to_string(),
            "https://example.com/icon-512.png".to_string(),
            "https://example.com/favicon.ico".to_string(),
        ];
        assert_eq!(
            preferred_favicon_url(&urls),
            Some("https://example.com/favicon.ico".to_string())
        );

        let urls = vec![
            "https://example.com/apple-touch-icon.png".to_string(),
            "https://example.com/theme-icon.png".to_string(),
        ];
        assert_eq!(
            preferred_favicon_url(&urls),
            Some("https://example.com/apple-touch-icon.png".to_string())
        );

        let urls = vec![
            "https://example.com/favicon-32x32.png".to_string(),
            "https://example.com/legacy.ico".to_string(),
        ];
        assert_eq!(
            preferred_favicon_url(&urls),
            Some("https://example.com/favicon-32x32.png".to_string())
        );

        let urls = vec![
            "https://cdn.example.com/a.png".to_string(),
            "https://cdn.example.com/b.png".to_string(),
        ];
        assert_eq!(
            preferred_favicon_url(&urls),
            Some("https://cdn.example.com/a.png".to_string())
        );
    }
}
