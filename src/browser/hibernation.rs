//! Tab lifecycle state machine and RAM-pressure-based hibernation engine.
//!
//! Enforces:
//! - Default idle timeout: 600s (10 min).
//! - High RAM pressure (>= 80%): 30s idle timeout.
//! - Critical RAM pressure (>= 90%): hibernate ALL background tabs immediately.
//! - Never hibernate the active tab.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cef::{
    CefString, ContentSettingTypes, ContentSettingValues, ImplBrowser, ImplBrowserHost,
    ImplDownloadItemCallback, ImplFrame, ImplRequestContext, ImplUrlrequest,
};
use crossbeam_channel::{Receiver, Sender};
use sysinfo::{ProcessesToUpdate, System};

use crate::browser::favicon::create_favicon_download_callback;
use crate::browser::find;
use crate::browser::parallel_download::{
    self, part_path_for, safe_file_name, DownloadWriter, SegmentRange,
};
use crate::browser::{adblocker, client, display_capture};
use crate::config::{
    BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon, WorkspaceStartupBehavior,
    PARALLEL_DOWNLOAD_SEGMENTS, RAM_CHECK_INTERVAL, RAM_CRITICAL_PRESSURE_THRESHOLD,
    RAM_HIGH_PRESSURE_THRESHOLD, TAB_IDLE_TIMEOUT_PRESSURE, TAB_WATCHDOG_TICK_INTERVAL,
};
use crate::events::{
    normalize_permission_origin, BrowserEvent, BrowserShortcut, DownloadEngine, DownloadInfo,
    DownloadNotification, DownloadNotificationKind, DownloadState, FindOptions, FindSnippet,
    MediaAccessPermissions, MediaPermissionCallback, ParallelDownloadEvent, UiAction,
    WebNotification,
};
use crate::storage::db::{BookmarkItem, SessionTabSnapshot, SitePermission, StorageHandle};
use crate::ui::chrome::{
    BookmarkInfo, ChromeState, DisplayMediaPicker, Favicon, PermissionPrompt, PermissionPromptKind,
    TabDisplayState, TabInfo, WorkspaceInfo,
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

/// One unresolved native media-access prompt. The CEF callback is moved to the
/// main UI thread and kept here until Allow/Deny/dismiss removes this entry,
/// which makes every callback callable at most once.
struct PendingMediaPermission {
    tab_id: u64,
    origin: String,
    permissions: u32,
    private: bool,
    callback: MediaPermissionCallback,
    prompt_id: Option<u64>,
    /// A remembered audio grant waits briefly so a simultaneous video request
    /// can start first. Chromium's capture service rejects video when audio is
    /// already starting, but accepts audio immediately after video.
    auto_resolve: Option<(Instant, bool)>,
}

/// Let a camera request overtake a remembered microphone grant from the same
/// page load. Meet requests audio a few milliseconds before video.
const AUDIO_AUTO_RESOLVE_DELAY: Duration = Duration::from_millis(150);

impl std::fmt::Debug for PendingMediaPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingMediaPermission")
            .field("tab_id", &self.tab_id)
            .field("origin", &self.origin)
            .field("permissions", &self.permissions)
            .field("private", &self.private)
            .field("prompt_id", &self.prompt_id)
            .field("auto_resolve", &self.auto_resolve)
            .finish_non_exhaustive()
    }
}

/// One unresolved native Notification API permission prompt. The renderer token
/// identifies the page promise that must be resolved after the user decides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingNotificationPermission {
    tab_id: u64,
    origin: String,
    token: String,
    private: bool,
}

/// A native display picker awaiting a user choice. The renderer's pending
/// Promise is resolved by executing JavaScript back in the originating tab.
#[derive(Debug, Clone)]
struct PendingDisplayMediaPicker {
    tab_id: u64,
    picker: DisplayMediaPicker,
}

/// Resolve a one-shot CEF callback on the UI thread. MediaAccess callbacks need
/// the exact requested bitmask; Chrome-runtime prompt callbacks are dismissed
/// because CEF's Accept/Deny paths unconditionally consume request callbacks.
/// Content settings are kept in sync separately. Pending entries are removed
/// before this runs, so CEF never sees the same callback twice.
fn resolve_permission_callback(callback: MediaPermissionCallback, granted: bool, permissions: u32) {
    callback.resolve(granted, permissions);
}

/// Remove a pending item and its queue position atomically. Returning the removed
/// value is what guarantees a one-shot callback cannot be resolved twice.
fn take_pending_permission<T>(
    pending: &mut HashMap<u64, T>,
    queue: &mut VecDeque<u64>,
    request_id: u64,
) -> Option<T> {
    let value = pending.remove(&request_id)?;
    queue.retain(|id| *id != request_id);
    Some(value)
}

/// A saved decision only auto-resolves when every recognized permission in the
/// request has the same answer. Mixed rules are shown to the user instead.
fn remembered_permission_decision(
    saved: &BTreeMap<(String, String), bool>,
    origin: &str,
    permissions: u32,
) -> Option<bool> {
    let kinds = MediaAccessPermissions::from_bits(permissions).kinds();
    let mut decision = None;
    for kind in kinds {
        let allowed = *saved.get(&(origin.to_string(), kind.to_string()))?;
        match decision {
            None => decision = Some(allowed),
            Some(previous) if previous != allowed => return None,
            Some(_) => {}
        }
    }
    decision
}

/// Record one explicit site-permission decision. Private decisions only touch
/// the in-memory incognito map; `storage` is deliberately not called for them.
fn record_site_permission_decision(
    saved: &mut BTreeMap<(String, String), bool>,
    private_saved: &mut BTreeMap<(String, String), bool>,
    storage: Option<&StorageHandle>,
    origin: &str,
    kind: &str,
    is_private: bool,
    granted: bool,
) {
    if kind.is_empty() || origin.is_empty() {
        return;
    }
    let target = if is_private { private_saved } else { saved };
    target.insert((origin.to_string(), kind.to_string()), granted);
    if !is_private {
        if let Some(storage) = storage {
            storage.save_site_permission(origin.to_string(), kind.to_string(), granted);
        }
    }
}

/// Record an explicit Allow/Deny decision for every recognized media bit.
fn record_media_permission(
    saved: &mut BTreeMap<(String, String), bool>,
    private_saved: &mut BTreeMap<(String, String), bool>,
    storage: Option<&StorageHandle>,
    origin: &str,
    permissions: u32,
    is_private: bool,
    granted: bool,
) {
    let kinds = MediaAccessPermissions::from_bits(permissions).kinds();
    if kinds.is_empty() || origin.is_empty() {
        return;
    }
    for kind in kinds {
        record_site_permission_decision(
            saved,
            private_saved,
            storage,
            origin,
            kind,
            is_private,
            granted,
        );
    }
}

/// Keep Chromium's own camera/microphone content settings aligned with the
/// decision Kasumi just showed. Letting Chromium auto-approve future requests
/// avoids creating a `PermissionPromptCallback` for them at all, which is the
/// only safe way to grant a content setting without risking CEF's one-shot
/// prompt callback path.
fn sync_media_content_settings(
    context: Option<&cef::RequestContext>,
    origin: &str,
    permissions: u32,
    granted: bool,
) {
    let Some(context) = context else {
        return;
    };
    if origin.is_empty() {
        return;
    }

    let value = if granted {
        ContentSettingValues::ALLOW
    } else {
        ContentSettingValues::BLOCK
    };
    let requesting_url = CefString::from(origin);
    for (bit, content_type) in [
        (
            MediaAccessPermissions::AUDIO,
            ContentSettingTypes::MEDIASTREAM_MIC,
        ),
        (
            MediaAccessPermissions::VIDEO,
            ContentSettingTypes::MEDIASTREAM_CAMERA,
        ),
    ] {
        if MediaAccessPermissions::from_bits(permissions).contains(bit) {
            context.set_content_setting(Some(&requesting_url), None, content_type, value.clone());
        }
    }
}

/// Plain-data mirror of a CEF `OnFindResult` callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FindResultSnapshot {
    pub count: i32,
    pub active_ordinal: i32,
    pub final_update: bool,
    /// Our monotonic request id, shared by the native and rich bridges.
    pub request_id: u64,
    /// Advanced queries must ignore in-flight CEF results from an older query.
    pub advanced: bool,
    /// A hibernated tab is queued to run this query when its browser wakes.
    pub pending: bool,
}

/// A find request that must wait until a hibernated tab's browser is recreated.
#[derive(Debug, Clone)]
struct PendingFindRequest {
    query: String,
    match_case: bool,
    options: FindOptions,
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
    sys: Option<System>,
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

    // One in-memory CEF request context shared by the incognito workspace. It
    // lives on the main UI thread and survives tab hibernation, but is dropped
    // as soon as the last private tab closes.
    private_context: Option<cef::RequestContext>,
    // Normal space to restore on the next launch when the user leaves the
    // incognito space visible. Private state is never persisted.
    last_normal_workspace_id: u64,
    // Download ids originating from incognito tabs. They stay in memory even
    // after the source tab closes so late CEF updates cannot reach SQLite.
    private_download_ids: HashSet<u32>,

    // Unresolved WebRTC permission prompts. Both maps and callbacks live only
    // on the main UI thread; CEF handlers send plain events over crossbeam.
    pending_media_permissions: HashMap<u64, PendingMediaPermission>,
    pending_media_permission_queue: VecDeque<u64>,
    pending_notification_permissions: HashMap<u64, PendingNotificationPermission>,
    pending_notification_permission_queue: VecDeque<u64>,
    pending_display_media_pickers: HashMap<String, PendingDisplayMediaPicker>,
    pending_display_media_queue: VecDeque<String>,
    /// Native web notifications waiting for the shell to surface them.
    web_notifications: VecDeque<WebNotification>,
    /// Notification ids a page closed before the shell could dismiss them.
    closed_web_notifications: Vec<String>,
    saved_site_permissions: BTreeMap<(String, String), bool>,
    private_site_permissions: BTreeMap<(String, String), bool>,

    /// Latest CEF find result per tab, mirrored into `TabInfo` for the find bar
    /// and cross-tab result list. Kept outside `Tab` so every existing tab
    /// constructor stays small and hibernation never has to reset find state.
    find_results: HashMap<u64, FindResultSnapshot>,
    /// Rich snippets and invalid-regex messages mirrored from the renderer shim.
    find_snippets: HashMap<u64, Vec<FindSnippet>>,
    find_errors: HashMap<u64, String>,
    next_find_request_id: u64,
    pending_find_on_wake: HashMap<u64, PendingFindRequest>,

    // Number of `host.close_browser(1)` calls awaiting an `on_before_close` event.
    pending_browser_closes: u32,

    // Browser creation is deferred out of winit window-event callbacks. CEF's
    // external message pump re-enters the run loop on macOS, so creating a
    // browser directly from a key/redraw event can intermittently lose the
    // initial navigation. `tick()` drains this queue from `about_to_wait`.
    pending_browser_spawns: VecDeque<(u64, String, BrowserSpawnMode)>,

    // Retry state for browsers that have not committed their initial URL.
    pending_navigation_checks: HashMap<u64, PendingNavigationCheck>,

    // Set when CEF forwards Cmd/Ctrl+F; consumed by the winit handler.
    find_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+K; consumed by the winit handler.
    command_palette_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+,; consumed by the winit handler.
    settings_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+J; consumed by the winit handler.
    downloads_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+Shift+B; consumed by the winit handler.
    bookmark_manager_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+Shift+C; consumed by the winit handler.
    copy_url_requested: bool,

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
        let mut sys = System::new().ok();
        if let Some(system) = sys.as_mut() {
            system.refresh_memory();
        }
        let total = sys.as_ref().map_or(0, System::total_memory);
        let used = sys.as_ref().map_or(0, System::used_memory);
        let adblock_engine = adblocker::create_adblock_engine();
        let adblock_enabled = Arc::new(AtomicBool::new(preferences.adblock_enabled));
        let download_preferences = client::SharedDownloadPreferences::from_preferences(preferences);

        let mut workspaces = workspaces;
        // A private workspace must never be restored from SQLite, even if an
        // older database somehow contains one.
        workspaces.retain(|workspace| !Workspace::is_private_id(workspace.id));
        let mut bookmarks = bookmarks;
        bookmarks.retain(|bookmark| !Workspace::is_private_id(bookmark.workspace_id));
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
            private_context: None,
            last_normal_workspace_id: active_workspace_id,
            private_download_ids: HashSet::new(),
            pending_media_permissions: HashMap::new(),
            pending_media_permission_queue: VecDeque::new(),
            pending_notification_permissions: HashMap::new(),
            pending_notification_permission_queue: VecDeque::new(),
            pending_display_media_pickers: HashMap::new(),
            pending_display_media_queue: VecDeque::new(),
            web_notifications: VecDeque::new(),
            closed_web_notifications: Vec::new(),
            saved_site_permissions: BTreeMap::new(),
            private_site_permissions: BTreeMap::new(),
            find_results: HashMap::new(),
            find_snippets: HashMap::new(),
            find_errors: HashMap::new(),
            next_find_request_id: 1,
            pending_find_on_wake: HashMap::new(),
            pending_browser_closes: 0,
            pending_browser_spawns: VecDeque::new(),
            pending_navigation_checks: HashMap::new(),
            find_requested: false,
            command_palette_requested: false,
            settings_requested: false,
            downloads_requested: false,
            bookmark_manager_requested: false,
            copy_url_requested: false,
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

    /// Seed remembered Allow/Deny answers from SQLite. Normal-space requests
    /// can then resolve without showing a prompt; Incognito keeps its own
    /// in-memory map and never reads or writes this cache.
    pub fn seed_site_permissions(&mut self, permissions: Vec<SitePermission>) {
        self.saved_site_permissions.clear();
        let mut content_masks: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for permission in permissions {
            let origin = normalize_permission_origin(&permission.origin);
            if origin.is_empty() {
                continue;
            }
            // Notification decisions live in the same table but do not map to a
            // CEF media bit, so they skip the Chromium content-setting sync.
            if permission.permission != "notifications" {
                let bit = MediaAccessPermissions::bits_for_kind(&permission.permission);
                if bit == 0 {
                    continue;
                }
                // Web apps such as Google Meet inspect Chromium's own
                // content-setting state before requesting media. Restored
                // decisions must be visible there too, not only in Kasumi's
                // callback cache.
                let entry = content_masks.entry(origin.clone()).or_insert((0, 0));
                if permission.allowed {
                    entry.0 |= bit;
                } else {
                    entry.1 |= bit;
                }
            }
            self.saved_site_permissions
                .insert((origin, permission.permission), permission.allowed);
        }

        let global_context = cef::request_context_get_global_context();
        for (origin, (allowed, blocked)) in content_masks {
            if allowed != 0 {
                sync_media_content_settings(global_context.as_ref(), &origin, allowed, true);
            }
            if blocked != 0 {
                sync_media_content_settings(global_context.as_ref(), &origin, blocked, false);
            }
        }
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

        // Remember privacy before the tab can close: late CEF updates keep
        // arriving after `on_before_close` and must still bypass SQLite.
        if self.is_private_download(&info) {
            self.private_download_ids.insert(info.id);
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

        self.persist_download(info);
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
        self.persist_download(info);
    }

    fn now_unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default()
    }

    /// Reserved incognito space is the only workspace that must never touch
    /// SQLite. Keeping the check in one place avoids accidental persistence
    /// when new tab actions are added.
    fn is_private_workspace(workspace_id: u64) -> bool {
        Workspace::is_private_id(workspace_id)
    }

    fn is_private_tab(&self, tab_id: u64) -> bool {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .is_some_and(|tab| Self::is_private_workspace(tab.workspace_id))
    }

    /// Lazily create CEF's in-memory request context. An empty cache path puts
    /// the browser into Chromium's incognito mode: cookies, HTTP cache,
    /// IndexedDB, localStorage, and service workers stay off disk.
    fn private_context(&mut self) -> Option<cef::RequestContext> {
        if self.private_context.is_none() {
            let mut settings = cef::RequestContextSettings::default();
            settings.cache_path = cef::CefString::from("");
            settings.persist_session_cookies = 0;
            self.private_context = cef::request_context_create_context(Some(&settings), None);
            if self.private_context.is_none() {
                tracing::error!("Could not create CEF incognito request context");
            }
        }
        self.private_context.clone()
    }

    fn request_context_for_tab(&self, tab_id: u64) -> Option<cef::RequestContext> {
        if self.is_private_tab(tab_id) {
            return self.private_context.clone();
        }
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.host())
            .and_then(|host| host.request_context())
    }

    fn is_private_download(&self, info: &DownloadInfo) -> bool {
        self.private_download_ids.contains(&info.id) || self.is_private_tab(info.tab_id)
    }

    fn is_private_download_id(&self, download_id: u32) -> bool {
        self.private_download_ids.contains(&download_id)
            || self
                .downloads
                .iter()
                .any(|info| info.id == download_id && self.is_private_tab(info.tab_id))
    }

    fn active_media_permission_request_id(&self) -> Option<u64> {
        self.pending_media_permission_queue
            .iter()
            .find(|id| {
                self.pending_media_permissions
                    .get(id)
                    .is_some_and(|pending| pending.auto_resolve.is_none())
            })
            .copied()
    }

    /// Resolve remembered audio grants whose short delay expired. Called after
    /// each event drain so a video request can start first.
    fn resolve_ready_auto_media_permissions(&mut self) {
        let now = Instant::now();
        let ready: Vec<(u64, bool)> = self
            .pending_media_permissions
            .iter()
            .filter_map(|(id, pending)| {
                pending
                    .auto_resolve
                    .and_then(|(at, allowed)| (at <= now).then_some((*id, allowed)))
            })
            .collect();
        for (request_id, allowed) in ready {
            self.resolve_media_permission(request_id, allowed, false);
        }
    }

    /// Pending requests from the same tab, origin, and permission mask belong
    /// to one browser request. CEF can deliver both a Chrome-style content
    /// prompt and an Alloy media-access callback for the same getUserMedia
    /// call; one Allow/Deny must resolve both or the camera stays blocked.
    fn media_permission_group_ids(&self, request_id: u64) -> Vec<u64> {
        let Some(active) = self.pending_media_permissions.get(&request_id) else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = self
            .pending_media_permissions
            .iter()
            .filter(|(_, pending)| {
                pending.tab_id == active.tab_id
                    && pending.origin == active.origin
                    && pending.permissions == active.permissions
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Resolve one logical prompt exactly once: remove every callback in its
    /// group first, then call each one-shot callback only if removal succeeded.
    /// Explicit Allow/Deny are remembered; Escape/click-outside dismisses the
    /// current request without saving a rule.
    fn resolve_media_permission(&mut self, request_id: u64, granted: bool, persist: bool) {
        let ids = self.media_permission_group_ids(request_id);
        if ids.is_empty() {
            tracing::warn!("Ignoring stale media permission resolution {request_id}");
            return;
        }
        let pendings: Vec<PendingMediaPermission> = ids
            .into_iter()
            .filter_map(|id| {
                take_pending_permission(
                    &mut self.pending_media_permissions,
                    &mut self.pending_media_permission_queue,
                    id,
                )
            })
            .collect();
        if pendings.is_empty() {
            return;
        }

        let mut permissions = 0;
        let mut origin = String::new();
        let mut private = false;
        let mut tab_id = None;
        for pending in &pendings {
            permissions |= pending.permissions;
            if origin.is_empty() {
                origin = pending.origin.clone();
                private = pending.private;
                tab_id = Some(pending.tab_id);
            }
        }

        // Persist before moving the one-shot callbacks out of `pending`.
        if persist {
            record_media_permission(
                &mut self.saved_site_permissions,
                &mut self.private_site_permissions,
                self.storage.as_ref(),
                &origin,
                permissions,
                private,
                granted,
            );
        }

        // Incognito decisions stay in memory and never reach the shared CEF
        // content settings either. For normal tabs, update Chromium's content
        // settings so the next request for this origin is auto-approved and
        // never produces another one-shot permission prompt callback.
        if !private {
            let context = tab_id.and_then(|id| self.request_context_for_tab(id));
            sync_media_content_settings(context.as_ref(), &origin, permissions, granted);
        }
        tracing::info!(
            "Resolving {} media callback(s) for tab {} as {}",
            pendings.len(),
            request_id,
            if granted { "allow" } else { "deny" }
        );
        // CEF requires getUserMedia callbacks to grant exactly the requested
        // mask. Unknown future bits are still passed through on explicit Allow;
        // only persistence is limited to permissions Kasumi can describe.
        for pending in pendings {
            resolve_permission_callback(pending.callback, granted, pending.permissions);
        }
    }

    fn active_notification_permission_request_id(&self) -> Option<u64> {
        self.pending_notification_permission_queue
            .iter()
            .find(|id| self.pending_notification_permissions.contains_key(id))
            .copied()
    }

    /// Resolve the renderer's `Notification.requestPermission()` promise. The
    /// pending entry is removed first so navigation or a duplicate action can
    /// never resolve the same request twice.
    fn resolve_notification_permission(&mut self, request_id: u64, granted: bool, persist: bool) {
        let Some(pending) = take_pending_permission(
            &mut self.pending_notification_permissions,
            &mut self.pending_notification_permission_queue,
            request_id,
        ) else {
            tracing::warn!("Ignoring stale notification permission resolution {request_id}");
            return;
        };
        if persist {
            record_site_permission_decision(
                &mut self.saved_site_permissions,
                &mut self.private_site_permissions,
                self.storage.as_ref(),
                &pending.origin,
                "notifications",
                pending.private,
                granted,
            );
        }
        // A transient dismissal leaves the page in the askable "default" state;
        // only an explicit Allow/Deny becomes a remembered permission.
        let decision = if granted {
            "granted"
        } else if persist {
            "denied"
        } else {
            "default"
        };
        self.evaluate_notification_permission(pending.tab_id, &pending.token, decision);
    }

    /// Execute a small script in the originating tab. A missing frame means the
    /// page navigated or closed while the prompt was open; the renderer promise
    /// died with the frame, so there is nothing left to resolve.
    fn evaluate_notification_permission(&self, tab_id: u64, token: &str, decision: &str) {
        let token_json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
        let decision_json =
            serde_json::to_string(decision).unwrap_or_else(|_| "\"denied\"".to_string());
        let script = format!(
            "window.__kasumiResolveNotificationPermission && window.__kasumiResolveNotificationPermission({token_json}, {decision_json});"
        );
        let resolved = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.main_frame())
            .map(|frame| {
                frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
            })
            .is_some();
        if !resolved {
            tracing::warn!(
                "Could not resolve notification permission for tab {tab_id}: no live frame"
            );
        }
    }

    /// Dispatch a click/close event into the page that owns the notification.
    fn dispatch_web_notification_event(&self, tab_id: u64, notification_id: &str, event: &str) {
        let id_json = serde_json::to_string(notification_id).unwrap_or_else(|_| "\"\"".to_string());
        let event_json = serde_json::to_string(event).unwrap_or_else(|_| "\"close\"".to_string());
        let script = format!(
            "window.__kasumiNotificationEvent && window.__kasumiNotificationEvent({id_json}, {event_json});"
        );
        let dispatched = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.main_frame())
            .map(|frame| {
                frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
            })
            .is_some();
        if !dispatched {
            tracing::debug!(
                "Notification {notification_id} event {event} skipped: tab {tab_id} has no live frame"
            );
        }
    }

    fn discard_notification_permissions_for_tab(&mut self, tab_id: u64) {
        let ids: Vec<u64> = self
            .pending_notification_permissions
            .iter()
            .filter(|(_, pending)| pending.tab_id == tab_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = take_pending_permission(
                &mut self.pending_notification_permissions,
                &mut self.pending_notification_permission_queue,
                id,
            );
        }
    }

    fn discard_all_notification_permissions(&mut self) {
        self.pending_notification_permissions.clear();
        self.pending_notification_permission_queue.clear();
    }

    /// Drain notifications that arrived since the last UI frame. Closed ids are
    /// also returned so a page-initiated `close()` can dismiss a visible toast.
    pub fn consume_web_notifications(&mut self) -> Vec<WebNotification> {
        self.web_notifications.drain(..).collect()
    }

    pub fn consume_closed_web_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.closed_web_notifications)
    }

    /// Drop prompts whose browser is going away. Calling a CEF permission
    /// callback during navigation or teardown can run against a destroyed
    /// `PermissionRequestManager` and trap inside Chromium. Since the page is
    /// already gone, releasing the callback without invoking it is safer; CEF
    /// disposes the request with its browser.
    fn discard_media_permissions_for_tab(&mut self, tab_id: u64) {
        let ids: Vec<u64> = self
            .pending_media_permissions
            .iter()
            .filter(|(_, pending)| pending.tab_id == tab_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = take_pending_permission(
                &mut self.pending_media_permissions,
                &mut self.pending_media_permission_queue,
                id,
            );
        }
        self.discard_notification_permissions_for_tab(tab_id);
        self.discard_display_media_for_tab(tab_id);
    }

    fn discard_all_media_permissions(&mut self) {
        let ids: Vec<u64> = self.pending_media_permissions.keys().copied().collect();
        for id in ids {
            let _ = take_pending_permission(
                &mut self.pending_media_permissions,
                &mut self.pending_media_permission_queue,
                id,
            );
        }
        self.discard_all_notification_permissions();
        self.discard_all_display_media();
    }

    fn active_display_media_token(&self) -> Option<String> {
        self.pending_display_media_queue.front().cloned()
    }

    /// Drop picker state when its page is closing or navigating. No JavaScript is
    /// sent here: the renderer's pending Promise dies with the frame.
    fn discard_display_media_for_tab(&mut self, tab_id: u64) {
        let tokens: Vec<String> = self
            .pending_display_media_pickers
            .iter()
            .filter(|(_, pending)| pending.tab_id == tab_id)
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.pending_display_media_pickers.remove(&token);
            self.pending_display_media_queue
                .retain(|value| value != &token);
        }
    }

    fn discard_all_display_media(&mut self) {
        self.pending_display_media_pickers.clear();
        self.pending_display_media_queue.clear();
    }

    /// Resolve the renderer's `getDisplayMedia` Promise with the selected
    /// Chromium `DesktopMediaID`, or reject it when the user cancels.
    fn resolve_display_media_picker(&mut self, token: String, device_id: Option<String>) {
        let Some(pending) = self.pending_display_media_pickers.remove(&token) else {
            tracing::warn!("Ignoring stale display media picker {token}");
            return;
        };
        self.pending_display_media_queue
            .retain(|value| value != &token);

        let token_json = serde_json::to_string(&token).unwrap_or_else(|_| "\"\"".to_string());
        let device_json = device_id
            .as_deref()
            .and_then(|value| serde_json::to_string(value).ok())
            .unwrap_or_else(|| "null".to_string());
        let script = format!(
            "window.__kasumiResolveDisplayMedia && window.__kasumiResolveDisplayMedia({token_json}, {device_json});"
        );
        let resolved = self
            .tabs
            .iter()
            .find(|tab| tab.id == pending.tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.main_frame())
            .map(|frame| {
                frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
            })
            .is_some();
        if !resolved {
            tracing::warn!(
                "Could not resolve display media picker {token}: tab {} has no live frame",
                pending.tab_id
            );
        }
    }

    /// Single storage boundary for download snapshots. Private downloads are
    /// shown in the native bubble for the session but never reach the WAL.
    fn persist_download(&self, info: DownloadInfo) {
        if self.is_private_download(&info) {
            return;
        }
        if let Some(ref storage) = self.storage {
            storage.save_download(info);
        }
    }

    fn normal_workspace_fallback(&self) -> u64 {
        if !Self::is_private_workspace(self.last_normal_workspace_id)
            && self
                .workspaces
                .iter()
                .any(|workspace| workspace.id == self.last_normal_workspace_id)
        {
            return self.last_normal_workspace_id;
        }
        self.workspaces
            .iter()
            .find(|workspace| !Self::is_private_workspace(workspace.id))
            .map(|workspace| workspace.id)
            .unwrap_or(Workspace::DEFAULT_ID)
    }

    fn remember_normal_workspace(&mut self, workspace_id: u64) {
        if !Self::is_private_workspace(workspace_id) {
            self.last_normal_workspace_id = workspace_id;
        }
    }

    fn ensure_private_workspace(&mut self) {
        if !self
            .workspaces
            .iter()
            .any(|workspace| Self::is_private_workspace(workspace.id))
        {
            self.workspaces.push(Workspace::private_workspace());
            self.regroup_tabs();
        }
    }

    /// Remove the ephemeral workspace and CEF request context after the last
    /// private tab closes. Download ids are retained until their CEF callbacks
    /// finish so late updates still bypass SQLite.
    fn cleanup_private_workspace_if_empty(&mut self, workspace_id: u64) -> bool {
        if !Self::is_private_workspace(workspace_id)
            || self
                .tabs
                .iter()
                .any(|tab| Self::is_private_workspace(tab.workspace_id))
        {
            return false;
        }
        self.workspaces
            .retain(|workspace| !Self::is_private_workspace(workspace.id));
        self.private_context = None;
        self.private_site_permissions.clear();
        if Self::is_private_workspace(self.active_workspace_id) {
            self.active_workspace_id = self.normal_workspace_fallback();
        }
        true
    }

    /// Open the incognito space from a shell shortcut, native menu, or palette.
    /// Pressing it again while incognito is visible opens another private tab,
    /// matching the familiar browser behavior without spawning a second native
    /// window.
    pub fn new_private_window(&mut self) {
        self.ensure_private_workspace();
        if Workspace::is_private_id(self.active_workspace_id) {
            tracing::info!("Opening another tab in the incognito space");
            self.create_tab(None, true);
            return;
        }
        tracing::info!("Opening incognito space with isolated CEF request context");
        self.switch_workspace(Workspace::PRIVATE_ID);
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
        let snapshot = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
            .map(|info| {
                info.segments = 1;
                info.total_bytes = total_bytes;
                info.received_bytes = 0;
                info.percent_complete = 0;
                info.state = DownloadState::InProgress;
                info.updated_at = Self::now_unix_secs();
                info.clone()
            });
        if let Some(snapshot) = snapshot {
            self.persist_download(snapshot);
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

        let snapshot = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
            .map(|info| {
                info.segments = ranges.len().min(u8::MAX as usize) as u8;
                info.total_bytes = runtime.total_bytes;
                info.updated_at = Self::now_unix_secs();
                info.clone()
            });
        if let Some(snapshot) = snapshot {
            self.persist_download(snapshot);
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
        let Some(runtime) = self.parallel_downloads.get(&download_id) else {
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
        let segments = runtime.segments.len().min(u8::MAX as usize) as u8;

        let snapshot = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
            .map(|info| {
                info.received_bytes = received;
                info.total_bytes = total;
                info.segments = segments;
                info.percent_complete = if total > 0 {
                    ((received.saturating_mul(100)) / total).clamp(0, 100) as i32
                } else {
                    0
                };
                info.updated_at = Self::now_unix_secs();
                info.clone()
            });
        if persist {
            if let Some(snapshot) = snapshot {
                self.persist_download(snapshot);
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
            let snapshot = self
                .downloads
                .iter_mut()
                .find(|item| item.id == download_id)
                .map(|info| {
                    info.current_speed = speed;
                    info.clone()
                });
            if let Some(snapshot) = snapshot {
                self.persist_download(snapshot);
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
        let snapshot = self
            .downloads
            .iter_mut()
            .find(|item| item.id == download_id)
            .map(|info| {
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
                info.clone()
            });
        if let Some(snapshot) = snapshot {
            self.persist_download(snapshot);
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
        // Defensive guard: a stale/corrupt database must never resurrect private
        // navigation into a normal workspace on startup.
        let mut ordered: Vec<SessionTabSnapshot> = restored
            .into_iter()
            .filter(|snapshot| !Self::is_private_workspace(snapshot.workspace_id))
            .collect();
        if ordered.is_empty() {
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
        let workspace_id = if Self::is_private_workspace(closed.workspace_id) {
            self.ensure_private_workspace();
            Workspace::PRIVATE_ID
        } else if self.workspaces.iter().any(|w| w.id == closed.workspace_id) {
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
        let active_id = if Self::is_private_workspace(self.active_workspace_id) {
            self.active_tab_by_workspace
                .get(&self.normal_workspace_fallback())
                .copied()
        } else {
            self.active_tab_id
        };
        self.tabs
            .iter()
            .filter(|tab| !Self::is_private_workspace(tab.workspace_id))
            .map(|t| SessionTabSnapshot {
                workspace_id: t.workspace_id,
                url: t.url.clone(),
                title: t.title.clone(),
                is_active: active_id == Some(t.id),
                is_hibernated: t.state == TabState::Hibernated,
                is_pinned: t.pinned,
            })
            .collect()
    }

    fn workspace_snapshots(&self) -> Vec<Workspace> {
        let active_id = if Self::is_private_workspace(self.active_workspace_id) {
            self.normal_workspace_fallback()
        } else {
            self.active_workspace_id
        };
        let snapshots: Vec<Workspace> = self
            .workspaces
            .iter()
            .filter(|workspace| !Self::is_private_workspace(workspace.id))
            .map(|workspace| Workspace {
                is_active: workspace.id == active_id,
                ..workspace.clone()
            })
            .collect();
        if snapshots.is_empty() {
            vec![Workspace::default_workspace()]
        } else {
            snapshots
        }
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
        self.discard_all_media_permissions();
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
        self.create_background_tab_in_workspace(url, self.active_workspace_id)
    }

    /// Open a background tab in a specific workspace, used by incognito popups.
    fn create_background_tab_in_workspace(&mut self, url: String, workspace_id: u64) -> u64 {
        let id =
            self.insert_tab_in_workspace(Some(url.clone()), None, None, false, false, workspace_id);
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
        if self.tabs[index].pinned
            || self.tabs[index].workspace_id == workspace_id
            || Self::is_private_workspace(self.tabs[index].workspace_id)
            || Self::is_private_workspace(workspace_id)
        {
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
        self.remember_normal_workspace(target_workspace);
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
        let private_workspace_closed = self.cleanup_private_workspace_if_empty(workspace_id);
        self.recently_closed.push(closed_tab_from(&tab, index));
        if self.active_tab_by_workspace.get(&tab.workspace_id) == Some(&id) {
            self.active_tab_by_workspace.remove(&tab.workspace_id);
        }
        self.pending_navigation_checks.remove(&id);
        self.pending_find_on_wake.remove(&id);
        self.find_results.remove(&id);
        self.find_snippets.remove(&id);
        self.find_errors.remove(&id);
        self.discard_media_permissions_for_tab(id);
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
                if same_workspace.is_empty() && private_workspace_closed {
                    // Incognito is gone, so return to the normal space that was
                    // visible before it rather than opening a private tab.
                    let fallback = self
                        .tabs
                        .iter()
                        .find(|tab| tab.workspace_id == self.active_workspace_id)
                        .map(|tab| tab.id);
                    if let Some(id) = fallback {
                        self.focus_tab(id);
                    } else {
                        self.create_tab(None, true);
                    }
                } else if same_workspace.is_empty() {
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
        self.discard_media_permissions_for_tab(id);
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
        self.remember_normal_workspace(workspace_id);
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
        if Self::is_private_workspace(workspace_id)
            || !self
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

        let highest = self
            .workspaces
            .iter()
            .filter(|workspace| !Self::is_private_workspace(workspace.id))
            .map(|workspace| workspace.id)
            .max()
            .unwrap_or(Workspace::DEFAULT_ID);
        let id = highest.checked_add(1).unwrap_or(Workspace::DEFAULT_ID + 1);
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
        if Self::is_private_workspace(workspace_id) {
            return;
        }
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
        if Self::is_private_workspace(workspace_id)
            || self.workspaces.len() <= 1
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
        if Self::is_private_workspace(workspace_id) {
            tracing::info!("Bookmarks are disabled in the incognito space");
            return;
        }
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
        if Self::is_private_workspace(original_workspace_id)
            || Self::is_private_workspace(workspace_id)
        {
            return;
        }
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
        if Self::is_private_workspace(workspace_id) {
            return;
        }
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
    fn spawn_browser(&mut self, tab_id: u64, url: &str, hidden: bool) {
        // Private tabs receive CEF's in-memory request context. Keeping it on
        // the UI thread avoids sharing a request context across FFI handlers.
        let mut private_context = if self.is_private_tab(tab_id) {
            self.private_context()
        } else {
            None
        };
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
        // Keep the embedded child view on Alloy style. Chrome style is the
        // CEF 156 default, but its display-capture flow can capture the whole
        // desktop without offering the screen/window picker. Alloy style is
        // supported with a client-provided parent view and preserves native
        // child-window compositing plus CEF's media callbacks.
        window_info.runtime_style = cef::RuntimeStyle::ALLOY;

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
            private_context.as_mut(),
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
        self.resolve_ready_auto_media_permissions();
    }

    /// Consume a Cmd/Ctrl+F request forwarded from the CEF web view.
    pub fn consume_find_request(&mut self) -> bool {
        std::mem::take(&mut self.find_requested)
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

    /// Consume a Cmd/Ctrl+Shift+C request forwarded from the CEF web view.
    pub fn consume_copy_url_request(&mut self) -> bool {
        std::mem::take(&mut self.copy_url_requested)
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
            BrowserEvent::MediaAccessPermissionRequested {
                request_id,
                tab_id,
                origin,
                permissions,
                callback,
                prompt_id,
            } => {
                let Some(private) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| Self::is_private_workspace(tab.workspace_id))
                else {
                    // A prompt from a browser whose tab metadata is gone must
                    // never open an orphan dialog. Dropping our callback lets
                    // CEF dispose the request with its browser; invoking DENY
                    // here could run against an already torn-down prompt.
                    drop(callback);
                    return;
                };
                let origin = normalize_permission_origin(&origin);
                let saved = if private {
                    &self.private_site_permissions
                } else {
                    &self.saved_site_permissions
                };
                let requested = MediaAccessPermissions::from_bits(permissions);
                let auto_decision = if requested.has_unrecognized_bits() {
                    None
                } else {
                    remembered_permission_decision(saved, &origin, permissions)
                };
                // A remembered decision resolves the CEF callback directly, but
                // web apps also inspect Chromium's own content-setting state.
                // Sync it here so a restart does not leave Google Meet believing
                // camera/microphone access is still in the "prompt" state.
                if !private {
                    if let Some(allowed) = auto_decision {
                        let context = self.request_context_for_tab(tab_id);
                        sync_media_content_settings(
                            context.as_ref(),
                            &origin,
                            permissions,
                            allowed,
                        );
                    }
                }
                // Meet requests audio a few milliseconds before video. Let a
                // remembered video grant win that race so Chromium starts the
                // camera first; audio can then start alongside the live camera.
                let auto_resolve = (auto_decision == Some(true)
                    && permissions == MediaAccessPermissions::AUDIO)
                    .then(|| (Instant::now() + AUDIO_AUTO_RESOLVE_DELAY, true));
                self.pending_media_permissions.insert(
                    request_id,
                    PendingMediaPermission {
                        tab_id,
                        origin,
                        permissions,
                        private,
                        callback,
                        prompt_id,
                        auto_resolve,
                    },
                );
                self.pending_media_permission_queue.push_back(request_id);
                if let Some(allowed) = auto_decision {
                    if auto_resolve.is_none() {
                        // Resolve the whole group: CEF may have delivered the
                        // content-setting prompt and media-access callback as
                        // two events for the same getUserMedia call.
                        self.resolve_media_permission(request_id, allowed, false);
                    }
                }
            }
            BrowserEvent::PermissionPromptDismissed { tab_id, prompt_id } => {
                // CEF has already completed a prompt when it reports dismissal
                // (either with our result or with IGNORE after a navigation).
                // Clear the matching Kasumi prompt without calling the callback
                // a second time.
                let stale: Vec<u64> = self
                    .pending_media_permissions
                    .iter()
                    .filter(|(_, pending)| {
                        pending.tab_id == tab_id && pending.prompt_id == Some(prompt_id)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for id in stale {
                    if take_pending_permission(
                        &mut self.pending_media_permissions,
                        &mut self.pending_media_permission_queue,
                        id,
                    )
                    .is_some()
                    {
                        tracing::info!(
                            "CEF dismissed media permission prompt {prompt_id} for tab {tab_id}"
                        );
                    }
                }
            }
            BrowserEvent::DisplayMediaPickerRequested { tab_id, token } => {
                if self.pending_display_media_pickers.contains_key(&token)
                    || !self.tabs.iter().any(|tab| tab.id == tab_id)
                {
                    return;
                }
                let sources = display_capture::available_sources();
                tracing::info!(
                    "Display media request from tab {} found {} source(s)",
                    tab_id,
                    sources.len()
                );
                if sources.is_empty() {
                    self.resolve_display_media_picker(token, None);
                    return;
                }
                self.pending_display_media_pickers.insert(
                    token.clone(),
                    PendingDisplayMediaPicker {
                        tab_id,
                        picker: DisplayMediaPicker {
                            tab_id,
                            token: token.clone(),
                            sources,
                        },
                    },
                );
                self.pending_display_media_queue.push_back(token);
            }
            BrowserEvent::NotificationPermissionRequested {
                request_id,
                tab_id,
                origin,
                token,
            } => {
                // A request from a tab that closed while the console message
                // was in flight has no page left to answer.
                let Some(private) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| Self::is_private_workspace(tab.workspace_id))
                else {
                    return;
                };
                let origin = normalize_permission_origin(&origin);
                if origin.is_empty() || origin == "null" || token.is_empty() {
                    return;
                }
                let saved = if private {
                    &self.private_site_permissions
                } else {
                    &self.saved_site_permissions
                };
                let decision = saved
                    .get(&(origin.clone(), "notifications".to_string()))
                    .copied();
                if let Some(allowed) = decision {
                    let text = if allowed { "granted" } else { "denied" };
                    self.evaluate_notification_permission(tab_id, &token, text);
                    return;
                }
                self.pending_notification_permissions.insert(
                    request_id,
                    PendingNotificationPermission {
                        tab_id,
                        origin,
                        token,
                        private,
                    },
                );
                self.pending_notification_permission_queue
                    .push_back(request_id);
            }
            BrowserEvent::WebNotificationShown(mut notification) => {
                let Some(private) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == notification.tab_id)
                    .map(|tab| Self::is_private_workspace(tab.workspace_id))
                else {
                    return;
                };
                let origin = normalize_permission_origin(&notification.origin);
                let saved = if private {
                    &self.private_site_permissions
                } else {
                    &self.saved_site_permissions
                };
                if origin.is_empty()
                    || origin == "null"
                    || saved.get(&(origin.clone(), "notifications".to_string())) != Some(&true)
                {
                    tracing::warn!(
                        "Dropping notification for tab {}: origin has no notification grant",
                        notification.tab_id
                    );
                    return;
                }
                notification.origin = origin;
                // A tag replaces the previous notification from the same tab,
                // matching the Web Notification API's replacement semantics.
                if !notification.tag.is_empty() {
                    self.web_notifications.retain(|existing| {
                        !(existing.tab_id == notification.tab_id
                            && existing.tag == notification.tag)
                    });
                }
                self.web_notifications.push_back(notification);
            }
            BrowserEvent::WebNotificationClosed { id, .. } => {
                const MAX_CLOSED_IDS: usize = 64;
                if self.closed_web_notifications.len() < MAX_CLOSED_IDS {
                    self.closed_web_notifications.push(id);
                }
            }
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
                if let Some(pending) = self.pending_find_on_wake.remove(&tab_id) {
                    self.find_in_page(
                        tab_id,
                        &pending.query,
                        true,
                        pending.match_case,
                        false,
                        pending.options,
                    );
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
                let private = self.is_private_tab(tab_id);
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.title = title.clone();
                    if !private {
                        if let Some(ref storage) = self.storage {
                            storage.record_visit(tab.url.clone(), title);
                        }
                    }
                }
                self.sync_session();
            }
            BrowserEvent::AddressChange { tab_id, url } => {
                // A committed navigation invalidates any prompt from the page
                // that was showing before it. Drop the callback here instead of
                // running it while Chromium is tearing down the prompt.
                self.discard_media_permissions_for_tab(tab_id);
                let committed_initial = !is_blank_navigation_url(&url)
                    && self.pending_navigation_checks.remove(&tab_id).is_some();
                if committed_initial {
                    tracing::info!("Initial navigation committed for tab {tab_id}: {url}");
                }
                let private = self.is_private_tab(tab_id);
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
                    if !private {
                        if let Some(ref storage) = self.storage {
                            storage.record_visit(url, tab.title.clone());
                        }
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
            BrowserEvent::FindResult {
                tab_id,
                identifier: _,
                count,
                active_match_ordinal,
                final_update,
            } => {
                // The rich shim owns snippets; CEF remains authoritative for the
                // fast substring count and active match ordinal. `identifier`
                // is CEF's own ordinal and must not replace our shim request id,
                // otherwise late snippet payloads would look stale.
                let Some(current) = self.find_results.get(&tab_id).copied() else {
                    return;
                };
                if current.advanced || current.pending {
                    return;
                }
                self.find_results.insert(
                    tab_id,
                    FindResultSnapshot {
                        count,
                        active_ordinal: active_match_ordinal,
                        final_update,
                        request_id: current.request_id,
                        advanced: false,
                        pending: false,
                    },
                );
            }
            BrowserEvent::FindResultRich {
                tab_id,
                request_id,
                count,
                active_ordinal,
                snippets,
                error,
                cleared,
            } => {
                let current = self.find_results.get(&tab_id).copied();
                if cleared {
                    if current.is_some_and(|result| result.request_id == request_id) {
                        self.find_results.remove(&tab_id);
                        self.find_snippets.remove(&tab_id);
                        self.find_errors.remove(&tab_id);
                    }
                    return;
                }
                if !current.is_some_and(|result| result.request_id == request_id) {
                    // Late payload from a superseded query. It can still carry
                    // an active-match update for the current query, so only the
                    // count/snippet payload is dropped here.
                    if active_ordinal > 0 && snippets.is_empty() {
                        if let Some(result) = self.find_results.get_mut(&tab_id) {
                            result.active_ordinal = active_ordinal;
                        }
                    }
                    return;
                }
                if !snippets.is_empty() {
                    self.find_snippets.insert(tab_id, snippets);
                }
                if !error.is_empty() {
                    self.find_errors.insert(tab_id, error);
                    self.find_results.remove(&tab_id);
                    self.find_snippets.remove(&tab_id);
                    return;
                }
                self.find_errors.remove(&tab_id);
                self.find_results.insert(
                    tab_id,
                    FindResultSnapshot {
                        count,
                        active_ordinal,
                        final_update: true,
                        request_id,
                        advanced: current.is_some_and(|result| result.advanced),
                        pending: false,
                    },
                );
            }
            BrowserEvent::BrowserClosed { tab_id } => {
                self.pending_navigation_checks.remove(&tab_id);
                self.discard_media_permissions_for_tab(tab_id);
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
                BrowserShortcut::NewPrivateWindow => {
                    self.new_private_window();
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
                BrowserShortcut::FindInPage => {
                    self.find_requested = true;
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
                BrowserShortcut::CopyUrl => {
                    // The shell owns the egui clipboard, so hand the request to
                    // the same deferred path used by the winit shortcut.
                    self.copy_url_requested = true;
                }
                BrowserShortcut::Quit => {
                    // The shell exits the event loop from its own shortcut
                    // path; TabManager must never call process::exit().
                }
            },
            BrowserEvent::ViewSource { tab_id, url } => {
                if url.is_empty() {
                    return;
                }
                let source_url = if url.starts_with("view-source:") {
                    url
                } else {
                    format!("view-source:{}", url)
                };
                let workspace_id = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| tab.workspace_id)
                    .unwrap_or(self.active_workspace_id);
                if Self::is_private_workspace(workspace_id) {
                    self.ensure_private_workspace();
                }
                let existing = self
                    .tabs
                    .iter()
                    .find(|tab| tab.workspace_id == workspace_id && tab.url == source_url)
                    .map(|tab| tab.id);
                if let Some(id) = existing {
                    self.focus_tab(id);
                } else {
                    self.insert_tab_in_workspace(
                        Some(source_url),
                        None,
                        None,
                        true,
                        false,
                        workspace_id,
                    );
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
            BrowserEvent::OpenUrlFromTab {
                tab_id,
                url,
                background,
            } => {
                if url.is_empty() {
                    return;
                }
                // Popups and middle-clicks inherit the originating tab's
                // workspace, so an incognito page cannot escape to a normal
                // (persisted) browser context.
                let source_workspace = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| tab.workspace_id)
                    .unwrap_or(self.active_workspace_id);
                if Self::is_private_workspace(source_workspace) {
                    self.ensure_private_workspace();
                }
                if background {
                    self.create_background_tab_in_workspace(url, source_workspace);
                } else {
                    self.insert_tab_in_workspace(
                        Some(url),
                        None,
                        None,
                        true,
                        false,
                        source_workspace,
                    );
                }
            }
        }
    }

    /// Execute a small find script in a live tab. A missing frame means the tab
    /// hibernated or navigated while the user was typing; the request is simply
    /// dropped instead of touching CEF from the wrong thread.
    fn execute_find_script(&self, tab_id: u64, script: &str) -> bool {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.browser.as_ref())
            .and_then(|browser| browser.main_frame())
            .map(|frame| {
                frame.execute_java_script(Some(&CefString::from(script)), None, 0);
            })
            .is_some()
    }

    /// Run find on one tab or, for the cross-tab scope, every live tab.
    ///
    /// Plain substring queries use the fast native CEF matcher and the rich
    /// shim only for snippets. Regex, whole-word, and multi-term queries use the
    /// `TreeWalker`/`CSS.highlights` path exclusively. Hibernated tabs have no
    /// `BrowserHost`, so they are skipped here; clicking one in the results list
    /// wakes it and re-runs the query.
    pub fn find_in_page(
        &mut self,
        tab_id: u64,
        query: &str,
        forward: bool,
        match_case: bool,
        find_next: bool,
        options: FindOptions,
    ) {
        let targets: Vec<u64> = if options.all_tabs {
            // Include hibernated tabs so the query can wait for them to wake;
            // `find_next` and the script executor silently skip tabs with no
            // live frame.
            self.tabs.iter().map(|tab| tab.id).collect()
        } else {
            vec![tab_id]
        };
        if targets.is_empty() {
            return;
        }

        if query.is_empty() {
            for target in targets {
                self.stop_finding(target, true);
                self.pending_find_on_wake.remove(&target);
            }
            return;
        }

        // Move between existing matches without rebuilding the DOM ranges.
        if find_next {
            for target in targets {
                if !options.is_advanced() {
                    let Some(tab) = self.tabs.iter().find(|tab| tab.id == target) else {
                        continue;
                    };
                    if let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) {
                        let text = CefString::from(query);
                        host.find(Some(&text), i32::from(forward), i32::from(match_case), 1);
                    }
                }
                let script = find::next_script(forward);
                if !self.execute_find_script(target, &script) {
                    tracing::debug!("Find-next skipped for tab {target}: no live frame");
                }
            }
            return;
        }

        let request_id = self.next_find_request_id;
        self.next_find_request_id = self.next_find_request_id.wrapping_add(1).max(1);

        for target in targets {
            let has_browser = self
                .tabs
                .iter()
                .find(|tab| tab.id == target)
                .is_some_and(|tab| tab.browser.is_some());
            if !has_browser {
                self.pending_find_on_wake.insert(
                    target,
                    PendingFindRequest {
                        query: query.to_string(),
                        match_case,
                        // Waking this tab is a single-tab replay, not a new
                        // cross-tab sweep that would restart every live search.
                        options: FindOptions {
                            all_tabs: false,
                            ..options
                        },
                    },
                );
                self.find_results.insert(
                    target,
                    FindResultSnapshot {
                        count: -1,
                        active_ordinal: -1,
                        final_update: false,
                        request_id,
                        advanced: options.is_advanced(),
                        pending: true,
                    },
                );
                self.find_snippets.remove(&target);
                self.find_errors.remove(&target);
                continue;
            }

            self.find_results.insert(
                target,
                FindResultSnapshot {
                    count: 0,
                    active_ordinal: -1,
                    final_update: false,
                    request_id,
                    advanced: options.is_advanced(),
                    pending: false,
                },
            );
            self.find_snippets.remove(&target);
            self.find_errors.remove(&target);

            if !options.is_advanced() {
                let Some(tab) = self.tabs.iter().find(|tab| tab.id == target) else {
                    continue;
                };
                if let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) {
                    let text = CefString::from(query);
                    host.find(Some(&text), i32::from(forward), i32::from(match_case), 0);
                }
            }

            let script = find::run_script(
                request_id,
                query,
                match_case,
                options.regex,
                options.whole_word,
                options.multi_term,
            );
            if !self.execute_find_script(target, &script) {
                tracing::debug!("Rich find skipped for tab {target}: no live frame");
            }
        }
    }

    /// Focus one rich match ordinal, scrolling it into view.
    pub fn jump_to_find_match(&self, tab_id: u64, ordinal: i32) {
        let script = find::focus_script(ordinal);
        if !self.execute_find_script(tab_id, &script) {
            tracing::debug!("Find jump skipped for tab {tab_id}: no live frame");
        }
    }

    /// Clear find highlights (or just the active selection) on a tab.
    pub fn stop_finding(&mut self, tab_id: u64, clear_selection: bool) {
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) {
            if let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) {
                host.stop_finding(i32::from(clear_selection));
            }
        }
        if clear_selection {
            let script = find::clear_script();
            self.execute_find_script(tab_id, &script);
            self.pending_find_on_wake.remove(&tab_id);
            self.find_results.remove(&tab_id);
            self.find_snippets.remove(&tab_id);
            self.find_errors.remove(&tab_id);
        }
    }

    /// Clear every find session when the shared find bar is dismissed.
    pub fn stop_finding_all(&mut self, clear_selection: bool) {
        let ids: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        for tab_id in ids {
            self.stop_finding(tab_id, clear_selection);
        }
    }

    /// Dispatch user actions from egui chrome or keyboard shortcuts.
    pub fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::NewTab { url } => {
                self.create_tab(url, true);
            }
            UiAction::NewPrivateWindow => self.new_private_window(),
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
                    .filter(|tab| {
                        tab.workspace_id == workspace_id && tab.id != tab_id && !tab.pinned
                    })
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
                if !self.is_private_download_id(download_id) {
                    if let Some(ref storage) = self.storage {
                        storage.remove_download(download_id);
                    }
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
                let private = self.is_private_download_id(download_id);
                if let Some(info) = self.download_snapshot(download_id) {
                    if info.engine == DownloadEngine::Parallel {
                        self.start_native_download(info);
                    } else if private {
                        // Never replay an incognito URL into the persisted space:
                        // recreate the ephemeral workspace if it was closed.
                        self.ensure_private_workspace();
                        self.insert_tab_in_workspace(
                            Some(info.url),
                            None,
                            None,
                            true,
                            false,
                            Workspace::PRIVATE_ID,
                        );
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
            UiAction::ResolveMediaAccessPermission {
                request_id,
                granted,
                persist,
            } => self.resolve_media_permission(request_id, granted, persist),
            UiAction::ResolveDisplayMediaPicker { token, device_id } => {
                self.resolve_display_media_picker(token, device_id);
            }
            UiAction::ResolveNotificationPermission {
                request_id,
                granted,
                persist,
            } => self.resolve_notification_permission(request_id, granted, persist),
            UiAction::ActivateWebNotification {
                tab_id,
                notification_id,
            } => {
                self.focus_tab(tab_id);
                self.dispatch_web_notification_event(tab_id, &notification_id, "click");
            }
            UiAction::DismissWebNotification {
                tab_id,
                notification_id,
            } => {
                self.dispatch_web_notification_event(tab_id, &notification_id, "close");
            }
            UiAction::FindInPage {
                tab_id,
                query,
                forward,
                match_case,
                find_next,
                options,
            } => self.find_in_page(tab_id, &query, forward, match_case, find_next, options),
            UiAction::JumpToFindMatch { tab_id, ordinal } => {
                self.jump_to_find_match(tab_id, ordinal);
            }
            UiAction::StopFinding {
                tab_id,
                clear_selection,
            } => self.stop_finding(tab_id, clear_selection),
            UiAction::StopFindingAll { clear_selection } => {
                self.stop_finding_all(clear_selection);
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
                let snapshot = self
                    .downloads
                    .iter_mut()
                    .find(|item| item.id == download_id)
                    .filter(|info| info.state == DownloadState::Completed)
                    .map(|info| {
                        info.state = DownloadState::Interrupted;
                        info.current_speed = 0;
                        info.updated_at = Self::now_unix_secs();
                        info.clone()
                    });
                if let Some(snapshot) = snapshot {
                    self.persist_download(snapshot);
                }
            }
        }

        // 1. Refresh RAM every RAM_CHECK_INTERVAL (5s)
        if now.duration_since(self.last_ram_check) >= RAM_CHECK_INTERVAL {
            self.last_ram_check = now;
            if let Some(system) = self.sys.as_mut() {
                system.refresh_memory();
                self.ram_used_bytes = system.used_memory();
                self.ram_total_bytes = system.total_memory();

                // Refresh current process RAM
                if let Ok(pid) = sysinfo::get_current_pid() {
                    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
                    if let Some(proc) = system.process(pid) {
                        self.ram_process_mb = proc.memory() / (1024 * 1024);
                    }
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
                find_count: self
                    .find_results
                    .get(&t.id)
                    .map(|result| result.count)
                    .unwrap_or(-1),
                find_active_ordinal: self
                    .find_results
                    .get(&t.id)
                    .map(|result| result.active_ordinal)
                    .unwrap_or(-1),
                find_pending: self
                    .find_results
                    .get(&t.id)
                    .is_some_and(|result| result.pending),
                find_snippets: self.find_snippets.get(&t.id).cloned().unwrap_or_default(),
                find_error: self.find_errors.get(&t.id).cloned().unwrap_or_default(),
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

        // A live media prompt always wins; notification prompts queue behind it
        // so two modal requests can never fight over the shared overlay.
        state.permission_prompt = if let Some((request_id, pending)) = self
            .active_media_permission_request_id()
            .and_then(|id| self.pending_media_permissions.get(&id).map(|p| (id, p)))
        {
            Some(PermissionPrompt {
                kind: PermissionPromptKind::Media,
                request_id,
                tab_id: pending.tab_id,
                origin: pending.origin.clone(),
                permissions: pending.permissions,
                label: MediaAccessPermissions::from_bits(pending.permissions).label(),
                private: pending.private,
            })
        } else if let Some((request_id, pending)) = self
            .active_notification_permission_request_id()
            .and_then(|id| {
                self.pending_notification_permissions
                    .get(&id)
                    .map(|p| (id, p))
            })
        {
            Some(PermissionPrompt {
                kind: PermissionPromptKind::Notifications,
                request_id,
                tab_id: pending.tab_id,
                origin: pending.origin.clone(),
                permissions: 0,
                label: "notifications".to_string(),
                private: pending.private,
            })
        } else {
            None
        };
        state.display_media_picker = self
            .active_display_media_token()
            .and_then(|token| self.pending_display_media_pickers.get(&token))
            .map(|pending| pending.picker.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        closed_tab_from, fallback_favicon_url, idle_timeout_for_pressure, insertion_index,
        is_blank_navigation_url, is_critical_pressure, needs_initial_navigation,
        next_navigation_retry_delay, normalize_workspace_selection, pinned_insert_index,
        preferred_favicon_url, ram_pressure, record_media_permission,
        record_site_permission_decision, remembered_permission_decision, restore_insert_index,
        should_hibernate_tab, should_sync_omnibox, take_pending_permission, BrowserSpawnMode,
        PendingMediaPermission, Tab, TabManager, TabState,
    };
    use crate::config::{
        BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon, WorkspaceStartupBehavior,
        TAB_IDLE_TIMEOUT_NORMAL, TAB_IDLE_TIMEOUT_PRESSURE,
    };
    use crate::events::{
        BrowserEvent, DownloadEngine, DownloadInfo, DownloadNotificationKind, DownloadState,
        MediaAccessPermissions, MediaPermissionCallback, TestPermissionCallback, UiAction,
        WebNotification,
    };
    use crate::storage::db::{BookmarkItem, DbCommand, SessionTabSnapshot, StorageHandle};
    use crate::ui::chrome::{ChromeState, PermissionPromptKind};
    use crate::ui::platform_view::ContentRect;
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::UnboundedReceiver;

    #[test]
    fn pending_permission_is_taken_once_and_removed_from_queue() {
        let mut pending = HashMap::new();
        pending.insert(9, "callback");
        let mut queue = VecDeque::from(vec![8, 9, 10]);

        assert_eq!(
            take_pending_permission(&mut pending, &mut queue, 9),
            Some("callback")
        );
        assert_eq!(queue, VecDeque::from(vec![8, 10]));
        assert_eq!(take_pending_permission(&mut pending, &mut queue, 9), None);
    }

    fn permission_test_manager() -> TabManager {
        let (tx, rx) = crossbeam_channel::unbounded();
        TabManager::new(
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
        )
    }

    fn add_permission_test_tab(manager: &mut TabManager, id: u64) {
        manager.tabs.push(Tab {
            id,
            workspace_id: Workspace::DEFAULT_ID,
            url: "https://meet.example".to_string(),
            title: "Meet".to_string(),
            is_loading: false,
            can_go_back: false,
            can_go_forward: false,
            blocked_requests: 0,
            pinned: false,
            state: TabState::Active,
            browser: None,
            favicon: None,
        });
    }

    #[test]
    fn notification_permission_requests_queue_a_native_prompt() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 7);
        manager
            .event_sender
            .send(BrowserEvent::NotificationPermissionRequested {
                request_id: 11,
                tab_id: 7,
                origin: "https://chat.example/".to_string(),
                token: "token-1".to_string(),
            })
            .expect("send notification permission");
        manager.pump_events();

        assert_eq!(manager.pending_notification_permissions.len(), 1);
        assert_eq!(
            manager.active_notification_permission_request_id(),
            Some(11)
        );
        let mut state = ChromeState::default();
        manager.update_chrome_state(&mut state);
        let prompt = state.permission_prompt.expect("notification prompt");
        assert_eq!(prompt.kind, PermissionPromptKind::Notifications);
        assert_eq!(prompt.origin, "https://chat.example");
        assert_eq!(prompt.label, "notifications");

        manager.handle_ui_action(UiAction::ResolveNotificationPermission {
            request_id: 11,
            granted: true,
            persist: true,
        });
        assert!(manager.pending_notification_permissions.is_empty());
        assert_eq!(
            manager.saved_site_permissions.get(&(
                "https://chat.example".to_string(),
                "notifications".to_string()
            )),
            Some(&true)
        );
    }

    #[test]
    fn saved_notification_permission_resolves_without_a_prompt() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 7);
        manager.saved_site_permissions.insert(
            (
                "https://chat.example".to_string(),
                "notifications".to_string(),
            ),
            true,
        );
        manager
            .event_sender
            .send(BrowserEvent::NotificationPermissionRequested {
                request_id: 12,
                tab_id: 7,
                origin: "https://chat.example".to_string(),
                token: "token-2".to_string(),
            })
            .expect("send notification permission");
        manager.pump_events();

        assert!(manager.pending_notification_permissions.is_empty());
    }

    #[test]
    fn notifications_without_a_saved_grant_are_dropped() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 7);
        let notification = WebNotification {
            id: "kn-1".to_string(),
            tab_id: 7,
            origin: "https://chat.example".to_string(),
            title: "Build finished".to_string(),
            body: String::new(),
            tag: String::new(),
            require_interaction: false,
        };
        manager
            .event_sender
            .send(BrowserEvent::WebNotificationShown(notification.clone()))
            .expect("send notification");
        manager.pump_events();
        assert!(manager.consume_web_notifications().is_empty());

        manager.saved_site_permissions.insert(
            (
                "https://chat.example".to_string(),
                "notifications".to_string(),
            ),
            true,
        );
        manager
            .event_sender
            .send(BrowserEvent::WebNotificationShown(notification))
            .expect("send granted notification");
        manager.pump_events();
        let shown = manager.consume_web_notifications();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].origin, "https://chat.example");
    }

    #[test]
    fn notification_permission_record_respects_private_spaces() {
        let mut saved = BTreeMap::new();
        let mut private = BTreeMap::new();
        record_site_permission_decision(
            &mut saved,
            &mut private,
            None,
            "https://chat.example",
            "notifications",
            true,
            true,
        );
        assert!(saved.is_empty());
        assert_eq!(
            private.get(&(
                "https://chat.example".to_string(),
                "notifications".to_string()
            )),
            Some(&true)
        );
    }

    fn add_pending_permission(
        manager: &mut TabManager,
        request_id: u64,
        tab_id: u64,
    ) -> TestPermissionCallback {
        add_pending_permission_with(
            manager,
            request_id,
            tab_id,
            MediaAccessPermissions::AUDIO | MediaAccessPermissions::VIDEO,
            None,
        )
    }

    fn add_pending_permission_with(
        manager: &mut TabManager,
        request_id: u64,
        tab_id: u64,
        permissions: u32,
        prompt_id: Option<u64>,
    ) -> TestPermissionCallback {
        let callback = TestPermissionCallback::default();
        manager.pending_media_permissions.insert(
            request_id,
            PendingMediaPermission {
                tab_id,
                origin: "https://meet.example".to_string(),
                permissions,
                private: false,
                callback: MediaPermissionCallback::Test(callback.clone()),
                prompt_id,
                auto_resolve: None,
            },
        );
        manager.pending_media_permission_queue.push_back(request_id);
        callback
    }

    fn assert_permission_discarded(manager: &TabManager, callback: &TestPermissionCallback) {
        assert_eq!(callback.calls(), 0);
        assert!(manager.pending_media_permissions.is_empty());
        assert!(manager.pending_media_permission_queue.is_empty());
    }

    #[test]
    fn committed_navigation_discards_permission_callback() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let callback = add_pending_permission(&mut manager, 10, 1);

        manager.handle_browser_event(BrowserEvent::AddressChange {
            tab_id: 1,
            url: "https://meet.example/call".to_string(),
        });

        assert_permission_discarded(&manager, &callback);
    }

    #[test]
    fn browser_closed_discards_permission_callback() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let callback = add_pending_permission(&mut manager, 11, 1);

        manager.handle_browser_event(BrowserEvent::BrowserClosed { tab_id: 1 });

        assert_permission_discarded(&manager, &callback);
    }

    #[test]
    fn hibernation_discards_permission_callback() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let callback = add_pending_permission(&mut manager, 12, 1);

        manager.hibernate_tab(1);

        assert_permission_discarded(&manager, &callback);
    }

    #[test]
    fn shutdown_and_tab_close_discard_permission_callbacks() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        add_permission_test_tab(&mut manager, 2);
        let first = add_pending_permission(&mut manager, 13, 1);
        let second = add_pending_permission(&mut manager, 14, 2);

        manager.close_tab(1);
        assert_eq!(first.calls(), 0);
        assert!(manager
            .pending_media_permissions
            .values()
            .all(|pending| pending.tab_id != 1));

        manager.close_all_browsers();
        assert_eq!(second.calls(), 0);
        assert!(manager.pending_media_permissions.is_empty());
        assert!(manager.pending_media_permission_queue.is_empty());
    }

    #[test]
    fn explicit_permission_resolution_invokes_callback_once() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let callback = add_pending_permission(&mut manager, 15, 1);

        manager.resolve_media_permission(15, true, true);
        assert_eq!(
            callback.granted.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(callback.calls(), 1);

        manager.resolve_media_permission(15, false, true);
        assert_eq!(callback.calls(), 1);
        assert!(manager.pending_media_permissions.is_empty());
    }

    #[test]
    fn one_decision_resolves_the_matching_permission_group() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let content_prompt = add_pending_permission_with(
            &mut manager,
            16,
            1,
            MediaAccessPermissions::AUDIO | MediaAccessPermissions::VIDEO,
            Some(7),
        );
        let media_access = add_pending_permission_with(
            &mut manager,
            17,
            1,
            MediaAccessPermissions::AUDIO | MediaAccessPermissions::VIDEO,
            None,
        );
        let unrelated =
            add_pending_permission_with(&mut manager, 18, 1, MediaAccessPermissions::VIDEO, None);

        manager.resolve_media_permission(16, true, true);

        assert_eq!(
            content_prompt
                .granted
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            media_access
                .granted
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(unrelated.calls(), 0);
        assert_eq!(
            manager.pending_media_permission_queue,
            VecDeque::from(vec![18])
        );
    }

    #[test]
    fn remembered_audio_waits_for_a_video_request_before_resolving() {
        let mut manager = permission_test_manager();
        add_permission_test_tab(&mut manager, 1);
        let audio =
            add_pending_permission_with(&mut manager, 19, 1, MediaAccessPermissions::AUDIO, None);
        manager
            .pending_media_permissions
            .get_mut(&19)
            .expect("audio request")
            .auto_resolve = Some((Instant::now() + Duration::from_secs(60), true));

        manager.resolve_ready_auto_media_permissions();
        assert_eq!(audio.calls(), 0);
        assert_eq!(manager.active_media_permission_request_id(), None);

        manager
            .pending_media_permissions
            .get_mut(&19)
            .expect("audio request")
            .auto_resolve = Some((Instant::now() - Duration::from_millis(1), true));
        manager.resolve_ready_auto_media_permissions();
        assert_eq!(audio.granted.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(manager.pending_media_permissions.is_empty());
    }

    #[test]
    fn saved_permission_decision_requires_a_uniform_answer() {
        let mut saved = BTreeMap::new();
        saved.insert(
            ("https://example.com".to_string(), "camera".to_string()),
            true,
        );
        saved.insert(
            ("https://example.com".to_string(), "microphone".to_string()),
            true,
        );

        assert_eq!(
            remembered_permission_decision(&saved, "https://example.com", 3),
            Some(true)
        );
        assert_eq!(
            remembered_permission_decision(&saved, "https://other.example", 3),
            None
        );
        assert_eq!(
            remembered_permission_decision(&saved, "https://example.com", 0),
            None
        );

        saved.insert(
            ("https://example.com".to_string(), "microphone".to_string()),
            false,
        );
        assert_eq!(
            remembered_permission_decision(&saved, "https://example.com", 3),
            None
        );
    }

    #[test]
    fn incognito_media_permissions_bypass_sqlite() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let storage = StorageHandle::from_sender(tx);
        let mut saved = BTreeMap::new();
        let mut private = BTreeMap::new();

        record_media_permission(
            &mut saved,
            &mut private,
            Some(&storage),
            "https://meet.example",
            3,
            true,
            true,
        );
        assert!(saved.is_empty());
        assert_eq!(private.len(), 2);
        assert!(rx.try_recv().is_err());

        record_media_permission(
            &mut saved,
            &mut private,
            Some(&storage),
            "https://meet.example",
            3,
            false,
            true,
        );
        assert_eq!(saved.len(), 2);
        let mut written = Vec::new();
        for _ in 0..2 {
            match rx.try_recv().expect("site permission command") {
                DbCommand::SaveSitePermission {
                    origin,
                    permission,
                    allowed,
                } => {
                    assert_eq!(origin, "https://meet.example");
                    assert!(allowed);
                    written.push(permission);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
        assert_eq!(written, vec!["microphone", "camera"]);
    }

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
    fn close_other_tabs_never_closes_pinned_tabs() {
        let (mut manager, mut db_rx, active_id) = test_manager_with_storage();
        let pinned_id = manager.create_tab(Some("https://pinned.example".to_string()), false);
        manager.toggle_pin_tab(pinned_id);
        let other_id = manager.create_tab(Some("https://other.example".to_string()), false);
        let _ = drain_db_commands(&mut db_rx);

        manager.handle_ui_action(UiAction::CloseOtherTabs { tab_id: active_id });

        let mut remaining: Vec<u64> = manager.tabs.iter().map(|tab| tab.id).collect();
        remaining.sort_unstable();
        let mut expected = vec![active_id, pinned_id];
        expected.sort_unstable();
        assert_eq!(remaining, expected, "pinned tab must survive close-other");
        assert!(!remaining.contains(&other_id));

        // A pinned source tab may still close the other unpinned tabs.
        manager.handle_ui_action(UiAction::CloseOtherTabs { tab_id: pinned_id });
        let remaining: Vec<u64> = manager.tabs.iter().map(|tab| tab.id).collect();
        assert_eq!(remaining, vec![pinned_id]);
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

    type DbReceiver = UnboundedReceiver<DbCommand>;

    fn test_manager_with_storage() -> (TabManager, DbReceiver, u64) {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (db_tx, mut db_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = TabManager::new(
            event_tx,
            event_rx,
            std::ptr::null_mut(),
            ContentRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            },
            Some(StorageHandle::from_sender(db_tx)),
            &BrowserPreferences::default(),
            vec![Workspace::default_workspace()],
            vec![],
        );
        let normal_tab_id = manager.create_tab(Some("https://personal.example".to_string()), true);
        let _ = drain_db_commands(&mut db_rx);
        (manager, db_rx, normal_tab_id)
    }

    fn drain_db_commands(receiver: &mut DbReceiver) -> Vec<DbCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            commands.push(command);
        }
        commands
    }

    fn private_tab_id(manager: &TabManager) -> u64 {
        manager
            .tabs
            .iter()
            .find(|tab| Workspace::is_private_id(tab.workspace_id))
            .map(|tab| tab.id)
            .expect("incognito tab should exist")
    }

    fn assert_public_snapshots(commands: &[DbCommand]) {
        for command in commands {
            match command {
                DbCommand::SaveSession { tabs } => assert!(
                    tabs.iter()
                        .all(|tab| !Workspace::is_private_id(tab.workspace_id)),
                    "private tab reached the session snapshot"
                ),
                DbCommand::SaveWorkspaces { workspaces } => assert!(
                    workspaces
                        .iter()
                        .all(|workspace| !Workspace::is_private_id(workspace.id)),
                    "private workspace reached the workspace snapshot"
                ),
                DbCommand::AddBookmark { workspace_id, .. }
                | DbCommand::DeleteBookmark { workspace_id, .. } => assert!(
                    !Workspace::is_private_id(*workspace_id),
                    "private bookmark reached SQLite"
                ),
                DbCommand::SaveBookmark {
                    original_workspace_id,
                    workspace_id,
                    ..
                } => assert!(
                    !Workspace::is_private_id(*original_workspace_id)
                        && !Workspace::is_private_id(*workspace_id),
                    "private bookmark edit reached SQLite"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn incognito_shortcut_reuses_one_private_workspace() {
        let (mut manager, mut db_rx, _normal_tab_id) = test_manager_with_storage();

        manager.new_private_window();
        assert_eq!(manager.active_workspace_id, Workspace::PRIVATE_ID);
        assert_eq!(
            manager
                .workspaces
                .iter()
                .filter(|workspace| Workspace::is_private_id(workspace.id))
                .count(),
            1
        );
        let first_private = private_tab_id(&manager);

        manager.new_private_window();
        assert_eq!(
            manager
                .tabs
                .iter()
                .filter(|tab| Workspace::is_private_id(tab.workspace_id))
                .count(),
            2
        );
        assert_eq!(
            manager
                .workspaces
                .iter()
                .filter(|workspace| Workspace::is_private_id(workspace.id))
                .count(),
            1
        );
        let private_ids: Vec<u64> = manager
            .tabs
            .iter()
            .filter(|tab| Workspace::is_private_id(tab.workspace_id))
            .map(|tab| tab.id)
            .collect();
        assert_eq!(private_ids.len(), 2);
        assert_ne!(private_ids[0], private_ids[1]);
        assert!(private_ids.contains(&first_private));
        assert_public_snapshots(&drain_db_commands(&mut db_rx));
    }

    #[test]
    fn incognito_navigation_and_snapshots_never_emit_history_writes() {
        let (mut manager, mut db_rx, _normal_tab_id) = test_manager_with_storage();
        manager.new_private_window();
        let private_id = private_tab_id(&manager);
        let _ = drain_db_commands(&mut db_rx);

        manager.handle_browser_event(BrowserEvent::AddressChange {
            tab_id: private_id,
            url: "https://secret.example/path".to_string(),
        });
        manager.handle_browser_event(BrowserEvent::TitleChange {
            tab_id: private_id,
            title: "Secret title".to_string(),
        });
        manager.sync_session();

        let commands = drain_db_commands(&mut db_rx);
        assert!(
            commands
                .iter()
                .all(|command| !matches!(command, DbCommand::RecordVisit { .. })),
            "incognito navigation wrote a history visit"
        );
        assert_public_snapshots(&commands);
        assert!(manager
            .session_snapshots()
            .iter()
            .all(|tab| !Workspace::is_private_id(tab.workspace_id)
                && tab.url != "https://secret.example/path"));
        assert!(manager
            .workspace_snapshots()
            .iter()
            .all(|workspace| !Workspace::is_private_id(workspace.id)));
    }

    #[test]
    fn incognito_popups_and_view_source_inherit_private_workspace() {
        let (mut manager, mut db_rx, normal_tab_id) = test_manager_with_storage();
        manager.new_private_window();
        let private_id = private_tab_id(&manager);
        let _ = drain_db_commands(&mut db_rx);

        manager.handle_browser_event(BrowserEvent::OpenUrlFromTab {
            tab_id: private_id,
            url: "https://popup.example".to_string(),
            background: false,
        });
        manager.handle_browser_event(BrowserEvent::OpenUrlFromTab {
            tab_id: private_id,
            url: "https://background.example".to_string(),
            background: true,
        });
        manager.handle_browser_event(BrowserEvent::ViewSource {
            tab_id: private_id,
            url: "https://secret.example".to_string(),
        });

        let private_tabs: Vec<_> = manager
            .tabs
            .iter()
            .filter(|tab| Workspace::is_private_id(tab.workspace_id))
            .collect();
        assert_eq!(private_tabs.len(), 4);
        assert_eq!(
            manager
                .tabs
                .iter()
                .filter(|tab| tab.id == normal_tab_id)
                .count(),
            1
        );
        assert!(private_tabs
            .iter()
            .any(|tab| tab.url == "https://popup.example"));
        assert!(private_tabs
            .iter()
            .any(|tab| tab.url == "https://background.example"));
        assert!(private_tabs
            .iter()
            .any(|tab| tab.url == "view-source:https://secret.example"));

        let commands = drain_db_commands(&mut db_rx);
        assert!(
            commands
                .iter()
                .all(|command| !matches!(command, DbCommand::RecordVisit { .. })),
            "incognito popup wrote a history visit"
        );
        assert_public_snapshots(&commands);
    }

    #[test]
    fn closing_last_incognito_tab_returns_to_the_previous_workspace() {
        let (mut manager, mut db_rx, normal_tab_id) = test_manager_with_storage();
        manager.new_private_window();
        let private_id = private_tab_id(&manager);
        let _ = drain_db_commands(&mut db_rx);

        manager.close_tab(private_id);

        assert!(!manager
            .workspaces
            .iter()
            .any(|workspace| Workspace::is_private_id(workspace.id)));
        assert_eq!(manager.active_workspace_id, Workspace::DEFAULT_ID);
        assert_eq!(manager.active_tab_id, Some(normal_tab_id));
        assert!(manager.private_context.is_none());
        assert_eq!(
            manager.recently_closed.last().map(|tab| tab.workspace_id),
            Some(Workspace::PRIVATE_ID)
        );
        assert_public_snapshots(&drain_db_commands(&mut db_rx));
    }

    #[test]
    fn incognito_bookmarks_and_downloads_bypass_sqlite() {
        let (mut manager, mut db_rx, normal_tab_id) = test_manager_with_storage();
        manager.new_private_window();
        let private_id = private_tab_id(&manager);
        let _ = drain_db_commands(&mut db_rx);

        let mut info = test_download_info_with_id(91, DownloadState::InProgress);
        info.tab_id = private_id;
        manager.record_download(info, None);
        manager.add_bookmark(private_id);
        manager.save_bookmark(
            Workspace::PRIVATE_ID,
            "https://secret.example".to_string(),
            Workspace::PRIVATE_ID,
            "https://secret.example".to_string(),
            "Secret".to_string(),
            Workspace::PRIVATE_NAME.to_string(),
        );
        manager.delete_bookmark(Workspace::PRIVATE_ID, "https://secret.example".to_string());

        let commands = drain_db_commands(&mut db_rx);
        assert!(
            commands.iter().all(|command| !matches!(
                command,
                DbCommand::SaveDownload { .. }
                    | DbCommand::AddBookmark { .. }
                    | DbCommand::SaveBookmark { .. }
                    | DbCommand::DeleteBookmark { .. }
            )),
            "incognito bookmark or download reached SQLite"
        );
        assert_public_snapshots(&commands);

        // The same channel still carries normal writes, so the assertions above
        // prove privacy rather than a disconnected storage handle.
        manager.add_bookmark(normal_tab_id);
        assert!(drain_db_commands(&mut db_rx)
            .iter()
            .any(|command| matches!(command, DbCommand::AddBookmark { .. })));
    }

    #[test]
    fn retrying_a_private_download_reopens_it_incognito() {
        let (mut manager, mut db_rx, _normal_tab_id) = test_manager_with_storage();
        manager.new_private_window();
        let private_id = private_tab_id(&manager);
        let _ = drain_db_commands(&mut db_rx);

        let mut info = test_download_info_with_id(92, DownloadState::Interrupted);
        info.tab_id = private_id;
        manager.record_download(info, None);
        let _ = drain_db_commands(&mut db_rx);

        manager.close_tab(private_id);
        let _ = drain_db_commands(&mut db_rx);
        assert_eq!(manager.active_workspace_id, Workspace::DEFAULT_ID);

        manager.handle_ui_action(UiAction::RetryDownload { download_id: 92 });

        let active_id = manager.active_tab_id.expect("retry opens a tab");
        let active = manager
            .tabs
            .iter()
            .find(|tab| tab.id == active_id)
            .expect("active tab exists");
        assert_eq!(active.workspace_id, Workspace::PRIVATE_ID);
        assert_eq!(active.url, "https://example.com/file.zip");
        assert_public_snapshots(&drain_db_commands(&mut db_rx));
    }

    #[test]
    fn restore_ignores_private_rows_from_a_stale_database() {
        let (mut manager, mut db_rx, _normal_tab_id) = test_manager_with_storage();

        manager.restore_session(vec![SessionTabSnapshot {
            workspace_id: Workspace::PRIVATE_ID,
            url: "https://secret.example".to_string(),
            title: "Secret".to_string(),
            is_active: true,
            is_hibernated: true,
            is_pinned: false,
        }]);

        assert!(manager
            .tabs
            .iter()
            .all(|tab| !Workspace::is_private_id(tab.workspace_id)
                && tab.url != "https://secret.example"));
        assert_ne!(manager.active_workspace_id, Workspace::PRIVATE_ID);
        assert_public_snapshots(&drain_db_commands(&mut db_rx));
    }
}
