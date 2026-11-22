//! Cross-platform native child window embedding for CEF browser views.
//!
//! Each CEF browser renders into its own OS-native view (NSView on macOS,
//! HWND child window on Windows, X11 window on Linux). We embed that view
//! underneath our egui chrome by reparenting / repositioning it every time
//! the outer winit window resizes or the active tab changes.

#![allow(dead_code)]

use crate::config::CHROME_HEIGHT;

/// A rectangle describing where the CEF content should sit inside the outer
/// winit window, in logical points / device-independent pixels.
///
/// macOS `NSView` frames and CEF's `WindowInfo.bounds` both use this unit.
/// Platforms whose APIs take physical pixels (Windows `MoveWindow`, X11) use
/// `scale_factor` to convert at the call site.
#[derive(Debug, Clone, Copy)]
pub struct ContentRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

impl ContentRect {
    /// Compute the logical content rect from the full window physical size.
    pub fn from_window(physical_width: u32, physical_height: u32, scale_factor: f64) -> Self {
        let scale = scale_factor.max(f64::EPSILON);
        let chrome_height = CHROME_HEIGHT as u32;
        let logical_width = (physical_width as f64 / scale).round() as u32;
        let logical_height = (physical_height as f64 / scale).round() as u32;
        let content_height = logical_height.saturating_sub(chrome_height);
        Self {
            x: 0,
            y: chrome_height,
            width: logical_width,
            height: content_height,
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

// ---------------------------------------------------------------------------
// macOS (NSView)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod mac {
    use super::ContentRect;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::NSView;
    use objc2_foundation::NSRect;

    /// Attach the CEF `NSView` as a child of the winit window's content view,
    /// position it in `rect`, and make it visible.
    ///
    /// # Safety
    /// Both `parent_ns_view` and `cef_ns_view` must be valid, live Objective-C
    /// object pointers obtained from raw-window-handle / CEF respectively.
    pub unsafe fn show_view(
        parent_ns_view: *mut AnyObject,
        cef_ns_view: *mut AnyObject,
        rect: ContentRect,
    ) {
        if parent_ns_view.is_null() || cef_ns_view.is_null() {
            tracing::warn!("show_view: null pointer(s), skipping");
            return;
        }
        let parent = unsafe { &*parent_ns_view.cast::<NSView>() };
        let child = unsafe { &*cef_ns_view.cast::<NSView>() };
        // Add first so set_frame() can query the parent's coordinate system.
        unsafe { parent.addSubview(child) };
        set_frame(child, &rect);
        child.setHidden(false);
    }

    /// Reposition the CEF `NSView` without changing its parent.
    ///
    /// # Safety
    /// `cef_ns_view` must be a valid live `NSView *`.
    pub unsafe fn resize_view(cef_ns_view: *mut AnyObject, rect: ContentRect) {
        if cef_ns_view.is_null() {
            return;
        }
        let child = unsafe { &*cef_ns_view.cast::<NSView>() };
        set_frame(child, &rect);
    }

    /// Hide the CEF `NSView` (without destroying it or its browser process).
    ///
    /// # Safety
    /// `cef_ns_view` must be a valid live `NSView *`.
    pub unsafe fn hide_view(cef_ns_view: *mut AnyObject) {
        if cef_ns_view.is_null() {
            return;
        }
        let child = unsafe { &*cef_ns_view.cast::<NSView>() };
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
    use super::ContentRect;

    /// Reparent and position the CEF child `HWND`.
    pub fn show_view(
        parent_hwnd: windows_sys::Win32::Foundation::HWND,
        cef_hwnd: windows_sys::Win32::Foundation::HWND,
        rect: ContentRect,
    ) {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
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
    }

    pub fn resize_view(cef_hwnd: windows_sys::Win32::Foundation::HWND, rect: ContentRect) {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let (x, y, width, height) = rect.to_physical();
        unsafe {
            MoveWindow(cef_hwnd, x as i32, y as i32, width as i32, height as i32, 1);
        }
    }

    pub fn hide_view(cef_hwnd: windows_sys::Win32::Foundation::HWND) {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            ShowWindow(cef_hwnd, SW_HIDE);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux (X11)
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use super::ContentRect;

    pub fn show_view(
        conn: &x11rb::rust_connection::RustConnection,
        parent_xid: u32,
        cef_xid: u32,
        rect: ContentRect,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::*;
        let (x, y, width, height) = rect.to_physical();
        conn.reparent_window(cef_xid, parent_xid, x as i16, y as i16)?;
        conn.configure_window(
            cef_xid,
            &ConfigureWindowAux::new()
                .x(x as i32)
                .y(y as i32)
                .width(width)
                .height(height),
        )?;
        conn.map_window(cef_xid)?;
        conn.flush()?;
        Ok(())
    }

    pub fn resize_view(
        conn: &x11rb::rust_connection::RustConnection,
        cef_xid: u32,
        rect: ContentRect,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::*;
        let (x, y, width, height) = rect.to_physical();
        conn.configure_window(
            cef_xid,
            &ConfigureWindowAux::new()
                .x(x as i32)
                .y(y as i32)
                .width(width)
                .height(height),
        )?;
        conn.flush()?;
        Ok(())
    }

    pub fn hide_view(
        conn: &x11rb::rust_connection::RustConnection,
        cef_xid: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::*;
        conn.unmap_window(cef_xid)?;
        conn.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public re-exports — platform-agnostic surface for the rest of the codebase
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub use mac::{hide_view, resize_view, show_view};

#[cfg(windows)]
pub use win::{hide_view, resize_view, show_view};

#[cfg(target_os = "linux")]
pub use linux::{hide_view, resize_view, show_view};
