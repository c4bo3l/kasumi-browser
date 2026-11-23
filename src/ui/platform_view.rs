//! Cross-platform native child window embedding for CEF browser views.
//!
//! Each CEF browser renders into its own OS-native view (NSView on macOS,
//! HWND child window on Windows, X11 window on Linux). We embed that view
//! underneath our egui chrome by reparenting / repositioning it every time
//! the outer winit window resizes or the active tab changes.

#![allow(dead_code)]

/// A rectangle describing where the CEF content should sit inside the outer
/// winit window, in logical points / device-independent pixels.
///
/// The vertical workspace rail occupies the left edge, so the content starts
/// at `x = rail_width` and extends to the full window height. macOS `NSView`
/// frames and CEF's `WindowInfo.bounds` both use this unit. Platforms whose
/// APIs take physical pixels (Windows `MoveWindow`, X11) use `scale_factor`
/// to convert at the call site.
#[derive(Debug, Clone, Copy)]
pub struct ContentRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

impl ContentRect {
    /// Compute the logical content rect from the full window physical size and
    /// the logical width of the left workspace rail.
    pub fn from_window(
        physical_width: u32,
        physical_height: u32,
        scale_factor: f64,
        left_inset: f32,
    ) -> Self {
        let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        let logical_width = (physical_width as f64 / scale).round() as u32;
        let logical_height = (physical_height as f64 / scale).round() as u32;
        let x = if left_inset.is_finite() && left_inset > 0.0 {
            (left_inset.round() as u32).min(logical_width)
        } else {
            0
        };
        Self {
            x,
            y: 0,
            width: logical_width.saturating_sub(x),
            height: logical_height,
            scale_factor: scale,
        }
    }

    /// Convert a logical rect to physical pixels for APIs that need them.
    pub fn to_physical(self) -> (u32, u32, u32, u32) {
        let s = self.scale_factor;
        (
            (self.x as f64 * s).round() as u32,
            (self.y as f64 * s).round() as u32,
            (self.width as f64 * s).round() as u32,
            (self.height as f64 * s).round() as u32,
        )
    }
}

use cef::sys::cef_window_handle_t;

/// CEF's native window handle is a raw pointer on macOS, an `HWND` newtype on
/// Windows, and an XID integer on Linux. Every function in this module takes
/// `*mut c_void`, so these helpers keep that conversion in one audited place.
#[cfg(target_os = "macos")]
pub(crate) fn cef_handle_as_ptr(handle: cef_window_handle_t) -> *mut std::ffi::c_void {
    handle
}

#[cfg(target_os = "windows")]
pub(crate) fn cef_handle_as_ptr(handle: cef_window_handle_t) -> *mut std::ffi::c_void {
    // Windows CEF uses its own `HWND` newtype rather than a raw pointer.
    handle.0.cast::<std::ffi::c_void>()
}

#[cfg(target_os = "linux")]
pub(crate) fn cef_handle_as_ptr(handle: cef_window_handle_t) -> *mut std::ffi::c_void {
    handle as usize as *mut std::ffi::c_void
}

#[cfg(target_os = "macos")]
pub(crate) fn ptr_as_cef_handle(ptr: *mut std::ffi::c_void) -> cef_window_handle_t {
    ptr
}

#[cfg(target_os = "windows")]
pub(crate) fn ptr_as_cef_handle(ptr: *mut std::ffi::c_void) -> cef_window_handle_t {
    cef::sys::HWND(ptr.cast())
}

#[cfg(target_os = "linux")]
pub(crate) fn ptr_as_cef_handle(ptr: *mut std::ffi::c_void) -> cef_window_handle_t {
    ptr as usize as cef_window_handle_t
}

pub(crate) fn cef_handle_is_null(handle: cef_window_handle_t) -> bool {
    cef_handle_as_ptr(handle).is_null()
}

// ---------------------------------------------------------------------------
// macOS (NSView)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;

    use super::ContentRect;
    use objc2_app_kit::NSView;
    use objc2_foundation::NSRect;

    /// Attach the CEF `NSView` as a child of the winit window's content view,
    /// position it in `rect`, and make it visible.
    ///
    /// # Safety
    /// Both `parent` and `child` must be valid, live Objective-C object
    /// pointers obtained from raw-window-handle / CEF respectively.
    pub unsafe fn show_view(
        parent: *mut std::ffi::c_void,
        child_ptr: *mut std::ffi::c_void,
        rect: ContentRect,
    ) -> Result<(), String> {
        if parent.is_null() || child_ptr.is_null() {
            return Err("show_view received a null NSView pointer".to_string());
        }
        let parent_view = unsafe { &*parent.cast::<NSView>() };
        let child = unsafe { &*child_ptr.cast::<NSView>() };
        // Add first so set_frame() can query the parent's coordinate system.
        unsafe { parent_view.addSubview(child) };
        set_frame(child, &rect);
        child.setHidden(false);
        Ok(())
    }

    /// Reposition the CEF `NSView` without changing its parent.
    ///
    /// # Safety
    /// `child` must be a valid live `NSView *`.
    pub unsafe fn resize_view(
        child_ptr: *mut std::ffi::c_void,
        rect: ContentRect,
    ) -> Result<(), String> {
        if child_ptr.is_null() {
            return Ok(());
        }
        let child = unsafe { &*child_ptr.cast::<NSView>() };
        set_frame(child, &rect);
        Ok(())
    }

    /// Hide the CEF `NSView` (without destroying it or its browser process).
    ///
    /// # Safety
    /// `child` must be a valid live `NSView *`.
    pub unsafe fn hide_view(child_ptr: *mut std::ffi::c_void) -> Result<(), String> {
        if child_ptr.is_null() {
            return Ok(());
        }
        let child = unsafe { &*child_ptr.cast::<NSView>() };
        child.setHidden(true);
        Ok(())
    }

    /// Detach the CEF `NSView` from the winit content view. This completes the
    /// native view teardown that CEF requires after `DoClose` returns true.
    ///
    /// # Safety
    /// `cef_ns_view` must be a valid live `NSView *`.
    pub unsafe fn detach_view(cef_ns_view: *mut c_void) {
        if cef_ns_view.is_null() {
            return;
        }
        let child = unsafe { &*cef_ns_view.cast::<NSView>() };
        unsafe { child.removeFromSuperview() };
        child.setHidden(true);
    }

    fn set_frame(view: &NSView, rect: &ContentRect) {
        // winit's content view is flipped (upper-left origin), while a plain
        // NSView is not. Query the actual parent instead of assuming one.
        let (parent_height, parent_is_flipped) = if let Some(sv) = unsafe { view.superview() } {
            (sv.frame().size.height, sv.isFlipped())
        } else {
            ((rect.y + rect.height) as f64, true)
        };
        let y = if parent_is_flipped {
            rect.y as f64
        } else {
            parent_height - (rect.y as f64) - (rect.height as f64)
        };
        let ns_rect = NSRect::new(
            objc2_foundation::NSPoint::new(rect.x as f64, y),
            objc2_foundation::NSSize::new(rect.width as f64, rect.height as f64),
        );
        tracing::debug!(
            "CEF view frame: origin=({:.1}, {:.1}) size=({:.1} x {:.1}), parent_flipped={}",
            ns_rect.origin.x,
            ns_rect.origin.y,
            ns_rect.size.width,
            ns_rect.size.height,
            parent_is_flipped
        );
        unsafe { view.setFrame(ns_rect) };
    }
}

// ---------------------------------------------------------------------------
// Windows (HWND)
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use super::ContentRect;
    use windows_sys::Win32::Foundation::HWND;

    /// Reparent and position the CEF child `HWND`.
    ///
    /// # Safety
    /// `parent` and `child` must be valid, live Win32 window handles.
    pub unsafe fn show_view(
        parent: *mut c_void,
        child: *mut c_void,
        rect: ContentRect,
    ) -> Result<(), String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        if parent.is_null() || child.is_null() {
            return Err("show_view received a null HWND".to_string());
        }
        let (parent_hwnd, cef_hwnd): (HWND, HWND) = (parent, child);
        let (x, y, width, height) = rect.to_physical();
        unsafe {
            SetParent(cef_hwnd, parent_hwnd);
            MoveWindow(
                cef_hwnd,
                x as i32,
                y as i32,
                width as i32,
                height as i32,
                1, // repaint
            );
            ShowWindow(cef_hwnd, SW_SHOW);
        }
        Ok(())
    }

    /// # Safety
    /// `child` must be a valid, live Win32 window handle.
    pub unsafe fn resize_view(child: *mut c_void, rect: ContentRect) -> Result<(), String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        if child.is_null() {
            return Ok(());
        }
        let (x, y, width, height) = rect.to_physical();
        unsafe {
            MoveWindow(child, x as i32, y as i32, width as i32, height as i32, 1);
        }
        Ok(())
    }

    /// # Safety
    /// `child` must be a valid, live Win32 window handle.
    pub unsafe fn hide_view(child: *mut c_void) -> Result<(), String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        if child.is_null() {
            return Ok(());
        }
        unsafe {
            ShowWindow(child, SW_HIDE);
        }
        Ok(())
    }

    /// Destroy the child `HWND` to complete CEF's native browser teardown.
    ///
    /// # Safety
    /// `child` must be a valid live CEF child window handle.
    pub unsafe fn detach_view(child: *mut c_void) {
        use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
        if !child.is_null() {
            unsafe {
                DestroyWindow(child);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux (X11)
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, Window};
    use x11rb::rust_connection::RustConnection;

    use super::ContentRect;

    /// X11 window IDs are carried as pointer values by the platform-agnostic
    /// interface. A single connection is reused for every low-frequency
    /// show/hide/resize operation. Cache the connection *and* any connect
    /// error so we never retry a failed connection from a CEF callback.
    fn connection() -> Result<&'static RustConnection, String> {
        static CONNECTION: OnceLock<Result<RustConnection, String>> = OnceLock::new();
        CONNECTION
            .get_or_init(|| {
                x11rb::connect(None)
                    .map(|(conn, _screen_number)| conn)
                    .map_err(|e| format!("X11 connect failed: {e}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn xid(handle: *mut c_void) -> Window {
        handle as usize as Window
    }

    /// # Safety
    /// `parent` and `child` must encode valid, live X11 window IDs.
    pub unsafe fn show_view(
        parent: *mut c_void,
        child: *mut c_void,
        rect: ContentRect,
    ) -> Result<(), String> {
        if parent.is_null() || child.is_null() {
            return Err("show_view received a null X11 window ID".to_string());
        }
        let conn = connection()?;
        let (x, y, width, height) = rect.to_physical();
        let parent_xid = xid(parent);
        let cef_xid = xid(child);
        conn.reparent_window(cef_xid, parent_xid, x as i16, y as i16)
            .map_err(|e| format!("X11 reparent_window failed: {e}"))?;
        conn.configure_window(
            cef_xid,
            &ConfigureWindowAux::new()
                .x(x as i32)
                .y(y as i32)
                .width(width)
                .height(height),
        )
        .map_err(|e| format!("X11 configure_window failed: {e}"))?;
        conn.map_window(cef_xid)
            .map_err(|e| format!("X11 map_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("X11 flush failed: {e}"))?;
        Ok(())
    }

    /// # Safety
    /// `child` must encode a valid, live X11 window ID.
    pub unsafe fn resize_view(child: *mut c_void, rect: ContentRect) -> Result<(), String> {
        if child.is_null() {
            return Ok(());
        }
        let conn = connection()?;
        let (x, y, width, height) = rect.to_physical();
        conn.configure_window(
            xid(child),
            &ConfigureWindowAux::new()
                .x(x as i32)
                .y(y as i32)
                .width(width)
                .height(height),
        )
        .map_err(|e| format!("X11 configure_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("X11 flush failed: {e}"))?;
        Ok(())
    }

    /// # Safety
    /// `child` must encode a valid, live X11 window ID.
    pub unsafe fn hide_view(child: *mut c_void) -> Result<(), String> {
        if child.is_null() {
            return Ok(());
        }
        let conn = connection()?;
        conn.unmap_window(xid(child))
            .map_err(|e| format!("X11 unmap_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("X11 flush failed: {e}"))?;
        Ok(())
    }

    /// X11 teardown needs the connection handle, which is owned by the main
    /// loop. Hide/resize are sufficient here; closing the browser still works.
    ///
    /// # Safety
    /// `cef_xid` must encode a valid X11 window ID.
    pub unsafe fn detach_view(_cef_xid: *mut c_void) {
        tracing::debug!("detach_view is a no-op on X11");
    }
}

// ---------------------------------------------------------------------------
// Public re-exports — platform-agnostic surface for the rest of the codebase
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub use mac::{detach_view, hide_view, resize_view, show_view};

#[cfg(windows)]
pub use win::{detach_view, hide_view, resize_view, show_view};

#[cfg(target_os = "linux")]
pub use linux::{detach_view, hide_view, resize_view, show_view};

#[cfg(test)]
mod tests {
    use super::{cef_handle_as_ptr, cef_handle_is_null, ptr_as_cef_handle, ContentRect};
    use crate::config::{rail_width, RAIL_WIDTH_COLLAPSED, RAIL_WIDTH_EXPANDED};

    #[test]
    fn null_native_handles_are_detected_across_platforms() {
        assert!(cef_handle_is_null(ptr_as_cef_handle(std::ptr::null_mut())));
    }

    #[test]
    fn native_handle_round_trip_preserves_xid_and_pointer_values() {
        let ptr = 0x1234_usize as *mut std::ffi::c_void;
        assert_eq!(cef_handle_as_ptr(ptr_as_cef_handle(ptr)), ptr);
    }

    #[test]
    fn content_rect_is_logical_and_starts_right_of_the_rail() {
        let rect = ContentRect::from_window(2560, 1600, 2.0, rail_width(true));
        assert_eq!(rect.x, RAIL_WIDTH_EXPANDED as u32);
        assert_eq!(rect.y, 0);
        assert_eq!(rect.width, 1280 - RAIL_WIDTH_EXPANDED as u32);
        assert_eq!(rect.height, 800);
        assert_eq!(rect.scale_factor, 2.0);
        assert_eq!(rect.to_physical(), (360, 0, 2200, 1600));
    }

    #[test]
    fn collapsed_rail_leaves_a_favicon_width_strip() {
        let rect = ContentRect::from_window(2560, 1600, 2.0, rail_width(false));
        assert_eq!(rect.x, RAIL_WIDTH_COLLAPSED as u32);
        assert_eq!(rect.width, 1280 - RAIL_WIDTH_COLLAPSED as u32);
        assert_eq!(rect.height, 800);
    }

    #[test]
    fn content_rect_saturates_when_rail_is_wider_than_window() {
        let rect = ContentRect::from_window(800, 600, 1.0, 5000.0);
        assert_eq!(rect.x, 800);
        assert_eq!(rect.width, 0);
        assert_eq!(rect.height, 600);
    }

    #[test]
    fn content_rect_handles_zero_and_invalid_scale_factors() {
        let zero = ContentRect::from_window(800, 600, 0.0, 0.0);
        assert_eq!(zero.width, 800);
        assert_eq!(zero.scale_factor, 1.0);

        let invalid = ContentRect::from_window(800, 600, f64::NAN, 0.0);
        assert_eq!(invalid.width, 800);
        assert_eq!(invalid.scale_factor, 1.0);

        let negative_inset = ContentRect::from_window(800, 600, 1.0, -10.0);
        assert_eq!(negative_inset.x, 0);
        assert_eq!(negative_inset.width, 800);

        let empty = ContentRect::from_window(0, 0, 1.0, 0.0);
        assert_eq!(empty.width, 0);
        assert_eq!(empty.height, 0);
    }

    #[test]
    fn content_rect_rounds_fractional_logical_sizes() {
        let rect = ContentRect::from_window(1500, 900, 1.5, 10.0);
        assert_eq!(rect.width, 990);
        assert_eq!(rect.height, 600);
    }
}
