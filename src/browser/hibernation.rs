//! Tab lifecycle state machine and RAM-pressure-based hibernation engine.
//!
//! Enforces:
//! - Default idle timeout: 600s (10 min).
//! - High RAM pressure (>= 80%): 30s idle timeout.
//! - Critical RAM pressure (>= 90%): hibernate ALL background tabs immediately.
//! - Never hibernate the active tab.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Instant;

use cef::{ImplBrowser, ImplBrowserHost, ImplFrame};
use crossbeam_channel::{Receiver, Sender};
use sysinfo::{ProcessesToUpdate, System};

use crate::browser::{adblocker, client};
use crate::config::{
    DEFAULT_HOME_URL, RAM_CHECK_INTERVAL, RAM_CRITICAL_PRESSURE_THRESHOLD,
    RAM_HIGH_PRESSURE_THRESHOLD, TAB_IDLE_TIMEOUT_NORMAL, TAB_IDLE_TIMEOUT_PRESSURE,
    TAB_WATCHDOG_TICK_INTERVAL,
};
use crate::events::{BrowserEvent, UiAction};
use crate::storage::db::{SessionTabSnapshot, StorageHandle};
use crate::ui::chrome::{ChromeState, TabDisplayState, TabInfo};
use crate::ui::platform_view::{self, ContentRect};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    Active,
    Background { since: Instant },
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
            .field("state", &self.state)
            .field("has_browser", &self.browser.is_some())
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

    // Brave adblock engine shared across all tabs
    adblock_engine: Arc<adblock::Engine>,

    // SQLite persistence worker & closed tab stack
    pub storage: Option<StorageHandle>,
    pub recently_closed: Vec<String>,

    // Number of `host.close_browser(1)` calls awaiting an `on_before_close` event.
    pending_browser_closes: u32,
}

impl TabManager {
    pub fn new(
        event_sender: Sender<BrowserEvent>,
        event_receiver: Receiver<BrowserEvent>,
        parent_window_view: *mut std::ffi::c_void,
        content_rect: ContentRect,
        storage: Option<StorageHandle>,
    ) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        let total = sys.total_memory();
        let used = sys.used_memory();
        let adblock_engine = adblocker::create_adblock_engine();

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
            storage,
            recently_closed: Vec::new(),
            pending_browser_closes: 0,
        }
    }

    /// Restore previous session tabs (instant startup: active tab live, all others hibernated).
    pub fn restore_session(&mut self, restored: Vec<SessionTabSnapshot>) {
        if restored.is_empty() {
            self.create_tab(None, true);
            return;
        }

        let mut active_id = None;

        for snapshot in restored {
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
                state: if is_active {
                    TabState::Active
                } else {
                    TabState::Hibernated
                },
                browser: None,
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

    /// Re-open the most recently closed tab (Ctrl+Shift+T).
    pub fn reopen_closed_tab(&mut self) {
        if let Some(url) = self.recently_closed.pop() {
            tracing::info!("Re-opening closed tab: {}", url);
            self.create_tab(Some(url), true);
        }
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
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let target_url = url.unwrap_or_else(|| DEFAULT_HOME_URL.to_string());

        let tab = Tab {
            id,
            url: target_url.clone(),
            title: "New Tab".to_string(),
            is_loading: false,
            can_go_back: false,
            can_go_forward: false,
            blocked_requests: 0,
            state: if make_active {
                TabState::Active
            } else {
                TabState::Hibernated
            },
            browser: None,
        };

        self.tabs.push(tab);

        if make_active {
            self.focus_tab(id);
        } else {
            self.sync_session();
        }

        id
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
                            #[cfg(target_os = "macos")]
                            unsafe {
                                platform_view::hide_view(view_handle.cast());
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
            // Wake or spawn CEF browser instance
            self.spawn_browser(id, &target_url);
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
        self.recently_closed.push(tab.url.clone());

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

        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            if tab.state == TabState::Hibernated {
                return;
            }

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

    /// Spawns a CEF browser for a specific tab ID.
    fn spawn_browser(&self, tab_id: u64, url: &str) {
        let mut client = client::create_tab_client(
            tab_id,
            self.event_sender.clone(),
            self.adblock_engine.clone(),
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

    /// Position and show the active tab's native view.
    pub fn show_active_tab_view(&self) {
        if let Some(id) = self.active_tab_id {
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                if let Some(ref browser) = tab.browser {
                    if let Some(host) = browser.host() {
                        let view_handle = host.window_handle();
                        if !view_handle.is_null() && !self.parent_window_view.is_null() {
                            #[cfg(target_os = "macos")]
                            unsafe {
                                platform_view::show_view(
                                    self.parent_window_view.cast(),
                                    view_handle.cast(),
                                    self.content_rect,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Resize the active tab's native view.
    pub fn resize_active_tab_view(&self) {
        if let Some(id) = self.active_tab_id {
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                if let Some(ref browser) = tab.browser {
                    if let Some(host) = browser.host() {
                        let view_handle = host.window_handle();
                        if !view_handle.is_null() {
                            #[cfg(target_os = "macos")]
                            unsafe {
                                platform_view::resize_view(view_handle.cast(), self.content_rect);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Drain incoming CEF callbacks and update tab states.
    pub fn pump_events(&mut self) {
        while let Ok(event) = self.event_receiver.try_recv() {
            self.handle_browser_event(event);
        }
    }

    fn handle_browser_event(&mut self, event: BrowserEvent) {
        match event {
            BrowserEvent::BrowserCreated { tab_id, browser } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.browser = Some(browser);
                    if self.active_tab_id == Some(tab_id) {
                        self.show_active_tab_view();
                    }
                }
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
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.url = url.clone();
                    if let Some(ref storage) = self.storage {
                        storage.record_visit(url, tab.title.clone());
                    }
                }
                self.sync_session();
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
            UiAction::CloseTab { tab_id } => {
                self.close_tab(tab_id);
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
                if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
                    if let Some(ref browser) = tab.browser {
                        if let Some(host) = browser.host() {
                            host.show_dev_tools(None, None, None, None);
                        }
                    }
                }
            }
        }
    }

    /// Watchdog tick: checks RAM pressure and tab idle durations.
    pub fn tick(&mut self) {
        let now = Instant::now();

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

        let ram_ratio = if self.ram_total_bytes > 0 {
            self.ram_used_bytes as f32 / self.ram_total_bytes as f32
        } else {
            0.0
        };

        // Critical memory pressure: hibernate ALL background tabs immediately
        if ram_ratio >= RAM_CRITICAL_PRESSURE_THRESHOLD {
            tracing::warn!(
                "Critical RAM pressure ({:.1}%). Hibernating all background tabs.",
                ram_ratio * 100.0
            );
            self.hibernate_all_background();
            return;
        }

        // High memory pressure: shrink timeout to 30s
        let idle_timeout = if ram_ratio >= RAM_HIGH_PRESSURE_THRESHOLD {
            TAB_IDLE_TIMEOUT_PRESSURE
        } else {
            TAB_IDLE_TIMEOUT_NORMAL
        };

        let active = self.active_tab_id;
        let expired_ids: Vec<u64> = self
            .tabs
            .iter()
            .filter_map(|t| {
                if Some(t.id) == active {
                    return None;
                }
                match t.state {
                    TabState::Background { since } if now.duration_since(since) >= idle_timeout => {
                        Some(t.id)
                    }
                    _ => None,
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
            })
            .collect();

        state.active_tab_id = self.active_tab_id;
        state.ram_mb = self.ram_process_mb;

        if let Some(id) = self.active_tab_id {
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                state.is_loading = tab.is_loading;
                // If omnibox text was never set or tab changed, keep it synced
                if state.omnibox_text.is_empty() {
                    state.omnibox_text = tab.url.clone();
                }
            }
        }
    }
}
