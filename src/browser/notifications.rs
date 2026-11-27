//! Native bridge for the Web Notification API.
//!
//! CEF's Alloy embedded browsers have no notification presenter, so pages that
//! call `new Notification(...)` would otherwise fall back to Chromium internals
//! that never surface a native alert. Kasumi installs a small renderer shim that
//! forwards permission requests and notification payloads over internal console
//! messages. The browser process owns the native permission prompt, the
//! transient notification window, and the click-through back into the page.

use serde::Deserialize;

/// Console marker used by the shim when a page calls
/// `Notification.requestPermission()`.
pub const NOTIFICATION_PERMISSION_CONSOLE_PREFIX: &str = "__KASUMI_NOTIFICATION_PERMISSION__:";
/// Console marker used by the shim when a page constructs a notification.
pub const NOTIFICATION_SHOW_CONSOLE_PREFIX: &str = "__KASUMI_NOTIFICATION_SHOW__:";
/// Console marker used by the shim when a page calls `notification.close()`.
pub const NOTIFICATION_CLOSE_CONSOLE_PREFIX: &str = "__KASUMI_NOTIFICATION_CLOSE__:";

/// Maximum characters shown from a site-provided title. Long titles are
/// truncated instead of growing the native window without bound.
const MAX_TITLE_CHARS: usize = 180;
/// Maximum characters shown from a site-provided body.
const MAX_BODY_CHARS: usize = 640;

/// Permission request payload sent by the renderer shim.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationPermissionPayload {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub origin: String,
}

/// Notification payload sent by the renderer shim.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub require_interaction: bool,
    #[serde(default)]
    pub origin: String,
}

/// Close payload sent by `notification.close()`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationClosePayload {
    #[serde(default)]
    pub id: String,
}

/// Truncate text on a character boundary so multibyte site content cannot make
/// the native toast overflow or allocate an unbounded string.
fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn parse_payload<T>(message: &str, prefix: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    // Console control messages are small. Reject oversized site payloads
    // before serde allocates strings for them.
    const MAX_CONSOLE_PAYLOAD_BYTES: usize = 16 * 1024;
    let payload = message.strip_prefix(prefix)?.trim();
    if payload.is_empty() || payload.len() > MAX_CONSOLE_PAYLOAD_BYTES {
        return None;
    }
    serde_json::from_str(payload).ok()
}

pub fn parse_permission_message(message: &str) -> Option<NotificationPermissionPayload> {
    let payload: NotificationPermissionPayload =
        parse_payload(message, NOTIFICATION_PERMISSION_CONSOLE_PREFIX)?;
    if payload.token.trim().is_empty() {
        return None;
    }
    Some(NotificationPermissionPayload {
        token: payload.token.trim().to_string(),
        origin: payload.origin.trim().to_string(),
    })
}

pub fn parse_show_message(message: &str) -> Option<NotificationPayload> {
    let payload: NotificationPayload = parse_payload(message, NOTIFICATION_SHOW_CONSOLE_PREFIX)?;
    let id = payload.id.trim();
    if id.is_empty() {
        return None;
    }
    Some(NotificationPayload {
        id: id.to_string(),
        title: truncate_chars(payload.title.trim(), MAX_TITLE_CHARS),
        body: truncate_chars(payload.body.trim(), MAX_BODY_CHARS),
        tag: truncate_chars(payload.tag.trim(), MAX_TITLE_CHARS),
        require_interaction: payload.require_interaction,
        origin: payload.origin.trim().to_string(),
    })
}

pub fn parse_close_message(message: &str) -> Option<NotificationClosePayload> {
    let payload: NotificationClosePayload =
        parse_payload(message, NOTIFICATION_CLOSE_CONSOLE_PREFIX)?;
    if payload.id.trim().is_empty() {
        return None;
    }
    Some(NotificationClosePayload {
        id: payload.id.trim().to_string(),
    })
}

/// Page shim installed by the browser process after each main-frame load.
///
/// It replaces `window.Notification` with a native-backed implementation while
/// preserving the standard constructor, `permission`, `requestPermission`,
/// event handlers, and `close()` semantics pages rely on.
pub const RENDERER_SHIM: &str = r#"
(() => {
  if (window.__kasumiNotificationShim) return;
  window.__kasumiNotificationShim = 1;

  const PERMISSION_PREFIX = '__KASUMI_NOTIFICATION_PERMISSION__:';
  const SHOW_PREFIX = '__KASUMI_NOTIFICATION_SHOW__:';
  const CLOSE_PREFIX = '__KASUMI_NOTIFICATION_CLOSE__:';
  const NativeNotification = window.Notification;
  if (typeof NativeNotification !== 'function') return;

  let permission = 'default';
  let pendingPermission = null;
  let pendingToken = null;
  let pendingCallbacks = [];
  let pendingResolve = null;
  const live = new Map();

  const post = (prefix, payload) => {
    try { console.log(prefix + JSON.stringify(payload)); } catch (_) {}
  };
  const makeId = () => 'kn-' + Date.now() + '-' + Math.random().toString(36).slice(2);
  const dispatch = (notification, type) => {
    const event = { type, notification };
    const handler = notification['on' + type];
    if (typeof handler === 'function') {
      try { handler.call(notification, event); } catch (_) {}
    }
    const listeners = notification.__listeners && notification.__listeners[type];
    if (listeners) {
      for (const listener of listeners.slice()) {
        try { listener.call(notification, event); } catch (_) {}
      }
    }
    if (type === 'close') live.delete(notification.__kasumiId);
  };
  const settlePermission = (decision) => {
    permission = (decision === true || decision === 'granted')
      ? 'granted'
      : (decision === 'denied' ? 'denied' : 'default');
    const callbacks = pendingCallbacks;
    const resolve = pendingResolve;
    pendingPermission = null;
    pendingToken = null;
    pendingCallbacks = [];
    pendingResolve = null;
    if (typeof resolve === 'function') resolve(permission);
    for (const callback of callbacks) {
      try { callback(permission); } catch (_) {}
    }
  };

  function KasumiNotification(title, options) {
    if (!new.target) return new KasumiNotification(title, options);
    if (arguments.length === 0) {
      throw new TypeError("Failed to construct 'Notification': 1 argument required, but only 0 present.");
    }
    if (permission === 'denied') {
      throw new TypeError("Failed to construct 'Notification': The user denied permission to use the Notification API.");
    }
    options = options || {};
    this.title = String(title);
    this.body = options.body === undefined ? '' : String(options.body);
    this.icon = options.icon === undefined ? '' : String(options.icon);
    this.image = options.image === undefined ? '' : String(options.image);
    this.badge = options.badge === undefined ? '' : String(options.badge);
    this.tag = options.tag === undefined ? '' : String(options.tag);
    this.data = options.data;
    this.silent = !!options.silent;
    this.requireInteraction = !!options.requireInteraction;
    this.dir = options.dir === undefined ? 'auto' : String(options.dir);
    this.lang = options.lang === undefined ? '' : String(options.lang);
    this.timestamp = typeof options.timestamp === 'number' ? options.timestamp : Date.now();
    this.onclick = null;
    this.onshow = null;
    this.onclose = null;
    this.onerror = null;
    this.__kasumiId = makeId();
    this.__kasumiClosed = false;
    this.__listeners = Object.create(null);
    live.set(this.__kasumiId, this);
    if (permission === 'granted') {
      post(SHOW_PREFIX, {
        id: this.__kasumiId,
        title: this.title,
        body: this.body,
        tag: this.tag,
        requireInteraction: this.requireInteraction,
        origin: (location && location.origin) || ''
      });
      setTimeout(() => dispatch(this, 'show'), 0);
    } else {
      setTimeout(() => {
        dispatch(this, 'error');
        live.delete(this.__kasumiId);
      }, 0);
    }
  }

  KasumiNotification.prototype.close = function () {
    if (!this.__kasumiId || this.__kasumiClosed) return;
    this.__kasumiClosed = true;
    post(CLOSE_PREFIX, { id: this.__kasumiId });
    dispatch(this, 'close');
  };
  KasumiNotification.prototype.addEventListener = function (type, listener) {
    if (typeof listener !== 'function') return;
    const key = String(type);
    (this.__listeners[key] || (this.__listeners[key] = [])).push(listener);
  };
  KasumiNotification.prototype.removeEventListener = function (type, listener) {
    const listeners = this.__listeners && this.__listeners[String(type)];
    if (!listeners) return;
    const index = listeners.indexOf(listener);
    if (index >= 0) listeners.splice(index, 1);
  };
  Object.defineProperty(KasumiNotification, 'permission', {
    configurable: true,
    get: () => permission
  });
  KasumiNotification.requestPermission = function (callback) {
    if (typeof callback === 'function') {
      if (permission !== 'default') {
        setTimeout(() => callback(permission), 0);
        return Promise.resolve(permission);
      }
      pendingCallbacks.push(callback);
    }
    if (permission !== 'default') return Promise.resolve(permission);
    if (pendingPermission) return pendingPermission;
    pendingToken = makeId();
    pendingPermission = new Promise((resolve) => {
      pendingResolve = resolve;
      post(PERMISSION_PREFIX, {
        token: pendingToken,
        origin: (location && location.origin) || ''
      });
    });
    return pendingPermission;
  };
  KasumiNotification.maxActions = 0;

  try {
    Object.defineProperty(window, 'Notification', {
      configurable: true,
      writable: true,
      value: KasumiNotification
    });
  } catch (_) {
    window.Notification = KasumiNotification;
  }
  window.__kasumiResolveNotificationPermission = (token, decision) => {
    if (!pendingToken || String(token) !== pendingToken) return;
    settlePermission(decision === true || decision === 'granted');
  };
  window.__kasumiNotificationEvent = (id, type) => {
    const notification = live.get(String(id));
    if (!notification) return;
    dispatch(notification, String(type));
    if (type === 'click') notification.close();
  };
})();
"#;

#[cfg(test)]
mod tests {
    use super::{parse_close_message, parse_permission_message, parse_show_message, RENDERER_SHIM};

    #[test]
    fn permission_messages_require_a_token() {
        assert_eq!(
            parse_permission_message(
                "__KASUMI_NOTIFICATION_PERMISSION__:{\"token\":\"abc\",\"origin\":\"https://example.com\"}"
            )
            .expect("valid permission payload")
            .token,
            "abc"
        );
        assert!(parse_permission_message("__KASUMI_NOTIFICATION_PERMISSION__:{}").is_none());
        assert!(parse_permission_message("ordinary console output").is_none());
    }

    #[test]
    fn notification_messages_require_an_id_and_are_truncated() {
        let payload = parse_show_message(
            "__KASUMI_NOTIFICATION_SHOW__:{\"id\":\"kn-1\",\"title\":\"Hello\",\"body\":\"World\",\"tag\":\"chat\",\"requireInteraction\":true,\"origin\":\"https://example.com\"}",
        )
        .expect("valid notification payload");
        assert_eq!(payload.id, "kn-1");
        assert_eq!(payload.title, "Hello");
        assert_eq!(payload.body, "World");
        assert_eq!(payload.tag, "chat");
        assert!(payload.require_interaction);
        assert!(parse_show_message("__KASUMI_NOTIFICATION_SHOW__:{}").is_none());

        let long = "x".repeat(900);
        let truncated = parse_show_message(&format!(
            "__KASUMI_NOTIFICATION_SHOW__:{{\"id\":\"kn-2\",\"title\":\"t\",\"body\":\"{long}\"}}"
        ))
        .expect("long body payload");
        assert!(truncated.body.chars().count() <= 640);

        let oversized = "x".repeat(20 * 1024);
        assert!(parse_show_message(&format!(
            "__KASUMI_NOTIFICATION_SHOW__:{{\"id\":\"kn-3\",\"title\":\"{oversized}\"}}"
        ))
        .is_none());
    }

    #[test]
    fn close_messages_carry_the_notification_id() {
        let payload = parse_close_message("__KASUMI_NOTIFICATION_CLOSE__:{\"id\":\"kn-3\"}")
            .expect("valid close payload");
        assert_eq!(payload.id, "kn-3");
        assert!(parse_close_message("__KASUMI_NOTIFICATION_CLOSE__:{}").is_none());
    }

    #[test]
    fn renderer_shim_replaces_notification_and_keeps_the_api_surface() {
        assert!(RENDERER_SHIM.contains("Object.defineProperty(window, 'Notification'"));
        assert!(RENDERER_SHIM.contains("KasumiNotification.requestPermission"));
        assert!(RENDERER_SHIM.contains("__KASUMI_NOTIFICATION_SHOW__"));
        assert!(RENDERER_SHIM.contains("__kasumiNotificationEvent"));
        assert!(RENDERER_SHIM.contains("prototype.close"));
    }
}
