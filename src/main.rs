#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod browser;
mod config;
mod events;
mod storage;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use app::KasumiBrowserApp;
use browser::hibernation::TabManager;
use cef::CefString;
use config::{rail_width, SHUTDOWN_MAX_WAIT_TICKS};
use events::UiAction;
use storage::db::{StorageHandle, StorageManager};
use ui::chrome::{ChromeRenderer, ChromeState, PaletteRenderer, TooltipRenderer};
use ui::platform_view::ContentRect;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId, WindowLevel};

// ---------------------------------------------------------------------------
// macOS: CEF external message pump via a main-thread CoreFoundation timer.
//
// winit 0.30 on macOS has a single-entry guard around its event handler, and
// `ApplicationHandler::about_to_wait` is still dispatched through that guard.
// Calling `cef::do_message_loop_work()` there re-enters winit when CEF pumps
// NSApp events. A CFRunLoop timer runs outside winit's handler, so the pump
// stays on the main thread (no extra pump thread) without re-entering it.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod cf {
    use std::ffi::c_void;

    pub type CFAllocatorRef = *mut c_void;
    pub type CFRunLoopRef = *mut c_void;
    pub type CFRunLoopTimerRef = *mut c_void;
    pub type CFStringRef = *const c_void;
    pub type CFIndex = isize;
    pub type CFOptionFlags = usize;
    pub type CFRunLoopTimerCallBack = unsafe extern "C" fn(CFRunLoopTimerRef, *mut c_void);

    #[repr(C)]
    pub struct CFRunLoopTimerContext {
        pub version: CFIndex,
        pub info: *mut c_void,
        pub retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
        pub release: Option<unsafe extern "C" fn(*const c_void)>,
        pub copy_description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRunLoopGetMain() -> CFRunLoopRef;
        pub fn CFRunLoopTimerCreate(
            allocator: CFAllocatorRef,
            fire_date: f64,
            interval: f64,
            flags: CFOptionFlags,
            order: CFIndex,
            callout: CFRunLoopTimerCallBack,
            context: *const CFRunLoopTimerContext,
        ) -> CFRunLoopTimerRef;
        pub fn CFRunLoopAddTimer(
            run_loop: CFRunLoopRef,
            timer: CFRunLoopTimerRef,
            mode: CFStringRef,
        );
        pub fn CFRunLoopTimerInvalidate(timer: CFRunLoopTimerRef);
        pub fn CFRelease(cf: *const c_void);
        pub static kCFRunLoopCommonModes: CFStringRef;
    }
}

#[cfg(target_os = "macos")]
static CEF_PUMP_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
static CEF_PUMP_ENABLED: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "macos")]
struct PumpGuard;

#[cfg(target_os = "macos")]
impl Drop for PumpGuard {
    fn drop(&mut self) {
        CEF_PUMP_ACTIVE.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn cef_timer_callback(
    _timer: cf::CFRunLoopTimerRef,
    _info: *mut std::ffi::c_void,
) {
    // Once CEF shutdown starts, no further message-loop work is allowed.
    if !CEF_PUMP_ENABLED.load(Ordering::Acquire) {
        return;
    }
    // CEF's NSApp pump can fire this timer again through a nested run loop.
    if CEF_PUMP_ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    let _guard = PumpGuard;
    cef::do_message_loop_work();
}

#[cfg(target_os = "macos")]
struct CefPumpTimer {
    timer: cf::CFRunLoopTimerRef,
}

#[cfg(target_os = "macos")]
impl CefPumpTimer {
    fn start() -> Self {
        let context = cf::CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copy_description: None,
        };

        unsafe {
            let timer = cf::CFRunLoopTimerCreate(
                std::ptr::null_mut(),
                0.0,         // fire immediately
                1.0 / 120.0, // ~120 Hz; matches the external-message-pump cadence
                0,
                0, // order
                cef_timer_callback,
                &context,
            );
            cf::CFRunLoopAddTimer(cf::CFRunLoopGetMain(), timer, cf::kCFRunLoopCommonModes);
            Self { timer }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for CefPumpTimer {
    fn drop(&mut self) {
        unsafe {
            cf::CFRunLoopTimerInvalidate(self.timer);
            cf::CFRelease(self.timer.cast_const());
        }
    }
}

// ---------------------------------------------------------------------------
// macOS CrAppControlProtocol – required before CEF initialization
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn setup_mac_application() {
    use std::ffi::{c_void, CString};

    static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

    extern "C" fn is_handling_send_event(
        _this: *mut c_void,
        _cmd: *mut c_void,
    ) -> objc2::runtime::Bool {
        objc2::runtime::Bool::new(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
    }

    extern "C" fn set_handling_send_event(
        _this: *mut c_void,
        _cmd: *mut c_void,
        val: objc2::runtime::Bool,
    ) {
        HANDLING_SEND_EVENT.store(val.as_raw() != 0, Ordering::Relaxed);
    }

    unsafe {
        let cls_name = CString::new("NSApplication").unwrap();
        let cls = objc2::ffi::objc_getClass(cls_name.as_ptr());
        if !cls.is_null() {
            let cls_mut = cls as *mut _;
            let is_sel =
                objc2::ffi::sel_registerName(CString::new("isHandlingSendEvent").unwrap().as_ptr());
            let types_is = CString::new("c@:").unwrap();
            let imp_is: objc2::ffi::IMP =
                Some(std::mem::transmute(is_handling_send_event as *const ()));
            objc2::ffi::class_addMethod(cls_mut, is_sel, imp_is, types_is.as_ptr());

            let set_sel = objc2::ffi::sel_registerName(
                CString::new("setHandlingSendEvent:").unwrap().as_ptr(),
            );
            let types_set = CString::new("v@:c").unwrap();
            let imp_set: objc2::ffi::IMP =
                Some(std::mem::transmute(set_handling_send_event as *const ()));
            objc2::ffi::class_addMethod(cls_mut, set_sel, imp_set, types_set.as_ptr());

            let proto_name = CString::new("CrAppControlProtocol").unwrap();
            let proto = objc2::ffi::objc_getProtocol(proto_name.as_ptr());
            if !proto.is_null() {
                objc2::ffi::class_addProtocol(cls_mut, proto);
            }

            // Trigger sharedApplication so the class is instantiated.
            let shared_sel =
                objc2::ffi::sel_registerName(CString::new("sharedApplication").unwrap().as_ptr());
            let msg_send: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void =
                std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
            let _ = msg_send(cls as *mut c_void, shared_sel as *const c_void);
        }
    }
}

// ---------------------------------------------------------------------------
// Kasumi Browser winit ApplicationHandler
// ---------------------------------------------------------------------------

struct KasumiBrowserHandler {
    // wgpu instance shared between adapter request + surface creation
    wgpu_instance: wgpu::Instance,
    adapter: Arc<wgpu::Adapter>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,

    // Window + renderer – created in `resumed()`
    window: Option<Arc<Window>>,
    chrome: Option<ChromeRenderer>,
    tab_manager: Option<TabManager>,
    storage_handle: Option<StorageHandle>,

    // Transparent always-on-top overlay for the full-window command palette.
    // The CEF child view stays visible behind it on every platform.
    overlay_window: Option<Arc<Window>>,
    overlay_window_id: Option<WindowId>,
    overlay: Option<PaletteRenderer>,
    overlay_visible: bool,

    // Transparent, click-through overlay for rail tooltips. A native window is
    // required so tooltips can appear above the CEF child view.
    tooltip_window: Option<Arc<Window>>,
    tooltip_window_id: Option<WindowId>,
    tooltip: Option<TooltipRenderer>,
    tooltip_visible: bool,
    tooltip_size: Option<(u32, u32)>,

    // Current UI state (populated from CEF events in Phase 3+)
    chrome_state: ChromeState,

    // Keyboard modifier tracking
    modifiers: ModifiersState,

    // Shutdown state. CEF message-pump work is only ever performed from
    // `about_to_wait` so it can never re-enter winit's window event handler.
    shutdown_requested: bool,
    shutdown_browsers_closed: bool,
    shutdown_ticks: u32,
}

impl KasumiBrowserHandler {
    fn new(
        wgpu_instance: wgpu::Instance,
        adapter: Arc<wgpu::Adapter>,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Self {
        Self {
            wgpu_instance,
            adapter,
            device,
            queue,
            window: None,
            chrome: None,
            tab_manager: None,
            storage_handle: None,
            overlay_window: None,
            overlay_window_id: None,
            overlay: None,
            overlay_visible: false,
            tooltip_window: None,
            tooltip_window_id: None,
            tooltip: None,
            tooltip_visible: false,
            tooltip_size: None,
            chrome_state: ChromeState::default(),
            modifiers: ModifiersState::default(),
            shutdown_requested: false,
            shutdown_browsers_closed: false,
            shutdown_ticks: 0,
        }
    }

    fn sync_palette_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if self.shutdown_requested {
            self.hide_palette_overlay();
            return;
        }
        if self.chrome_state.command_palette_open {
            self.ensure_palette_overlay(event_loop);
        } else {
            self.hide_palette_overlay();
        }
    }

    fn ensure_palette_overlay(&mut self, event_loop: &ActiveEventLoop) {
        let mut created_window = false;
        if self.overlay_window.is_none() {
            created_window = true;
            let Some(main) = self.window.as_ref() else {
                return;
            };
            let attrs = Window::default_attributes()
                .with_title("Kasumi Command Palette")
                .with_inner_size(main.inner_size())
                .with_transparent(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_window_level(WindowLevel::AlwaysOnTop);
            let overlay_window = match event_loop.create_window(attrs) {
                Ok(window) => Arc::new(window),
                Err(e) => {
                    tracing::error!("Failed to create command palette overlay: {}", e);
                    return;
                }
            };
            overlay_window.set_window_level(WindowLevel::AlwaysOnTop);
            self.overlay_window_id = Some(overlay_window.id());
            self.overlay_window = Some(overlay_window);
        }

        if self.overlay.is_none() {
            let Some(window) = self.overlay_window.as_ref() else {
                return;
            };
            let Some(overlay) = PaletteRenderer::new(
                Arc::clone(window),
                &self.wgpu_instance,
                &self.adapter,
                Arc::clone(&self.device),
                Arc::clone(&self.queue),
            ) else {
                tracing::error!("Failed to initialize the command palette overlay renderer");
                return;
            };
            self.overlay = Some(overlay);
        }

        if created_window || !self.overlay_visible {
            self.position_palette_overlay();
        }
        if let Some(window) = self.overlay_window.as_ref() {
            if !self.overlay_visible {
                window.set_visible(true);
                window.focus_window();
                self.overlay_visible = true;
            }
            window.request_redraw();
        }
    }

    fn position_palette_overlay(&self) {
        let (Some(main), Some(overlay)) = (self.window.as_ref(), self.overlay_window.as_ref())
        else {
            return;
        };
        let _ = overlay.request_inner_size(main.inner_size());
        // The overlay is undecorated, so its outer origin is its content origin.
        // Anchor it to the main window's content origin to account for the
        // macOS title bar; egui coordinates are relative to that content area.
        let origin = main.inner_position().or_else(|_| main.outer_position());
        if let Ok(origin) = origin {
            overlay.set_outer_position(overlay_screen_position(
                origin,
                (0.0, 0.0),
                main.scale_factor(),
            ));
        }
    }

    fn hide_palette_overlay(&mut self) {
        if self.overlay_visible {
            if let Some(window) = self.overlay_window.as_ref() {
                let _ = window.set_visible(false);
            }
            self.overlay_visible = false;
            if let Some(window) = self.window.as_ref() {
                window.focus_window();
                window.request_redraw();
            }
        }
    }

    fn sync_tooltip_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if self.shutdown_requested || self.chrome_state.command_palette_open {
            self.hide_tooltip_overlay();
            return;
        }
        let Some(tooltip) = self.chrome_state.tooltip.clone() else {
            self.hide_tooltip_overlay();
            return;
        };

        if self.tooltip_window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("Kasumi Tooltip")
                .with_inner_size(winit::dpi::LogicalSize::new(tooltip.width, tooltip.height))
                .with_transparent(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_window_level(WindowLevel::AlwaysOnTop);
            let window = match event_loop.create_window(attrs) {
                Ok(window) => Arc::new(window),
                Err(e) => {
                    tracing::error!("Failed to create tooltip overlay: {e}");
                    return;
                }
            };
            window.set_window_level(WindowLevel::AlwaysOnTop);
            if let Err(e) = window.set_cursor_hittest(false) {
                tracing::warn!("Tooltip overlay click-through unavailable: {e}");
            }
            self.tooltip_window_id = Some(window.id());
            self.tooltip_window = Some(window);
        }

        if self.tooltip.is_none() {
            let Some(window) = self.tooltip_window.as_ref() else {
                return;
            };
            let Some(renderer) = TooltipRenderer::new(
                Arc::clone(window),
                &self.wgpu_instance,
                &self.adapter,
                Arc::clone(&self.device),
                Arc::clone(&self.queue),
            ) else {
                tracing::error!("Failed to initialize the tooltip overlay renderer");
                return;
            };
            self.tooltip = Some(renderer);
        }

        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
        let size = (
            (tooltip.width * scale as f32).round().max(1.0) as u32,
            (tooltip.height * scale as f32).round().max(1.0) as u32,
        );
        let (Some(main), Some(overlay)) = (self.window.as_ref(), self.tooltip_window.as_ref())
        else {
            return;
        };
        if self.tooltip_size != Some(size) {
            let _ = overlay.request_inner_size(winit::dpi::PhysicalSize::new(size.0, size.1));
            self.tooltip_size = Some(size);
        }
        let origin = main.inner_position().or_else(|_| main.outer_position());
        if let Ok(origin) = origin {
            overlay.set_outer_position(overlay_screen_position(
                origin,
                (tooltip.anchor.x, tooltip.anchor.y),
                scale,
            ));
        }
        if !self.tooltip_visible {
            overlay.set_visible(true);
            if let Err(e) = overlay.set_cursor_hittest(false) {
                tracing::warn!("Tooltip overlay click-through unavailable: {e}");
            }
            // Keep keyboard focus in the browser chrome, not the helper window.
            main.focus_window();
            self.tooltip_visible = true;
        }
        overlay.request_redraw();
    }

    fn hide_tooltip_overlay(&mut self) {
        if self.tooltip_visible {
            if let Some(window) = self.tooltip_window.as_ref() {
                let _ = window.set_visible(false);
            }
            self.tooltip_visible = false;
        }
        self.tooltip_size = None;
    }
}

/// Convert an egui content-space point into physical screen coordinates.
///
/// egui coordinates start at the main window's *content* origin, while the
/// helper overlays are undecorated, so their outer origin equals their content
/// origin. Callers must pass `inner_position()` (not `outer_position()`),
/// otherwise macOS title-bar height shifts tooltips away from their tab rows.
fn overlay_screen_position(
    content_origin: winit::dpi::PhysicalPosition<i32>,
    point: (f32, f32),
    scale_factor: f64,
) -> winit::dpi::PhysicalPosition<i32> {
    winit::dpi::PhysicalPosition::new(
        content_origin.x + (point.0 as f64 * scale_factor).round() as i32,
        content_origin.y + (point.1 as f64 * scale_factor).round() as i32,
    )
}

/// Cmd on macOS, Ctrl elsewhere. Keeping this decision in one place makes the
/// shortcut behavior easy to test without constructing a winit event loop.
fn is_primary_shortcut_modifier(modifiers: ModifiersState) -> bool {
    modifiers.control_key() || modifiers.super_key()
}

impl ApplicationHandler for KasumiBrowserHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already created
        }

        let attrs = Window::default_attributes()
            .with_title("Kasumi Browser")
            .with_inner_size(winit::dpi::LogicalSize::new(1280u32, 800u32));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("Failed to create window: {}", e);
                event_loop.exit();
                return;
            }
        };

        let Some(chrome) = ChromeRenderer::new(
            Arc::clone(&window),
            &self.wgpu_instance,
            &self.adapter,
            Arc::clone(&self.device),
            Arc::clone(&self.queue),
        ) else {
            tracing::error!("Failed to initialize chrome renderer");
            event_loop.exit();
            return;
        };

        let parent_view: *mut std::ffi::c_void = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match window.window_handle() {
                Ok(handle) => match handle.as_raw() {
                    #[cfg(target_os = "macos")]
                    RawWindowHandle::AppKit(appkit) => appkit.ns_view.as_ptr(),
                    #[cfg(windows)]
                    RawWindowHandle::Win32(win32) => win32.hwnd.get() as *mut std::ffi::c_void,
                    #[cfg(target_os = "linux")]
                    RawWindowHandle::Xlib(xlib) => xlib.window as usize as *mut std::ffi::c_void,
                    #[cfg(target_os = "linux")]
                    RawWindowHandle::Xcb(xcb) => xcb.window.get() as usize as *mut std::ffi::c_void,
                    _ => std::ptr::null_mut(),
                },
                Err(_) => std::ptr::null_mut(),
            }
        };

        let size = window.inner_size();
        let scale = window.scale_factor();
        let rect = ContentRect::from_window(
            size.width,
            size.height,
            scale,
            rail_width(self.chrome_state.rail_expanded),
        );
        tracing::info!("CEF content rect (logical): {:?}", rect);

        let db_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/kasumi_browser.db"
        ));
        let storage_mgr = StorageManager::new(db_path.clone());
        let storage_handle = storage_mgr.start_worker();
        let restored_tabs = StorageManager::load_session(&db_path).unwrap_or_default();

        let (tx, rx) = crossbeam_channel::unbounded();
        let mut tab_manager =
            TabManager::new(tx, rx, parent_view, rect, Some(storage_handle.clone()));
        tab_manager.restore_session(restored_tabs);

        self.window = Some(Arc::clone(&window));
        self.chrome = Some(chrome);
        self.tab_manager = Some(tab_manager);
        self.storage_handle = Some(storage_handle);

        tracing::info!("Window, chrome renderer, storage, and TabManager initialized successfully");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Overlay events never reach the main chrome or global shortcuts.
        if Some(window_id) == self.overlay_window_id {
            let _consumed = if let (Some(window), Some(overlay)) =
                (self.overlay_window.as_ref(), self.overlay.as_mut())
            {
                overlay.on_window_event(window, &event)
            } else {
                false
            };

            match event {
                WindowEvent::CloseRequested => {
                    self.chrome_state.close_command_palette();
                    self.sync_palette_overlay(event_loop);
                }
                WindowEvent::Resized(new_size) => {
                    if let Some(overlay) = self.overlay.as_mut() {
                        overlay.resize(new_size);
                    }
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    if let (Some(window), Some(overlay)) =
                        (self.overlay_window.as_ref(), self.overlay.as_mut())
                    {
                        overlay.resize(window.inner_size());
                    }
                }
                WindowEvent::RedrawRequested => {
                    let mut actions = Vec::new();
                    if let (Some(window), Some(overlay)) =
                        (self.overlay_window.as_ref(), self.overlay.as_mut())
                    {
                        actions = overlay.render(window, &mut self.chrome_state);
                    }
                    for action in actions {
                        if let Some(tm) = self.tab_manager.as_mut() {
                            tm.handle_ui_action(action);
                        }
                    }
                    self.sync_palette_overlay(event_loop);
                }
                _ => {}
            }
            return;
        }

        // Tooltip overlay events never reach the main chrome or shortcuts.
        if Some(window_id) == self.tooltip_window_id {
            let _consumed = if let (Some(window), Some(tooltip)) =
                (self.tooltip_window.as_ref(), self.tooltip.as_mut())
            {
                tooltip.on_window_event(window, &event)
            } else {
                false
            };

            match event {
                WindowEvent::CloseRequested => {
                    self.chrome_state.tooltip = None;
                    self.hide_tooltip_overlay();
                }
                WindowEvent::Resized(new_size) => {
                    if let Some(tooltip) = self.tooltip.as_mut() {
                        tooltip.resize(new_size);
                    }
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    if let (Some(window), Some(tooltip)) =
                        (self.tooltip_window.as_ref(), self.tooltip.as_mut())
                    {
                        tooltip.resize(window.inner_size());
                    }
                }
                WindowEvent::RedrawRequested => {
                    if let (Some(window), Some(renderer)) =
                        (self.tooltip_window.as_ref(), self.tooltip.as_mut())
                    {
                        if let Some(tooltip) = self.chrome_state.tooltip.clone() {
                            renderer.render(window, &tooltip);
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        // Let egui consume keyboard/mouse input first
        let _consumed =
            if let (Some(window), Some(chrome)) = (self.window.as_ref(), self.chrome.as_mut()) {
                chrome.on_window_event(window, &event)
            } else {
                false
            };

        // Track modifier keys ourselves too (for Ctrl+L etc.)
        match &event {
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                let ctrl = is_primary_shortcut_modifier(self.modifiers);
                let alt = self.modifiers.alt_key();
                let shift = self.modifiers.shift_key();

                if alt {
                    if let Key::Character(c) = logical_key {
                        if c.eq_ignore_ascii_case("h") {
                            if let Some(tm) = self.tab_manager.as_mut() {
                                tm.handle_ui_action(UiAction::HibernateAllBackground);
                            }
                        }
                    }
                } else if ctrl && shift {
                    if let Key::Character(c) = logical_key {
                        if c.eq_ignore_ascii_case("t") {
                            if let Some(tm) = self.tab_manager.as_mut() {
                                tm.reopen_closed_tab();
                            }
                        }
                    }
                } else {
                    match logical_key {
                        Key::Named(NamedKey::F12) => {
                            if let Some(tm) = self.tab_manager.as_mut() {
                                if let Some(id) = tm.active_tab_id {
                                    tm.handle_ui_action(UiAction::OpenDevTools { tab_id: id });
                                }
                            }
                        }
                        Key::Character(c) if ctrl => match c.as_str() {
                            "l" | "L" | "k" | "K" => {
                                self.chrome_state.open_command_palette();
                            }
                            "t" | "T" => {
                                if let Some(tm) = self.tab_manager.as_mut() {
                                    tm.handle_ui_action(UiAction::NewTab { url: None });
                                }
                            }
                            "w" | "W" => {
                                if let Some(tm) = self.tab_manager.as_mut() {
                                    if let Some(id) = tm.active_tab_id {
                                        tm.handle_ui_action(UiAction::CloseTab { tab_id: id });
                                    }
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
            WindowEvent::CloseRequested => {
                tracing::info!("Window close requested — deferring CEF shutdown to about_to_wait");
                // Never pump CEF (or call cef::shutdown()) from inside a window
                // event callback. On macOS, do_message_loop_work() dispatches
                // NSApp events that can re-enter winit's single-entry handler.
                self.shutdown_requested = true;
            }
            WindowEvent::Resized(new_size) => {
                if let Some(chrome) = self.chrome.as_mut() {
                    chrome.resize(*new_size);
                }
                self.position_palette_overlay();
                if let (Some(window), Some(tm)) = (self.window.as_ref(), self.tab_manager.as_mut())
                {
                    let scale = window.scale_factor();
                    let rect = ContentRect::from_window(
                        new_size.width,
                        new_size.height,
                        scale,
                        rail_width(self.chrome_state.rail_expanded),
                    );
                    tm.resize(rect);
                }
            }
            WindowEvent::RedrawRequested => {
                let rail_before = self.chrome_state.rail_expanded;
                if let (Some(window), Some(chrome), Some(tm)) = (
                    self.window.as_ref(),
                    self.chrome.as_mut(),
                    self.tab_manager.as_mut(),
                ) {
                    // CEF events and the watchdog are processed once per loop
                    // iteration in `about_to_wait`, never here. This keeps all
                    // CEF work in one non-window-event callback and prevents
                    // double-processing.
                    let actions = chrome.render(window, &mut self.chrome_state);

                    for action in actions {
                        tm.handle_ui_action(action);
                    }
                }

                if rail_before != self.chrome_state.rail_expanded {
                    if let (Some(window), Some(tm)) =
                        (self.window.as_ref(), self.tab_manager.as_mut())
                    {
                        let size = window.inner_size();
                        let scale = window.scale_factor();
                        let rect = ContentRect::from_window(
                            size.width,
                            size.height,
                            scale,
                            rail_width(self.chrome_state.rail_expanded),
                        );
                        tracing::info!("Workspace rail toggled; CEF content rect: {:?}", rect);
                        tm.resize(rect);
                    }
                }
            }
            _ => {}
        }

        self.sync_palette_overlay(event_loop);
        self.sync_tooltip_overlay(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // CEF's NSApp pump can invoke winit's run-loop observer, which calls
        // this method again while `cef::do_message_loop_work()` is still on the
        // stack. Never start teardown from that nested tick.
        #[cfg(target_os = "macos")]
        if CEF_PUMP_ACTIVE.load(Ordering::Acquire) {
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
            return;
        }

        // One-time shutdown setup. This is deliberately done here instead of
        // in `CloseRequested` so no CEF call is made from a window event.
        if self.shutdown_requested && !self.shutdown_browsers_closed {
            self.shutdown_browsers_closed = true;
            if let Some(tm) = self.tab_manager.as_mut() {
                tm.sync_session();
                tm.close_all_browsers();
            }
            if let Some(storage) = self.storage_handle.as_ref() {
                storage.flush_wal();
            }
        }

        // On macOS the external CEF pump runs from a main-thread CFRunLoop timer
        // (see CefPumpTimer) because even `about_to_wait` is inside winit's
        // single-entry event handler. Other platforms have no such guard issue.
        #[cfg(not(target_os = "macos"))]
        cef::do_message_loop_work();

        if let Some(tm) = self.tab_manager.as_mut() {
            tm.pump_events();
            if tm.consume_command_palette_request() {
                self.chrome_state.open_command_palette();
            }
            if !self.shutdown_requested {
                tm.tick();
            }
            tm.update_chrome_state(&mut self.chrome_state);
        }

        self.sync_palette_overlay(event_loop);
        self.sync_tooltip_overlay(event_loop);

        if self.shutdown_requested {
            self.shutdown_ticks += 1;
            let pending = self
                .tab_manager
                .as_ref()
                .map_or(0, |tm| tm.pending_browser_closes());

            if pending == 0 || self.shutdown_ticks >= SHUTDOWN_MAX_WAIT_TICKS {
                if pending > 0 {
                    tracing::info!(
                        "Proceeding to CEF shutdown with {} close callback(s) pending",
                        pending
                    );
                }
                tracing::info!(
                    "CEF shutdown ready (waited {} ticks) — exiting event loop",
                    self.shutdown_ticks
                );
                // On macOS winit installs CFRunLoop observers that are torn
                // down when run_app() returns. CEF shutdown dispatches NSApp
                // events, so it must happen while those observers still exist
                // (but after the CEF pump timer has been disabled).
                #[cfg(target_os = "macos")]
                {
                    CEF_PUMP_ENABLED.store(false, Ordering::Release);
                    cef::shutdown();
                }
                event_loop.exit();
                return;
            }

            // Keep ticking until every on_before_close callback has been observed.
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
            return;
        }

        // Request a redraw every frame so the chrome stays responsive.
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    // ── 1. macOS: load CEF framework dynamic library FIRST ──────────────────
    #[cfg(target_os = "macos")]
    let current_exe = std::env::current_exe().unwrap_or_default();
    #[cfg(target_os = "macos")]
    let (_loader, framework_dir, running_from_bundle) = {
        // A real .app bundle has <App>.app/Contents/MacOS/<exe> plus an
        // Info.plist. Checking only for target/Frameworks is not enough: CEF's
        // build script leaves that directory behind for dev builds too.
        let bundle_contents = current_exe
            .parent()
            .and_then(|macos_dir| macos_dir.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let bundle_info_plist = bundle_contents.join("Info.plist");
        let bundle_framework =
            bundle_contents.join("Frameworks/Chromium Embedded Framework.framework");
        let is_bundled = bundle_info_plist.exists() && bundle_framework.exists();

        if is_bundled {
            let loader = cef::library_loader::LibraryLoader::new(&current_exe, false);
            let loaded = loader.load();
            if !loaded {
                eprintln!("[Kasumi Browser] FATAL: CEF framework library load failed");
                std::process::exit(1);
            }
            (Some(loader), Some(bundle_framework), true)
        } else {
            let fallback_dir = PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/.cef/cef_macos_x86_64/Chromium Embedded Framework.framework"
            ));
            let framework_binary = fallback_dir.join("Chromium Embedded Framework");
            use std::os::unix::ffi::OsStrExt;
            let loaded = if let Ok(cpath) =
                std::ffi::CString::new(framework_binary.as_os_str().as_bytes())
            {
                unsafe { cef::load_library(Some(&*cpath.as_ptr().cast())) == 1 }
            } else {
                false
            };
            if !loaded {
                eprintln!("[Kasumi Browser] FATAL: dev fallback CEF framework load failed");
                std::process::exit(1);
            }
            (None, Some(fallback_dir), false)
        }
    };

    // Set CEF API version hash immediately after framework load
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

    // ── 2. Subprocess fork trap ──────────────────────────────────────────────
    // Chromium re-executes the main binary for renderers/GPU/utility processes.
    // execute_process MUST run before winit / tokio / wgpu / rusqlite.
    let args = cef::args::Args::new();
    let mut app = KasumiBrowserApp::new();

    let exit_code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if exit_code >= 0 {
        std::process::exit(exit_code);
    }

    // ── 3. Logging (main browser process only) ───────────────────────────────
    tracing_subscriber::fmt::init();
    tracing::info!("Kasumi Browser starting (PID: {})", std::process::id());

    // ── 4. macOS: configure NSApplication CrAppControlProtocol ──────────────
    #[cfg(target_os = "macos")]
    setup_mac_application();

    // ── 5. Initialize CEF ────────────────────────────────────────────────────
    let mut settings = cef::Settings::default();
    settings.external_message_pump = 1;
    settings.multi_threaded_message_loop = 0;
    settings.windowless_rendering_enabled = 0; // native child window, never OSR
    settings.no_sandbox = 1;

    let cache_dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target/cef_cache"));
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        tracing::warn!("Could not create CEF cache dir: {}", e);
    }
    if let Some(s) = cache_dir.to_str() {
        settings.root_cache_path = CefString::from(s);
        settings.cache_path = CefString::from(s);
    }

    #[cfg(target_os = "macos")]
    if let Some(ref fw_path) = framework_dir {
        if let Some(fw_str) = fw_path.to_str() {
            settings.framework_dir_path = CefString::from(fw_str);
            let res_dir = fw_path.join("Resources");
            if let Some(res_str) = res_dir.to_str() {
                settings.resources_dir_path = CefString::from(res_str);
                settings.locales_dir_path = CefString::from(res_str);
            }
        }
    }

    // macOS CEF requires either a helper app inside a .app bundle or an
    // absolute subprocess executable path. When running unbundled (cargo run /
    // direct binary), re-exec this binary for renderer/GPU/utility processes.
    #[cfg(target_os = "macos")]
    if !running_from_bundle {
        if let Some(exe) = current_exe.to_str() {
            settings.browser_subprocess_path = CefString::from(exe);
            tracing::info!("Unbundled CEF subprocess path: {}", exe);
        }
    }

    let init_result = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if init_result != 1 {
        tracing::error!("CEF initialization failed (code: {})", init_result);
        std::process::exit(1);
    }
    tracing::info!("CEF initialized successfully");

    // macOS: pump CEF from a main-thread run-loop timer, outside winit's
    // single-entry event handler. This timer is invalidated before shutdown.
    #[cfg(target_os = "macos")]
    let cef_pump_timer = CefPumpTimer::start();

    // ── 6. Initialize wgpu (blocking — before winit event loop) ─────────────
    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    // We need an adapter. Because we don't have a surface yet (window not created)
    // we request with no surface compatibility constraint. This works on macOS/Metal.
    let adapter =
        match pollster::block_on(wgpu_instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })) {
            Some(a) => Arc::new(a),
            None => {
                tracing::error!("No suitable wgpu adapter found");
                cef::shutdown();
                std::process::exit(1);
            }
        };

    let (device, queue) = match pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("kasumi_browser"),
            required_features: wgpu::Features::empty(),
            // Use the adapter's actual limits so the surface can match the
            // physical window size on Retina/HiDPI displays (downlevel_defaults
            // caps max_texture_dimension_2d at 2048, too small for 2560×1600).
            required_limits: adapter.limits(),
            memory_hints: Default::default(),
        },
        None,
    )) {
        Ok(dq) => (Arc::new(dq.0), Arc::new(dq.1)),
        Err(e) => {
            tracing::error!("wgpu device request failed: {}", e);
            cef::shutdown();
            std::process::exit(1);
        }
    };

    tracing::info!("wgpu adapter: {:?}", adapter.get_info().name);

    // ── 7. Run winit event loop ───────────────────────────────────────────────
    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            tracing::error!("Failed to create event loop: {}", e);
            cef::shutdown();
            std::process::exit(1);
        }
    };

    let mut handler = KasumiBrowserHandler::new(wgpu_instance, adapter, device, queue);

    if let Err(e) = event_loop.run_app(&mut handler) {
        tracing::error!("Event loop error: {}", e);
    }

    // macOS CEF shutdown already ran in about_to_wait while winit's run-loop
    // observers were still alive. Leak the timer so its destructor cannot touch
    // CoreFoundation after shutdown; the callback itself is already disabled.
    #[cfg(target_os = "macos")]
    std::mem::forget(cef_pump_timer);

    #[cfg(not(target_os = "macos"))]
    cef::shutdown();

    tracing::info!("Kasumi Browser exiting cleanly");
}

#[cfg(test)]
mod tests {
    use super::{is_primary_shortcut_modifier, overlay_screen_position};
    use winit::dpi::PhysicalPosition;
    use winit::keyboard::ModifiersState;

    #[test]
    fn shortcuts_accept_ctrl_or_cmd_without_conflicting_modifiers() {
        assert!(!is_primary_shortcut_modifier(ModifiersState::empty()));
        assert!(is_primary_shortcut_modifier(ModifiersState::CONTROL));
        assert!(is_primary_shortcut_modifier(ModifiersState::SUPER));
        assert!(is_primary_shortcut_modifier(
            ModifiersState::CONTROL | ModifiersState::SHIFT
        ));
        assert!(is_primary_shortcut_modifier(
            ModifiersState::SUPER | ModifiersState::ALT
        ));
        assert!(!is_primary_shortcut_modifier(ModifiersState::SHIFT));
        assert!(!is_primary_shortcut_modifier(ModifiersState::ALT));
    }

    #[test]
    fn overlay_position_uses_content_origin_and_pixel_scale() {
        let position = overlay_screen_position(PhysicalPosition::new(100, 40), (188.0, 174.0), 2.0);
        assert_eq!((position.x, position.y), (476, 388));

        let position = overlay_screen_position(PhysicalPosition::new(100, 40), (52.0, 120.5), 1.0);
        assert_eq!((position.x, position.y), (152, 161));
    }
}
