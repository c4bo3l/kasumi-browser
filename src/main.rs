#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod browser;
mod config;
mod events;
mod storage;
mod ui;

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;

use app::KasumiBrowserApp;
use browser::hibernation::TabManager;
use cef::CefString;
use config::{rail_width, BrowserPreferences, SHUTDOWN_MAX_WAIT_TICKS};
use events::{BrowserShortcut, UiAction};
use storage::db::{StorageHandle, StorageManager};
use ui::chrome::{
    ChromeOverlayRenderer, ChromeRenderer, ChromeState, ChromeTheme, TooltipRenderer,
};
use ui::platform_view::ContentRect;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId, WindowLevel};

/// Custom winit events used only by the macOS run-loop bridge. Other
/// platforms keep the empty event type so the winit handler shape is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KasumiEvent {
    /// Sent after macOS CEF shutdown has run outside winit's single-entry
    /// event handler so the loop can exit safely.
    #[cfg(target_os = "macos")]
    ShutdownComplete,
    /// A shell shortcut captured by a macOS local NSEvent monitor before the
    /// key reaches the CEF child view. Re-dispatched through the winit event
    /// loop so all UI mutations stay on the main thread.
    #[cfg(target_os = "macos")]
    NativeShortcut(BrowserShortcut),
}

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
    pub type CFRunLoopObserverRef = *mut c_void;
    pub type CFStringRef = *const c_void;
    pub type CFIndex = isize;
    pub type CFOptionFlags = usize;
    pub type CFRunLoopTimerCallBack = unsafe extern "C" fn(CFRunLoopTimerRef, *mut c_void);
    pub type CFRunLoopObserverCallBack =
        unsafe extern "C" fn(CFRunLoopObserverRef, u32, *mut c_void);

    #[repr(C)]
    pub struct CFRunLoopObserverContext {
        pub version: CFIndex,
        pub info: *mut c_void,
        pub retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
        pub release: Option<unsafe extern "C" fn(*const c_void)>,
        pub copy_description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    }

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
        pub fn CFRunLoopObserverCreate(
            allocator: CFAllocatorRef,
            activities: CFOptionFlags,
            repeats: u8,
            order: CFIndex,
            callout: CFRunLoopObserverCallBack,
            context: *const CFRunLoopObserverContext,
        ) -> CFRunLoopObserverRef;
        pub fn CFRunLoopAddObserver(
            run_loop: CFRunLoopRef,
            observer: CFRunLoopObserverRef,
            mode: CFStringRef,
        );
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

#[cfg(target_os = "macos")]
static CEF_SHUTDOWN_SCHEDULED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static CEF_SHUTDOWN_DONE: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static SHUTDOWN_PROXY: OnceLock<winit::event_loop::EventLoopProxy<KasumiEvent>> = OnceLock::new();

#[cfg(target_os = "macos")]
static SHORTCUT_PROXY: OnceLock<winit::event_loop::EventLoopProxy<KasumiEvent>> = OnceLock::new();

#[cfg(target_os = "macos")]
unsafe extern "C" fn cef_shutdown_observer_callback(
    _observer: cf::CFRunLoopObserverRef,
    _activity: u32,
    _info: *mut std::ffi::c_void,
) {
    // `cef::shutdown()` dispatches NSApp events that can re-enter
    // `about_to_wait` and try to schedule another shutdown. The done flag is
    // set before the call, so those nested ticks become no-ops.
    CEF_SHUTDOWN_SCHEDULED.store(false, Ordering::Release);
    if CEF_SHUTDOWN_DONE.swap(true, Ordering::AcqRel) {
        return;
    }

    // Crucially, this runs from the run loop instead of from winit's
    // `about_to_wait` handler. `cef::shutdown()` dispatches NSApp events, and
    // doing that inside winit's single-entry handler panics with
    // "tried to handle event while another event is currently being handled".
    tracing::info!("Running deferred CEF shutdown outside the winit event handler");
    cef::shutdown();

    if let Some(proxy) = SHUTDOWN_PROXY.get() {
        let _ = proxy.send_event(KasumiEvent::ShutdownComplete);
    }
}

#[cfg(target_os = "macos")]
struct CefShutdownObserver;

#[cfg(target_os = "macos")]
impl CefShutdownObserver {
    /// Schedule a one-shot shutdown on the next run-loop pass, after the
    /// current winit event handler has returned.
    fn schedule() {
        if CEF_SHUTDOWN_DONE.load(Ordering::Acquire)
            || CEF_SHUTDOWN_SCHEDULED.swap(true, Ordering::AcqRel)
        {
            return;
        }

        let context = cf::CFRunLoopObserverContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copy_description: None,
        };

        unsafe {
            // kCFRunLoopBeforeWaiting | kCFRunLoopAfterWaiting
            let activities: cf::CFOptionFlags = (1 << 5) | (1 << 6);
            let observer = cf::CFRunLoopObserverCreate(
                std::ptr::null_mut(),
                activities,
                0, // one-shot
                0, // order
                cef_shutdown_observer_callback,
                &context,
            );
            if observer.is_null() {
                CEF_SHUTDOWN_SCHEDULED.store(false, Ordering::Release);
                tracing::error!("Failed to create CEF shutdown observer");
                return;
            }
            cf::CFRunLoopAddObserver(cf::CFRunLoopGetMain(), observer, cf::kCFRunLoopCommonModes);
            // Intentionally leaked: the observer invalidates itself after its
            // single fire, and releasing it after CEF shutdown is unsafe.
        }
    }
}

// ---------------------------------------------------------------------------
// macOS native shortcut monitor.
//
// CEF's `on_pre_key_event` is not a reliable place for browser-chrome
// shortcuts on macOS: AppKit/CEF can consume `Cmd+,` before CEF sees it. An
// NSEvent local monitor runs earlier in the same main-thread event dispatch,
// so it can recognize shell shortcuts even while the CEF child view has focus
// and forward them to the winit event loop.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod native_shortcuts {
    use crate::events::BrowserShortcut;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSKeyDownMask};
    use std::ptr::NonNull;

    use super::{KasumiEvent, SHORTCUT_PROXY};

    const MAC_KEYCODE_F12: u16 = 0x58;

    /// Install the monitor and return it so the handler can keep it alive for
    /// the lifetime of the event loop.
    pub(crate) fn install() -> Option<Retained<AnyObject>> {
        let Some(proxy) = SHORTCUT_PROXY.get().cloned() else {
            tracing::warn!("Native shortcut monitor skipped: event loop proxy unavailable");
            return None;
        };

        let handler: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent> =
            RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
                let event_ref = unsafe { event.as_ref() };
                let Some(shortcut) = shortcut_from_event(event_ref) else {
                    return event.as_ptr();
                };
                if proxy
                    .send_event(KasumiEvent::NativeShortcut(shortcut))
                    .is_ok()
                {
                    tracing::info!("Native shortcut captured: {:?}", shortcut);
                    // Returning nil stops AppKit from also delivering this
                    // key equivalent to winit or the web page.
                    std::ptr::null_mut()
                } else {
                    event.as_ptr()
                }
            });

        // SAFETY: the block's signature matches NSEvent's declared API, and the
        // returned monitor is retained by the caller for the app's lifetime.
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSKeyDownMask, &handler) }
    }

    fn shortcut_from_event(event: &NSEvent) -> Option<BrowserShortcut> {
        if unsafe { event.isARepeat() } {
            return None;
        }
        let flags = unsafe { event.modifierFlags() };
        let key_code = unsafe { event.keyCode() };
        let characters = unsafe { event.charactersIgnoringModifiers() }?.to_string();
        let key = characters.chars().next()?.to_ascii_lowercase();
        shortcut_from_parts(key, key_code, flags)
    }

    /// Pure classifier so the shortcut mapping is unit-testable without
    /// constructing native NSEvents.
    pub(crate) fn shortcut_from_parts(
        key: char,
        key_code: u16,
        flags: NSEventModifierFlags,
    ) -> Option<BrowserShortcut> {
        if key_code == MAC_KEYCODE_F12 {
            return Some(BrowserShortcut::OpenDevTools);
        }

        let primary = flags.contains(NSEventModifierFlags::NSEventModifierFlagCommand)
            || flags.contains(NSEventModifierFlags::NSEventModifierFlagControl);
        let shift = flags.contains(NSEventModifierFlags::NSEventModifierFlagShift);
        let option = flags.contains(NSEventModifierFlags::NSEventModifierFlagOption);
        if !primary || option {
            return None;
        }

        match key {
            't' if shift => Some(BrowserShortcut::ReopenClosedTab),
            't' => Some(BrowserShortcut::NewTab),
            'w' if !shift => Some(BrowserShortcut::CloseTab),
            'l' | 'k' if !shift => Some(BrowserShortcut::OpenCommandPalette),
            ',' | '.' if !shift => Some(BrowserShortcut::OpenSettings),
            'z' if !shift => Some(BrowserShortcut::ReopenClosedTab),
            _ => None,
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

    fn send_menu_shortcut(shortcut: BrowserShortcut) {
        if let Some(proxy) = SHORTCUT_PROXY.get() {
            let _ = proxy.send_event(KasumiEvent::NativeShortcut(shortcut));
        }
    }

    extern "C" fn menu_new_tab(_this: *mut c_void, _cmd: *mut c_void) {
        send_menu_shortcut(BrowserShortcut::NewTab);
    }

    extern "C" fn menu_close_tab(_this: *mut c_void, _cmd: *mut c_void) {
        send_menu_shortcut(BrowserShortcut::CloseTab);
    }

    extern "C" fn menu_open_settings(_this: *mut c_void, _cmd: *mut c_void) {
        send_menu_shortcut(BrowserShortcut::OpenSettings);
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

            // Target/action selectors for the native macOS File menu. The menu
            // is installed later, once the winit event loop proxy is ready.
            let register_action = |name: &str, imp: extern "C" fn(*mut c_void, *mut c_void)| {
                let sel =
                    objc2::ffi::sel_registerName(CString::new(name).unwrap_or_default().as_ptr());
                let types = CString::new("v@:").unwrap_or_default();
                let imp: objc2::ffi::IMP = Some(std::mem::transmute(imp as *const ()));
                objc2::ffi::class_addMethod(cls_mut, sel, imp, types.as_ptr());
            };
            register_action("kasumiNewTab:", menu_new_tab);
            register_action("kasumiCloseTab:", menu_close_tab);
            register_action("kasumiOpenSettings:", menu_open_settings);

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

/// Build the native macOS menu bar. The File menu mirrors the shell shortcuts
/// so Settings is discoverable without knowing `Cmd+,`.
#[cfg(target_os = "macos")]
fn install_macos_main_menu() {
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::{MainThreadMarker, NSString};

    // This function is only called from the main thread in `resumed()`.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);

    let main_menu = NSMenu::new(mtm);
    unsafe { main_menu.setTitle(&NSString::from_str("Kasumi Browser")) };

    let app_item = NSMenuItem::new(mtm);
    unsafe { app_item.setTitle(&NSString::from_str("Kasumi Browser")) };
    let app_menu = NSMenu::new(mtm);
    let app_version = NSMenuItem::new(mtm);
    unsafe {
        app_version.setTitle(&NSString::from_str("Kasumi Browser 0.5.2"));
        app_version.setEnabled(false);
    }
    app_menu.addItem(&app_version);
    app_item.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_item);

    let file_item = NSMenuItem::new(mtm);
    unsafe { file_item.setTitle(&NSString::from_str("File")) };
    let file_menu = NSMenu::new(mtm);
    file_item.setSubmenu(Some(&file_menu));
    main_menu.addItem(&file_item);

    let target: &AnyObject = app.as_ref();
    let add_item = |title: &str, selector: objc2::runtime::Sel, key: &str| {
        let item = unsafe {
            file_menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str(title),
                Some(selector),
                &NSString::from_str(key),
            )
        };
        unsafe { item.setTarget(Some(target)) };
    };

    add_item("New Tab", objc2::sel!(kasumiNewTab:), "t");
    add_item("Close Tab", objc2::sel!(kasumiCloseTab:), "w");
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_item("Settings…", objc2::sel!(kasumiOpenSettings:), ",");

    app.setMainMenu(Some(&main_menu));
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

    // Transparent always-on-top overlay shared by the command palette and the
    // rail tab context menu. The CEF child view stays visible behind it.
    overlay_window: Option<Arc<Window>>,
    overlay_window_id: Option<WindowId>,
    overlay: Option<ChromeOverlayRenderer>,
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
    // Set once CEF shutdown is ready so reentrant `about_to_wait` ticks do not
    // re-log the same decision or reschedule the one-shot run-loop observer.
    shutdown_finalization_requested: bool,

    // Kept alive so macOS continues intercepting shell shortcuts while the
    // CEF child view owns keyboard focus.
    #[cfg(target_os = "macos")]
    shortcut_monitor: Option<objc2::rc::Retained<objc2::runtime::AnyObject>>,
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
            shutdown_finalization_requested: false,
            #[cfg(target_os = "macos")]
            shortcut_monitor: None,
        }
    }

    fn sync_chrome_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if self.shutdown_requested {
            self.hide_chrome_overlay();
            return;
        }
        if self.chrome_state.has_overlay() {
            self.ensure_chrome_overlay(event_loop);
        } else {
            self.hide_chrome_overlay();
        }
    }

    fn ensure_chrome_overlay(&mut self, event_loop: &ActiveEventLoop) {
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
            let Some(overlay) = ChromeOverlayRenderer::new(
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
            self.position_chrome_overlay();
        }
        if let Some(window) = self.overlay_window.as_ref() {
            if !self.overlay_visible {
                window.set_visible(true);
                // AppKit can re-center a newly visible window on the primary
                // display, so reapply the main window's current position after
                // the overlay is visible as well as before it is shown.
                self.position_chrome_overlay();
                window.focus_window();
                self.overlay_visible = true;
            }
            window.request_redraw();
        }
    }

    fn position_chrome_overlay(&self) {
        let (Some(main), Some(overlay)) = (self.window.as_ref(), self.overlay_window.as_ref())
        else {
            return;
        };
        let _ = overlay.request_inner_size(main.inner_size());
        // The overlay is undecorated, so its outer origin is its content origin.
        // Anchor it to the main window's current content origin to account for
        // macOS title bars and for windows moved to another display; egui
        // coordinates are relative to that content area.
        let target = match main.inner_position().or_else(|_| main.outer_position()) {
            Ok(origin) => overlay_screen_position(origin, (0.0, 0.0), main.scale_factor()),
            Err(_) => {
                // Last-resort fallback for platforms that cannot report a
                // window origin: center the overlay on the monitor containing
                // the main window so a multi-display move never leaves it on
                // the primary screen.
                let Some(monitor) = main.current_monitor() else {
                    return;
                };
                let monitor_origin = monitor.position();
                let monitor_size = monitor.size();
                let window_size = main.outer_size();
                winit::dpi::PhysicalPosition::new(
                    monitor_origin.x
                        + (monitor_size.width.saturating_sub(window_size.width) / 2) as i32,
                    monitor_origin.y
                        + (monitor_size.height.saturating_sub(window_size.height) / 2) as i32,
                )
            }
        };
        overlay.set_outer_position(target);
    }

    fn hide_chrome_overlay(&mut self) {
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
        if self.shutdown_requested || self.chrome_state.has_overlay() {
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

    /// Dispatch a browser shortcut once, regardless of whether it came from
    /// winit, CEF, or the macOS native event monitor.
    fn handle_browser_shortcut(&mut self, shortcut: BrowserShortcut) {
        match shortcut {
            BrowserShortcut::NewTab => {
                if let Some(tm) = self.tab_manager.as_mut() {
                    tm.handle_ui_action(UiAction::NewTab { url: None });
                }
            }
            BrowserShortcut::CloseTab => {
                if let Some(tm) = self.tab_manager.as_mut() {
                    if let Some(id) = tm.active_tab_id {
                        tm.handle_ui_action(UiAction::CloseTab { tab_id: id });
                    }
                }
            }
            BrowserShortcut::ReopenClosedTab => {
                if let Some(tm) = self.tab_manager.as_mut() {
                    tm.reopen_closed_tab();
                }
            }
            BrowserShortcut::OpenCommandPalette => {
                self.chrome_state.open_command_palette();
            }
            BrowserShortcut::OpenDevTools => {
                if let Some(tm) = self.tab_manager.as_mut() {
                    if let Some(id) = tm.active_tab_id {
                        tm.handle_ui_action(UiAction::OpenDevTools { tab_id: id });
                    }
                }
            }
            BrowserShortcut::OpenSettings => {
                self.chrome_state.open_settings();
            }
        }
    }

    /// Persist and apply a preference document emitted by the Settings UI.
    fn apply_preferences(&mut self, preferences: BrowserPreferences) {
        let preferences = preferences.normalized();
        let rail_default_changed =
            self.chrome_state.preferences.rail_expanded != preferences.rail_expanded;
        self.chrome_state.set_preferences(preferences.clone());
        tracing::info!(
            "Applied preferences: search={}, home={}, theme={:?}, adblock={}, idle={}m",
            preferences.search_engine.label(),
            preferences.home_url(),
            preferences.theme,
            preferences.adblock_enabled,
            preferences.idle_timeout_minutes
        );

        if let Some(storage) = self.storage_handle.as_ref() {
            storage.save_preferences(preferences.clone());
        }

        if rail_default_changed {
            self.chrome_state.rail_expanded = preferences.rail_expanded;
            self.resize_content_to_rail();
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Recompute the CEF child-view rect after a rail width change.
    fn resize_content_to_rail(&mut self) {
        let (Some(window), Some(tm)) = (self.window.as_ref(), self.tab_manager.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let scale = window.scale_factor();
        let rect = ContentRect::from_window(
            size.width,
            size.height,
            scale,
            rail_width(self.chrome_state.rail_expanded),
        );
        tracing::info!("Workspace rail width changed; CEF content rect: {:?}", rect);
        tm.resize(rect);
    }

    /// Dispatch UI actions once, including preference persistence, so both the
    /// main window and the overlay window apply settings identically.
    fn handle_ui_actions(&mut self, actions: Vec<UiAction>) {
        for action in actions {
            if let UiAction::ApplyPreferences(preferences) = &action {
                self.apply_preferences(preferences.clone());
            }
            if let Some(tm) = self.tab_manager.as_mut() {
                tm.handle_ui_action(action);
            }
        }
    }
}

/// True when a main-window pointer position is over the CEF content area
/// instead of the native rail. Used to clear cached rail tooltips when the
/// cursor crosses into the child view, where egui receives no hover updates.
fn pointer_is_past_rail(physical_x: f64, scale_factor: f64, rail_expanded: bool) -> bool {
    scale_factor > 0.0 && physical_x / scale_factor >= rail_width(rail_expanded) as f64
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

/// `Cmd/Ctrl+Z` restores the most recently closed tab. Shift is excluded so
/// `Cmd/Ctrl+Shift+Z` remains available for a future redo action.
fn is_reopen_tab_shortcut(modifiers: ModifiersState, key: &Key) -> bool {
    is_primary_shortcut_modifier(modifiers)
        && !modifiers.alt_key()
        && !modifiers.shift_key()
        && matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("z"))
}

impl ApplicationHandler<KasumiEvent> for KasumiBrowserHandler {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: KasumiEvent) {
        #[cfg(target_os = "macos")]
        match event {
            KasumiEvent::ShutdownComplete => {
                tracing::info!("CEF shutdown complete — exiting event loop");
                event_loop.exit();
            }
            KasumiEvent::NativeShortcut(shortcut) => {
                self.handle_browser_shortcut(shortcut);
                self.sync_chrome_overlay(event_loop);
                self.sync_tooltip_overlay(event_loop);
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (event_loop, event);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already created
        }

        let db_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/kasumi_browser.db"
        ));
        let preferences = StorageManager::load_preferences(&db_path);

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

        // Follow the OS appearance and stored preferences from the very first
        // frame, not only after the first `ThemeChanged` event arrives.
        self.chrome_state.set_system_theme(ChromeTheme::from_winit(
            event_loop.system_theme().or_else(|| window.theme()),
        ));
        self.chrome_state.set_preferences(preferences.clone());
        self.chrome_state.rail_expanded = preferences.rail_expanded;
        tracing::info!(
            "System theme: {:?}, chrome theme: {:?}",
            self.chrome_state.system_theme,
            self.chrome_state.theme
        );

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

        let storage_mgr = StorageManager::new(db_path.clone());
        let storage_handle = storage_mgr.start_worker();
        let restored_tabs = StorageManager::load_session(&db_path).unwrap_or_default();

        let (tx, rx) = crossbeam_channel::unbounded();
        let mut tab_manager = TabManager::new(
            tx,
            rx,
            parent_view,
            rect,
            Some(storage_handle.clone()),
            &preferences,
        );
        tab_manager.restore_session(restored_tabs);

        self.window = Some(Arc::clone(&window));
        self.chrome = Some(chrome);
        self.tab_manager = Some(tab_manager);
        self.storage_handle = Some(storage_handle);

        // Capture shell shortcuts before AppKit can hand them to the CEF child
        // view. The monitor is owned by this handler and removed on shutdown.
        #[cfg(target_os = "macos")]
        {
            install_macos_main_menu();
            self.shortcut_monitor = native_shortcuts::install();
        }

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
                    self.chrome_state.close_active_overlay();
                    self.sync_chrome_overlay(event_loop);
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
                    self.handle_ui_actions(actions);
                    self.sync_chrome_overlay(event_loop);
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
                WindowEvent::CloseRequested | WindowEvent::CursorLeft { .. } => {
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
                            renderer.render(window, &tooltip, self.chrome_state.theme);
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
            WindowEvent::ThemeChanged(theme) => {
                let next = ChromeTheme::from_winit(Some(*theme));
                let before = self.chrome_state.theme;
                self.chrome_state.set_system_theme(next);
                if before != self.chrome_state.theme {
                    tracing::info!(
                        "System theme changed to {:?}; chrome theme is {:?}",
                        next,
                        self.chrome_state.theme
                    );
                    if let Some(window) = self.window.as_ref() {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                // The pointer may leave the rail without egui receiving a
                // hover update (for example when it enters the CEF child
                // view). Clear the cached tooltip immediately.
                self.chrome_state.tooltip = None;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self
                    .window
                    .as_ref()
                    .map_or(1.0, |window| window.scale_factor());
                if pointer_is_past_rail(position.x, scale, self.chrome_state.rail_expanded) {
                    self.chrome_state.tooltip = None;
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        repeat: false,
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
                } else if is_reopen_tab_shortcut(self.modifiers, logical_key) {
                    if let Some(tm) = self.tab_manager.as_mut() {
                        tm.handle_ui_action(UiAction::ReopenClosedTab);
                    }
                } else {
                    match logical_key {
                        Key::Named(NamedKey::F12) => {
                            self.handle_browser_shortcut(BrowserShortcut::OpenDevTools);
                        }
                        Key::Character(c) if ctrl => match c.as_str() {
                            "l" | "L" | "k" | "K" => {
                                self.handle_browser_shortcut(BrowserShortcut::OpenCommandPalette);
                            }
                            "," | "<" | "." | ">" => {
                                self.handle_browser_shortcut(BrowserShortcut::OpenSettings);
                            }
                            "t" | "T" => {
                                self.handle_browser_shortcut(BrowserShortcut::NewTab);
                            }
                            "w" | "W" => {
                                self.handle_browser_shortcut(BrowserShortcut::CloseTab);
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
            WindowEvent::Moved(_) => {
                // Keep helper windows on the same display as the browser when
                // the user drags the main window between monitors.
                self.position_chrome_overlay();
                self.chrome_state.tooltip = None;
                self.hide_tooltip_overlay();
            }
            WindowEvent::Resized(new_size) => {
                if let Some(chrome) = self.chrome.as_mut() {
                    chrome.resize(*new_size);
                }
                self.position_chrome_overlay();
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
                if let (Some(window), Some(chrome)) = (self.window.as_ref(), self.chrome.as_mut()) {
                    // CEF events and the watchdog are processed once per loop
                    // iteration in `about_to_wait`, never here. This keeps all
                    // CEF work in one non-window-event callback and prevents
                    // double-processing.
                    let actions = chrome.render(window, &mut self.chrome_state);
                    self.handle_ui_actions(actions);
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

        self.sync_chrome_overlay(event_loop);
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
            if tm.consume_settings_request() {
                self.chrome_state.open_settings();
            }
            if !self.shutdown_requested {
                tm.tick();
            }
            tm.update_chrome_state(&mut self.chrome_state);
        }

        self.sync_chrome_overlay(event_loop);
        self.sync_tooltip_overlay(event_loop);

        if self.shutdown_requested {
            // The one-shot observer may not fire until the run loop is allowed
            // to wait. Until then, this state is final; just wait quietly.
            if self.shutdown_finalization_requested {
                return;
            }

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
                self.shutdown_finalization_requested = true;
                // On macOS winit installs CFRunLoop observers that are torn
                // down when run_app() returns, so CEF shutdown must happen
                // before returning from run_app(). It also dispatches NSApp
                // events, so run it from a one-shot run-loop observer rather
                // than inside this winit handler to avoid reentrant dispatch.
                #[cfg(target_os = "macos")]
                {
                    CEF_PUMP_ENABLED.store(false, Ordering::Release);
                    CefShutdownObserver::schedule();
                    // Do not exit yet; the observer sends ShutdownComplete
                    // after `cef::shutdown()` has finished.
                    return;
                }

                #[cfg(not(target_os = "macos"))]
                {
                    cef::shutdown();
                    event_loop.exit();
                    return;
                }
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
    let event_loop = match EventLoop::<KasumiEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            tracing::error!("Failed to create event loop: {}", e);
            cef::shutdown();
            std::process::exit(1);
        }
    };

    #[cfg(target_os = "macos")]
    if SHUTDOWN_PROXY.set(event_loop.create_proxy()).is_err() {
        tracing::warn!("Shutdown event proxy was already initialized");
    }

    #[cfg(target_os = "macos")]
    if SHORTCUT_PROXY.set(event_loop.create_proxy()).is_err() {
        tracing::warn!("Native shortcut event proxy was already initialized");
    }

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
    use super::{
        is_primary_shortcut_modifier, is_reopen_tab_shortcut, overlay_screen_position,
        pointer_is_past_rail, BrowserShortcut,
    };
    use winit::dpi::PhysicalPosition;
    use winit::keyboard::{Key, ModifiersState};

    #[cfg(target_os = "macos")]
    use {
        super::native_shortcuts::shortcut_from_parts, objc2_app_kit::NSEventModifierFlags as Flags,
    };

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
    fn reopen_tab_shortcut_requires_primary_modifier_without_shift_or_alt() {
        let z: Key = Key::Character("z".into());
        let upper_z: Key = Key::Character("Z".into());

        assert!(is_reopen_tab_shortcut(ModifiersState::CONTROL, &z));
        assert!(is_reopen_tab_shortcut(ModifiersState::SUPER, &upper_z));
        assert!(!is_reopen_tab_shortcut(ModifiersState::empty(), &z));
        assert!(!is_reopen_tab_shortcut(ModifiersState::SHIFT, &z));
        assert!(!is_reopen_tab_shortcut(
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            &z
        ));
        assert!(!is_reopen_tab_shortcut(
            ModifiersState::CONTROL | ModifiersState::ALT,
            &z
        ));
        assert!(!is_reopen_tab_shortcut(
            ModifiersState::CONTROL,
            &Key::Character("t".into())
        ));
    }

    #[test]
    fn pointer_past_rail_uses_logical_rail_width() {
        // Expanded rail is 180 logical points: 360 physical pixels at 2x.
        assert!(!pointer_is_past_rail(359.0, 2.0, true));
        assert!(pointer_is_past_rail(360.0, 2.0, true));

        // Collapsed rail is 44 logical points.
        assert!(!pointer_is_past_rail(43.0, 1.0, false));
        assert!(pointer_is_past_rail(44.0, 1.0, false));

        // Invalid scale factors must not clear tooltips spuriously.
        assert!(!pointer_is_past_rail(1_000.0, 0.0, true));
    }

    #[test]
    fn overlay_position_uses_content_origin_and_pixel_scale() {
        let position = overlay_screen_position(PhysicalPosition::new(100, 40), (188.0, 174.0), 2.0);
        assert_eq!((position.x, position.y), (476, 388));

        let position = overlay_screen_position(PhysicalPosition::new(100, 40), (52.0, 120.5), 1.0);
        assert_eq!((position.x, position.y), (152, 161));
    }

    #[test]
    fn overlay_position_follows_secondary_display_origins() {
        // Displays to the left/above the primary display use negative physical
        // coordinates; the overlay must preserve those, not clamp to zero.
        let position =
            overlay_screen_position(PhysicalPosition::new(-1920, -1080), (0.0, 0.0), 2.0);
        assert_eq!((position.x, position.y), (-1920, -1080));

        let position =
            overlay_screen_position(PhysicalPosition::new(-1920, -1080), (100.0, 50.0), 2.0);
        assert_eq!((position.x, position.y), (-1720, -980));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_monitor_captures_settings_shortcuts_while_page_has_focus() {
        let cmd = Flags::NSEventModifierFlagCommand;
        assert_eq!(
            shortcut_from_parts(',', 0x2B, cmd),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            shortcut_from_parts('.', 0x2F, cmd),
            Some(BrowserShortcut::OpenSettings)
        );
        assert_eq!(
            shortcut_from_parts('<', 0x2B, cmd | Flags::NSEventModifierFlagShift),
            None,
            "Shift+Cmd+, is not a shell shortcut"
        );
        assert_eq!(
            shortcut_from_parts('a', 0x00, Flags::empty()),
            None,
            "plain typing must pass through to the page"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_monitor_maps_all_shell_shortcuts() {
        let cmd = Flags::NSEventModifierFlagCommand;
        let shift = Flags::NSEventModifierFlagShift;
        assert_eq!(
            shortcut_from_parts('t', 0x11, cmd),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            shortcut_from_parts('w', 0x13, cmd),
            Some(BrowserShortcut::CloseTab)
        );
        assert_eq!(
            shortcut_from_parts('l', 0x25, cmd),
            Some(BrowserShortcut::OpenCommandPalette)
        );
        assert_eq!(
            shortcut_from_parts('t', 0x11, cmd | shift),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            shortcut_from_parts('\0', 0x58, cmd),
            Some(BrowserShortcut::OpenDevTools)
        );
    }
}
