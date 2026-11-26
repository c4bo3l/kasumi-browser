//! Native source discovery for Chromium's `getDisplayMedia` desktop capture.
//!
//! CEF can grant desktop capture, but its Alloy media path has no source
//! picker: without an explicit `device_id` on the media request it falls back to
//! `DesktopMediaID(TYPE_SCREEN, -1)`, which Chromium maps to the first display.
//! Kasumi therefore enumerates displays and shareable windows itself and
//! resolves `getDisplayMedia` through Chromium's legacy desktop-capture
//! `getUserMedia` constraints, which do carry the selected `DesktopMediaID`.

use crate::ui::chrome::DisplayMediaSource;

/// Page shim installed by the browser process after each main-frame load.
///
/// Running this from `RenderProcessHandler::on_context_created` re-entered V8
/// while a DevTools context was being created and broke the initial navigation,
/// so the browser process injects it from `LoadHandler::on_load_end` instead.
/// A page may call `getDisplayMedia` only after user interaction, which happens
/// well after load end.
pub const RENDERER_SHIM: &str = r#"
(() => {
  if (window.__kasumiBrowserShim) return;
  window.__kasumiBrowserShim = 1;

  const pendingDisplays = new Map();
  const mediaDevices = navigator.mediaDevices;
  const proto = typeof MediaDevices === 'function' ? MediaDevices.prototype : null;
  const originalGetDisplayMedia =
    (proto && proto.getDisplayMedia) || (mediaDevices && mediaDevices.getDisplayMedia);
  const originalGetUserMedia = mediaDevices && mediaDevices.getUserMedia;
  if (typeof originalGetDisplayMedia === 'function') {
    const callOriginalGetUserMedia = typeof originalGetUserMedia === 'function'
      ? originalGetUserMedia.bind(mediaDevices)
      : null;
    window.__kasumiResolveDisplayMedia = (token, deviceId) => {
      const key = String(token);
      const entry = pendingDisplays.get(key);
      if (!entry) return;
      pendingDisplays.delete(key);
      clearTimeout(entry.timer);
      if (!deviceId) {
        entry.reject(new DOMException('Screen sharing was cancelled', 'NotAllowedError'));
        return;
      }
      if (!callOriginalGetUserMedia) {
        entry.reject(new DOMException('Desktop capture is unavailable', 'NotSupportedError'));
        return;
      }
      const constraints = entry.constraints || {};
      const video = constraints.video && typeof constraints.video === 'object'
        ? constraints.video
        : {};
      // Standard getDisplayMedia rejects `exact`/`min` constraints and Blink
      // deliberately does not forward a content-capture device ID to CEF, so
      // an `ideal` deviceId is ignored. Chromium's legacy desktop-capture
      // getUserMedia path maps chromeMediaSourceId to the media request's
      // device ID, which is exactly what CEF's media-access query parses as a
      // DesktopMediaID. Use it only after the user picked a source.
      const mandatory = {
        chromeMediaSource: 'desktop',
        chromeMediaSourceId: String(deviceId)
      };
      const maxWidth = video.width && typeof video.width === 'object'
        ? (video.width.exact ?? video.width.max ?? video.width.ideal)
        : (typeof video.width === 'number' ? video.width : undefined);
      const maxHeight = video.height && typeof video.height === 'object'
        ? (video.height.exact ?? video.height.max ?? video.height.ideal)
        : (typeof video.height === 'number' ? video.height : undefined);
      const maxFrameRate = video.frameRate && typeof video.frameRate === 'object'
        ? (video.frameRate.exact ?? video.frameRate.max ?? video.frameRate.ideal)
        : (typeof video.frameRate === 'number' ? video.frameRate : undefined);
      if (typeof maxWidth === 'number') mandatory.maxWidth = maxWidth;
      if (typeof maxHeight === 'number') mandatory.maxHeight = maxHeight;
      if (typeof maxFrameRate === 'number') mandatory.maxFrameRate = maxFrameRate;

      const audioRequested = constraints.audio === true ||
        (constraints.audio && typeof constraints.audio === 'object');
      const capture = (withAudio) => callOriginalGetUserMedia({
        audio: withAudio
          ? { mandatory: { chromeMediaSource: 'desktop' } }
          : false,
        video: { mandatory }
      });
      // macOS can deny system-audio capture while still allowing the screen
      // video. Retrying video-only keeps presentation working instead of
      // failing the whole getDisplayMedia promise when a site asks for audio.
      entry.resolve(
        audioRequested
          ? capture(true).catch(() => capture(false))
          : capture(false)
      );
    };
    const requestDisplaySource = (constraints) => {
      const token = 'kdm-' + Date.now() + '-' + Math.random().toString(36).slice(2);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          if (pendingDisplays.delete(token)) {
            reject(new DOMException('Screen sharing timed out', 'NotAllowedError'));
          }
        }, 120000);
        pendingDisplays.set(token, { resolve, reject, constraints, timer });
        console.log('__KASUMI_DISPLAY_MEDIA_REQUEST__:' + token);
      });
    };
    if (proto) proto.getDisplayMedia = requestDisplaySource;
    if (mediaDevices) mediaDevices.getDisplayMedia = requestDisplaySource;
  }

  const normalizeOpus = (sdp) => {
    if (typeof sdp !== 'string' || sdp.indexOf('a=rtpmap:111 opus/') === -1) {
      return sdp;
    }
    const lines = sdp.split('\r\n');
    const opusPayloads = new Set();
    for (const line of lines) {
      const match = /^a=rtpmap:(\d+) opus\/48000\/2$/.exec(line);
      if (match) opusPayloads.add(match[1]);
    }
    if (!opusPayloads.size) return sdp;

    const preferred = new Map();
    for (const line of lines) {
      const match = /^a=fmtp:(\d+) (.+)$/.exec(line);
      if (!match || !opusPayloads.has(match[1])) continue;
      const previous = preferred.get(match[1]);
      if (!previous || (previous.indexOf('stereo=1') === -1 && match[2].indexOf('stereo=1') !== -1)) {
        preferred.set(match[1], match[2]);
      }
    }
    let changed = false;
    const normalized = lines.map((line) => {
      const match = /^a=fmtp:(\d+) (.+)$/.exec(line);
      if (!match || !opusPayloads.has(match[1])) return line;
      const expected = preferred.get(match[1]);
      if (!expected || expected === match[2]) return line;
      changed = true;
      return 'a=fmtp:' + match[1] + ' ' + expected;
    });
    return changed ? normalized.join('\r\n') : sdp;
  };
  const normalizeDescription = (description) => {
    if (!description || typeof description.sdp !== 'string') return description;
    const sdp = normalizeOpus(description.sdp);
    if (sdp === description.sdp) return description;
    return { type: description.type, sdp };
  };
  if (typeof RTCPeerConnection === 'function') {
    for (const method of ['setLocalDescription', 'setRemoteDescription']) {
      const original = RTCPeerConnection.prototype[method];
      if (typeof original !== 'function') continue;
      RTCPeerConnection.prototype[method] = function (description, ...rest) {
        return original.call(this, normalizeDescription(description), ...rest);
      };
    }
  }
})();
"#;

/// Build a Chromium `DesktopMediaID` string for a display.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn screen_device_id(display_id: u32) -> String {
    format!("screen:{display_id}:0")
}

/// Build a Chromium `DesktopMediaID` string for a window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn window_device_id(window_id: u32) -> String {
    format!("window:{window_id}:0")
}

/// Enumerate sources available to `getDisplayMedia` on this platform.
#[cfg(target_os = "macos")]
pub fn available_sources() -> Vec<DisplayMediaSource> {
    let mut sources = Vec::new();
    collect_displays(&mut sources);
    collect_windows(&mut sources);
    sources
}

#[cfg(not(target_os = "macos"))]
pub fn available_sources() -> Vec<DisplayMediaSource> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn collect_displays(sources: &mut Vec<DisplayMediaSource>) {
    unsafe {
        let mut count = 0u32;
        if CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) != 0 || count == 0 {
            return;
        }
        let mut ids = vec![0u32; count as usize];
        if CGGetActiveDisplayList(count, ids.as_mut_ptr(), std::ptr::null_mut()) != 0 {
            return;
        }
        let main = CGMainDisplayID();
        for (index, display_id) in ids.into_iter().enumerate() {
            let bounds = CGDisplayBounds(display_id);
            let width = bounds.size.width.round() as u32;
            let height = bounds.size.height.round() as u32;
            let suffix = if display_id == main { " (main)" } else { "" };
            sources.push(DisplayMediaSource {
                device_id: screen_device_id(display_id),
                label: format!("Display {} — {width} × {height}{suffix}", index + 1),
                is_window: false,
            });
        }
    }
}

#[cfg(target_os = "macos")]
fn collect_windows(sources: &mut Vec<DisplayMediaSource>) {
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    const MAX_SOURCES: usize = 48;
    const NULL_WINDOW_ID: u32 = 0;

    unsafe {
        let list =
            CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, NULL_WINDOW_ID);
        if list.is_null() {
            return;
        }
        let count = CFArrayGetCount(list).max(0) as usize;
        let own_pid = std::process::id() as i64;
        for index in 0..count {
            if sources.len() >= MAX_SOURCES {
                break;
            }
            let dictionary = CFArrayGetValueAtIndex(list, index as isize) as CFDictionaryRef;
            if dictionary.is_null() {
                continue;
            }
            if !cf_bool(dictionary_value(dictionary, kCGWindowIsOnscreen)) {
                continue;
            }
            // Layer 0 is the normal application window layer. Menus, overlays,
            // and screen saver windows are not useful share targets.
            if cf_i64(dictionary_value(dictionary, kCGWindowLayer)).unwrap_or(i64::MAX) != 0 {
                continue;
            }
            if cf_i64(dictionary_value(dictionary, kCGWindowOwnerPID)) == Some(own_pid) {
                continue;
            }
            let Some(window_id) = cf_i64(dictionary_value(dictionary, kCGWindowNumber)) else {
                continue;
            };
            if window_id <= 0 {
                continue;
            }
            let owner =
                cf_string(dictionary_value(dictionary, kCGWindowOwnerName)).unwrap_or_default();
            let title = cf_string(dictionary_value(dictionary, kCGWindowName)).unwrap_or_default();
            let label = match (owner.is_empty(), title.is_empty()) {
                (true, true) => continue,
                (false, true) => owner,
                (true, false) => title,
                (false, false) => format!("{owner} — {title}"),
            };
            sources.push(DisplayMediaSource {
                device_id: window_device_id(window_id as u32),
                label,
                is_window: true,
            });
        }
        CFRelease(list);
    }
}

#[cfg(target_os = "macos")]
type CFTypeRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CFArrayRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CFDictionaryRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CFStringRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CFNumberRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CFBooleanRef = *const std::ffi::c_void;
#[cfg(target_os = "macos")]
type CGDirectDisplayID = u32;

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGWindowNumber: CFStringRef;
    static kCGWindowLayer: CFStringRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowOwnerName: CFStringRef;
    static kCGWindowName: CFStringRef;
    static kCGWindowIsOnscreen: CFStringRef;

    fn CGGetActiveDisplayList(
        max_displays: u32,
        displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
    fn CFDictionaryGetValueIfPresent(
        dictionary: CFDictionaryRef,
        key: CFStringRef,
        value: *mut CFTypeRef,
    ) -> u8;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut std::ffi::c_char,
        buffer_size: usize,
        encoding: u32,
    ) -> u8;
    fn CFNumberGetValue(number: CFNumberRef, the_type: i32, value: *mut std::ffi::c_void) -> u8;
    fn CFBooleanGetValue(boolean: CFBooleanRef) -> u8;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFRelease(value: CFTypeRef);
}

#[cfg(target_os = "macos")]
unsafe fn dictionary_value(dictionary: CFDictionaryRef, key: CFStringRef) -> Option<CFTypeRef> {
    let mut value: CFTypeRef = std::ptr::null();
    if CFDictionaryGetValueIfPresent(dictionary, key, &mut value) != 0 && !value.is_null() {
        Some(value)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
unsafe fn cf_string(value: Option<CFTypeRef>) -> Option<String> {
    const UTF8: u32 = 0x0800_0100;
    let value = value?;
    if CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    let mut buffer = [0u8; 512];
    if CFStringGetCString(value, buffer.as_mut_ptr().cast(), buffer.len(), UTF8) == 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

#[cfg(target_os = "macos")]
unsafe fn cf_i64(value: Option<CFTypeRef>) -> Option<i64> {
    const NUMBER_S_INT64: i32 = 4;
    let value = value?;
    if CFGetTypeID(value) != CFNumberGetTypeID() {
        return None;
    }
    let mut result = 0i64;
    if CFNumberGetValue(value, NUMBER_S_INT64, (&mut result as *mut i64).cast()) == 0 {
        return None;
    }
    Some(result)
}

#[cfg(target_os = "macos")]
unsafe fn cf_bool(value: Option<CFTypeRef>) -> bool {
    let Some(value) = value else {
        return false;
    };
    CFGetTypeID(value) == CFBooleanGetTypeID() && CFBooleanGetValue(value) != 0
}

#[cfg(test)]
mod tests {
    use super::{screen_device_id, window_device_id, RENDERER_SHIM};

    #[test]
    fn desktop_media_ids_match_chromium_serialization() {
        assert_eq!(screen_device_id(37), "screen:37:0");
        assert_eq!(window_device_id(91234), "window:91234:0");
    }

    #[test]
    fn renderer_shim_uses_legacy_desktop_capture_constraints() {
        assert!(RENDERER_SHIM.contains("chromeMediaSource: 'desktop'"));
        assert!(RENDERER_SHIM.contains("chromeMediaSourceId: String(deviceId)"));
        assert!(!RENDERER_SHIM.contains("deviceId: { ideal"));
        assert!(RENDERER_SHIM.contains("__KASUMI_DISPLAY_MEDIA_REQUEST__"));
    }

    #[test]
    fn renderer_shim_retries_display_capture_without_system_audio() {
        assert!(RENDERER_SHIM.contains("capture(true).catch(() => capture(false))"));
    }
}
