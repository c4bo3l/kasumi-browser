//! Tab lifecycle state machine and RAM-pressure-based hibernation engine.
//!
//! Enforces:
//! - Default idle timeout: 600s (10 min).
//! - High RAM pressure (>= 80%): 30s idle timeout.
//! - Critical RAM pressure (>= 90%): hibernate ALL background tabs immediately.
//! - Never hibernate the active tab.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cef::{CefString, ImplBrowser, ImplBrowserHost, ImplFrame};
use crossbeam_channel::{Receiver, Sender};
use sysinfo::{ProcessesToUpdate, System};

use crate::browser::favicon::create_favicon_download_callback;
use crate::browser::{adblocker, client};
use crate::config::{
    BrowserPreferences, RAM_CHECK_INTERVAL, RAM_CRITICAL_PRESSURE_THRESHOLD,
    RAM_HIGH_PRESSURE_THRESHOLD, TAB_IDLE_TIMEOUT_PRESSURE, TAB_WATCHDOG_TICK_INTERVAL,
};
use crate::events::{BrowserEvent, BrowserShortcut, UiAction};
use crate::storage::db::{SessionTabSnapshot, StorageHandle};
use crate::ui::chrome::{ChromeState, Favicon, TabDisplayState, TabInfo};
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
pub fn pinned_insert_index(tabs: &[Tab], exclude_index: Option<usize>) -> usize {
    tabs.iter()
        .enumerate()
        .filter(|(index, tab)| Some(*index) != exclude_index && tab.pinned)
        .count()
}

/// Choose an insertion index that preserves the pinned-before-unpinned
/// invariant used by both the tab model and the native rail.
pub fn insertion_index(tabs: &[Tab], requested: Option<usize>, pinned: bool) -> usize {
    let boundary = pinned_insert_index(tabs, None);
    let requested = requested.unwrap_or(tabs.len());
    if pinned {
        requested.min(boundary)
    } else {
        requested.clamp(boundary, tabs.len())
    }
}

/// Capture the metadata needed to undo a tab close. Keeping this pure makes
/// the closed-tab stack's title and index behavior testable without CEF.
pub fn closed_tab_from(tab: &Tab, index: usize) -> ClosedTab {
    ClosedTab {
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

pub struct TabManager {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<u64>,
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
    pending_browser_spawns: VecDeque<(u64, String)>,

    // Retry state for browsers that have not committed their initial URL.
    pending_navigation_checks: HashMap<u64, PendingNavigationCheck>,

    // Set when CEF forwards Cmd/Ctrl+K; consumed by the winit handler.
    command_palette_requested: bool,

    // Set when CEF forwards Cmd/Ctrl+,; consumed by the winit handler.
    settings_requested: bool,

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

impl TabManager {
    pub fn new(
        event_sender: Sender<BrowserEvent>,
        event_receiver: Receiver<BrowserEvent>,
        parent_window_view: *mut std::ffi::c_void,
        content_rect: ContentRect,
        storage: Option<StorageHandle>,
        preferences: &BrowserPreferences,
    ) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        let total = sys.total_memory();
        let used = sys.used_memory();
        let adblock_engine = adblocker::create_adblock_engine();
        let adblock_enabled = Arc::new(AtomicBool::new(preferences.adblock_enabled));

        Self {
            tabs: Vec::new(),
            active_tab_id: None,
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
            idle_timeout: preferences.idle_timeout(),
            home_url: preferences.home_url().to_string(),
            storage,
            recently_closed: Vec::new(),
            pending_browser_closes: 0,
            pending_browser_spawns: VecDeque::new(),
            pending_navigation_checks: HashMap::new(),
            command_palette_requested: false,
            settings_requested: false,
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
        self.idle_timeout = preferences.idle_timeout();
        self.home_url = preferences.home_url().to_string();
    }

    /// Restore previous session tabs (instant startup: active tab live, all others hibernated).
    pub fn restore_session(&mut self, restored: Vec<SessionTabSnapshot>) {
        if restored.is_empty() {
            self.create_tab(None, true);
            return;
        }

        let mut active_id = None;
        let mut ordered = restored;
        // Legacy sessions have no pin data, so always place pinned tabs first.
        ordered.sort_by_key(|snapshot| !snapshot.is_pinned);

        for snapshot in ordered {
            let id = self.next_tab_id;
            self.next_tab_id += 1;

            let is_active = snapshot.is_active;
            let tab = Tab {
                id,
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
                state: if is_active {
                    TabState::Active
                } else {
                    TabState::Hibernated
                },
                browser: None,
                favicon: None,
            };

            self.tabs.push(tab);

            if is_active && active_id.is_none() {
                active_id = Some(id);
            }
        }

        // If no tab was marked active, pick the first one
        let target_active = active_id.or_else(|| self.tabs.first().map(|t| t.id));
        if let Some(id) = target_active {
            self.focus_tab(id);
        }

        self.sync_session();
    }

    /// Re-open the most recently closed tab in its original position.
    /// Bound to both `Ctrl/Cmd+Shift+T` and `Ctrl/Cmd+Z`.
    pub fn reopen_closed_tab(&mut self) {
        let Some(closed) = self.recently_closed.pop() else {
            return;
        };
        let index = restore_insert_index(closed.index, self.tabs.len());
        tracing::info!("Re-opening closed tab at {}: {}", index, closed.url);
        self.insert_tab(
            Some(closed.url),
            Some(closed.title),
            Some(index),
            true,
            closed.pinned,
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
                url: t.url.clone(),
                title: t.title.clone(),
                is_active: Some(t.id) == self.active_tab_id,
                is_hibernated: t.state == TabState::Hibernated,
                is_pinned: t.pinned,
            })
            .collect()
    }

    /// Sync open tabs to SQLite.
    pub fn sync_session(&self) {
        if let Some(ref storage) = self.storage {
            storage.save_session(self.session_snapshots());
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

    /// Create a new tab. If `make_active` is true, focuses the new tab immediately.
    pub fn create_tab(&mut self, url: Option<String>, make_active: bool) -> u64 {
        self.insert_tab(url, None, None, make_active, false)
    }

    /// Insert a tab with an optional restored title, position, and pin state.
    fn insert_tab(
        &mut self,
        url: Option<String>,
        title: Option<String>,
        index: Option<usize>,
        make_active: bool,
        pinned: bool,
    ) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let target_url = url.unwrap_or_else(|| self.home_url.clone());
        let at = insertion_index(&self.tabs, index, pinned);

        let tab = Tab {
            id,
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

    /// Toggle a tab between the compact pinned section and the normal tabs.
    /// Pinning appends to the pinned section; unpinning inserts at its start.
    pub fn toggle_pin_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let pinned = !self.tabs[index].pinned;
        let insert_at = pinned_insert_index(&self.tabs, Some(index));
        let mut tab = self.tabs.remove(index);
        tab.pinned = pinned;
        self.tabs.insert(insert_at, tab);
        self.sync_session();
    }

    /// Focus an existing tab by ID.
    pub fn focus_tab(&mut self, id: u64) {
        if self.active_tab_id == Some(id) {
            // Already active; ensure child view is positioned
            self.show_active_tab_view();
            return;
        }

        // 1. Hide the old active tab's native view and transition to Background
        if let Some(old_id) = self.active_tab_id {
            if let Some(old_tab) = self.tabs.iter_mut().find(|t| t.id == old_id) {
                if let Some(ref browser) = old_tab.browser {
                    if let Some(host) = browser.host() {
                        let view_handle = host.window_handle();
                        if !view_handle.is_null() {
                            if let Err(e) = unsafe { platform_view::hide_view(view_handle.cast()) }
                            {
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

        // 2. Activate target tab
        self.active_tab_id = Some(id);

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
            self.queue_browser_spawn(id, target_url);
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

        let mut tab = self.tabs.remove(index);
        self.recently_closed.push(closed_tab_from(&tab, index));
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
            if !self.tabs.is_empty() {
                // Focus nearest tab
                let new_index = index.min(self.tabs.len() - 1);
                let new_id = self.tabs[new_index].id;
                self.focus_tab(new_id);
            } else {
                // All tabs closed — open a default tab
                self.create_tab(None, true);
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

    /// Resize outer bounds and adjust CEF child view.
    pub fn resize(&mut self, rect: ContentRect) {
        self.content_rect = rect;
        self.resize_active_tab_view();
    }

    /// Queue a browser creation request for the next `about_to_wait` tick.
    fn queue_browser_spawn(&mut self, tab_id: u64, url: String) {
        if self
            .pending_browser_spawns
            .iter()
            .any(|(id, _)| *id == tab_id)
        {
            return;
        }
        tracing::info!("Queued browser spawn for tab {tab_id} ({url})");
        self.pending_browser_spawns.push_back((tab_id, url));
    }

    /// Create queued browsers on the UI thread, outside winit window events.
    fn drain_browser_spawns(&mut self) {
        while let Some((tab_id, url)) = self.pending_browser_spawns.pop_front() {
            let still_needed = self.tabs.iter().any(|tab| {
                tab.id == tab_id && tab.browser.is_none() && Some(tab_id) == self.active_tab_id
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
            self.spawn_browser(tab_id, &url);
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

    /// Spawns a CEF browser for a specific tab ID.
    fn spawn_browser(&self, tab_id: u64, url: &str) {
        let mut client = client::create_tab_client(
            tab_id,
            self.event_sender.clone(),
            self.adblock_engine.clone(),
            Arc::clone(&self.adblock_enabled),
        );
        let mut window_info = cef::WindowInfo::default();

        window_info.windowless_rendering_enabled = 0;
        window_info.parent_view = self.parent_window_view;
        window_info.bounds = cef::Rect {
            x: self.content_rect.x as i32,
            y: self.content_rect.y as i32,
            width: self.content_rect.width as i32,
            height: self.content_rect.height as i32,
        };

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

    /// Show DevTools once per browser. Repeated F12 presses focus the existing
    /// window instead of creating a new one.
    fn show_dev_tools(&self, tab_id: u64) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let Some(host) = tab.browser.as_ref().and_then(|browser| browser.host()) else {
            return;
        };

        if host.has_dev_tools() != 0 {
            return;
        }

        let mut window_info = cef::WindowInfo::default();
        window_info.windowless_rendering_enabled = 0;
        window_info.bounds = cef::Rect {
            x: 100,
            y: 100,
            width: 960,
            height: 720,
        };
        host.show_dev_tools(Some(&window_info), None, None, None);
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
        if view_handle.is_null() || self.parent_window_view.is_null() {
            return;
        }
        if let Err(e) = unsafe {
            platform_view::show_view(
                self.parent_window_view.cast(),
                view_handle.cast(),
                self.content_rect,
            )
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
        if view_handle.is_null() {
            return;
        }
        if let Err(e) = unsafe { platform_view::resize_view(view_handle.cast(), self.content_rect) }
        {
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

    fn handle_browser_event(&mut self, event: BrowserEvent) {
        match event {
            BrowserEvent::BrowserCreated { tab_id, browser } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.browser = Some(browser);
                }
                if self.active_tab_id == Some(tab_id) {
                    self.show_active_tab_view();
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
                let ids: Vec<u64> = self
                    .tabs
                    .iter()
                    .rev()
                    .filter(|t| t.id != tab_id)
                    .map(|t| t.id)
                    .collect();
                for id in ids {
                    self.close_tab(id);
                }
            }
            UiAction::CloseTabsToRight { tab_id } => {
                let Some(index) = self.tabs.iter().position(|t| t.id == tab_id) else {
                    return;
                };
                let ids: Vec<u64> = self.tabs[index + 1..].iter().rev().map(|t| t.id).collect();
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
                    self.insert_tab(Some(url), Some(title), Some(index + 1), true, pinned);
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
            UiAction::ApplyPreferences(preferences) => {
                self.apply_preferences(&preferences);
            }
        }
    }

    /// Watchdog tick: checks RAM pressure and tab idle durations.
    pub fn tick(&mut self) {
        let now = Instant::now();

        // Create browsers only from `about_to_wait`, never from a window event.
        self.drain_browser_spawns();
        self.retry_blank_initial_navigations(now);

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
        next_navigation_retry_delay, pinned_insert_index, preferred_favicon_url, ram_pressure,
        restore_insert_index, should_hibernate_tab, should_sync_omnibox, Tab, TabState,
    };
    use crate::config::{TAB_IDLE_TIMEOUT_NORMAL, TAB_IDLE_TIMEOUT_PRESSURE};
    use std::time::{Duration, Instant};

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

        assert_eq!(pinned_insert_index(&tabs, None), 2);
        assert_eq!(pinned_insert_index(&tabs, Some(0)), 1);
        assert_eq!(insertion_index(&tabs, None, true), 2);
        assert_eq!(insertion_index(&tabs, Some(99), true), 2);
        assert_eq!(insertion_index(&tabs, None, false), 3);
        assert_eq!(insertion_index(&tabs, Some(0), false), 2);
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
