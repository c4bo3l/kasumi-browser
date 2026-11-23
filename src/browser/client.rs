use crate::events::{BrowserEvent, BrowserShortcut};
use cef::*;
use crossbeam_channel::Sender;

wrap_life_span_handler! {
    pub struct TabLifeSpanHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                let _ = self.event_sender.send(BrowserEvent::BrowserCreated {
                    tab_id: self.tab_id,
                    browser: browser.clone(),
                });
            }
        }

        fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
            // Returning true tells CEF that the host window needs a custom close
            // notification. We must then complete the browser close ourselves;
            // otherwise the browser object lingers and page media keeps playing.
            let completed = browser
                .as_ref()
                .and_then(|browser| browser.host())
                .map(|host| {
                    // Returning true means the embedder owns native view
                    // teardown. Remove the child view before asking CEF to
                    // finish closing, otherwise the browser stays alive and
                    // page media keeps playing.
                    let view_handle = host.window_handle();
                    if !view_handle.is_null() {
                        unsafe {
                            crate::ui::platform_view::detach_view(view_handle.cast());
                        }
                    }
                    host.try_close_browser()
                })
                .unwrap_or(0);
            tracing::info!(
                "CEF do_close for tab {} (detached view, try_close_browser={})",
                self.tab_id,
                completed
            );
            1
        }
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            // Only report that the CEF browser is gone. Do NOT remove the tab here:
            // hibernation also closes browsers, and the tab metadata must survive.
            tracing::info!("CEF on_before_close for tab {}", self.tab_id);
            let _ = self.event_sender.send(BrowserEvent::BrowserClosed { tab_id: self.tab_id });
        }
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefStringUtf16>,
            _target_frame_name: Option<&CefStringUtf16>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            if let Some(url) = target_url {
                let url_str = url.to_string();
                if url_str.is_empty() {
                    return 1;
                }

                // DevTools and view-source windows are part of the browser
                // itself; routing them through the normal new-tab popup path
                // makes them invisible or useless in this shell.
                let is_internal_view = url_str.starts_with("devtools://")
                    || url_str.starts_with("chrome-devtools://")
                    || url_str.starts_with("view-source:");
                if is_internal_view {
                    return 0;
                }

                let _ = self.event_sender.send(BrowserEvent::NewWindow { url: url_str });
            }
            1 // Cancel ordinary popup windows; open them as Kasumi tabs instead.
        }
    }
}

wrap_load_handler! {
    pub struct TabLoadHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            let _ = self.event_sender.send(BrowserEvent::LoadingStateChange {
                tab_id: self.tab_id,
                is_loading: is_loading != 0,
                can_go_back: can_go_back != 0,
                can_go_forward: can_go_forward != 0,
            });
        }
    }
}

wrap_display_handler! {
    pub struct TabDisplayHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if let (Some(frame), Some(url)) = (frame, url) {
                if frame.is_main() != 0 {
                    let _ = self.event_sender.send(BrowserEvent::AddressChange {
                        tab_id: self.tab_id,
                        url: url.to_string(),
                    });
                }
            }
        }

        fn on_title_change(
            &self,
            _browser: Option<&mut Browser>,
            title: Option<&CefString>,
        ) {
            if let Some(title) = title {
                let _ = self.event_sender.send(BrowserEvent::TitleChange {
                    tab_id: self.tab_id,
                    title: title.to_string(),
                });
            }
        }

        fn on_favicon_urlchange(
            &self,
            _browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            if let Some(icon_urls) = icon_urls {
                let urls: Vec<String> = icon_urls.clone().into_iter().collect();
                tracing::info!("Favicon URLs for tab {}: {:?}", self.tab_id, urls);
                if !urls.is_empty() {
                    let _ = self.event_sender.send(BrowserEvent::FaviconUrlChanged {
                        tab_id: self.tab_id,
                        urls,
                    });
                }
            }
        }

        fn on_loading_progress_change(
            &self,
            _browser: Option<&mut Browser>,
            progress: f64,
        ) {
            let _ = self.event_sender.send(BrowserEvent::LoadingProgressChange {
                tab_id: self.tab_id,
                progress,
            });
        }
    }
}

/// CEF event flags. Values come from `cef_event_flags_t`.
const SHIFT_MODIFIER_FLAG: u32 = 2;
const PRIMARY_MODIFIER_FLAGS: u32 = 4 | 128; // EVENTFLAG_CONTROL_DOWN | EVENTFLAG_COMMAND_DOWN
const ALT_MODIFIER_FLAG: u32 = 8;
const REPEAT_EVENT_FLAG: u32 = 8_192;

/// Virtual key codes, using CEF's platform-independent Windows key codes.
const VK_K: i32 = 0x4B;
const VK_L: i32 = 0x4C;
const VK_T: i32 = 0x54;
const VK_W: i32 = 0x57;
const VK_Z: i32 = 0x5A;
const VK_F12: i32 = 0x7B;

/// Classify a key event forwarded from the CEF web view.
///
/// Native child views do not deliver key events to the winit parent, so the
/// browser has to mirror the shell shortcuts itself. Keeping the mapping in a
/// pure helper makes the cross-platform behavior unit-testable.
pub(crate) fn browser_shortcut(
    key_down: bool,
    key_code: i32,
    modifiers: u32,
) -> Option<BrowserShortcut> {
    let is_repeat = modifiers & REPEAT_EVENT_FLAG != 0;
    if !key_down || is_repeat {
        return None;
    }

    let primary = modifiers & PRIMARY_MODIFIER_FLAGS != 0;
    let shift = modifiers & SHIFT_MODIFIER_FLAG != 0;
    let alt = modifiers & ALT_MODIFIER_FLAG != 0;

    if key_code == VK_F12 {
        return Some(BrowserShortcut::OpenDevTools);
    }

    if !primary || alt {
        return None;
    }

    match key_code {
        VK_T if shift => Some(BrowserShortcut::ReopenClosedTab),
        VK_T => Some(BrowserShortcut::NewTab),
        VK_W if !shift => Some(BrowserShortcut::CloseTab),
        VK_L | VK_K if !shift => Some(BrowserShortcut::OpenCommandPalette),
        VK_Z if !shift => Some(BrowserShortcut::ReopenClosedTab),
        _ => None,
    }
}

wrap_keyboard_handler! {
    pub struct TabKeyboardHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> i32 {
            if let Some(event) = event {
                let is_key_down = event.type_ == KeyEventType::KEYDOWN
                    || event.type_ == KeyEventType::RAWKEYDOWN;
                if let Some(shortcut) =
                    browser_shortcut(is_key_down, event.windows_key_code, event.modifiers)
                {
                    let _ = self.event_sender.send(BrowserEvent::BrowserShortcut {
                        tab_id: self.tab_id,
                        shortcut,
                    });
                    return 1;
                }
            }
            0
        }
    }
}

wrap_context_menu_handler! {
    pub struct TabContextMenuHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl ContextMenuHandler {
        fn on_context_menu_command(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            command_id: ::std::os::raw::c_int,
            _event_flags: EventFlags,
        ) -> i32 {
            let view_source_id = cef::sys::cef_menu_id_t::MENU_ID_VIEW_SOURCE as i32;
            if command_id != view_source_id {
                return 0;
            }

            let Some(params) = params else {
                return 0;
            };
            let page_url_userfree = params.page_url();
            let page_url = CefStringUtf16::from(&page_url_userfree).to_string();
            if page_url.is_empty() {
                return 0;
            }

            let _ = self.event_sender.send(BrowserEvent::ViewSource {
                tab_id: self.tab_id,
                url: page_url,
            });
            1
        }
    }
}

wrap_client! {
    pub struct TabClient {
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
        display_handler: DisplayHandler,
        request_handler: RequestHandler,
        keyboard_handler: KeyboardHandler,
        context_menu_handler: ContextMenuHandler,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request_handler.clone())
        }
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(self.keyboard_handler.clone())
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu_handler.clone())
        }
    }
}

pub fn create_tab_client(
    tab_id: u64,
    event_sender: Sender<BrowserEvent>,
    adblock_engine: std::sync::Arc<adblock::Engine>,
) -> Client {
    let life_span_handler = TabLifeSpanHandler::new(tab_id, event_sender.clone());
    let load_handler = TabLoadHandler::new(tab_id, event_sender.clone());
    let display_handler = TabDisplayHandler::new(tab_id, event_sender.clone());
    let keyboard_handler = TabKeyboardHandler::new(tab_id, event_sender.clone());
    let context_menu_handler = TabContextMenuHandler::new(tab_id, event_sender.clone());
    let request_handler =
        super::adblocker::create_tab_request_handler(tab_id, event_sender, adblock_engine);

    TabClient::new(
        life_span_handler.into(),
        load_handler.into(),
        display_handler.into(),
        request_handler.into(),
        keyboard_handler.into(),
        context_menu_handler.into(),
    )
}

#[cfg(test)]
mod tests {
    use super::browser_shortcut;
    use crate::events::BrowserShortcut;

    #[test]
    fn web_view_forwards_all_shell_shortcuts() {
        assert_eq!(
            browser_shortcut(true, 0x54, 128),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x54, 4),
            Some(BrowserShortcut::NewTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x54, 128 | 2),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x5A, 128),
            Some(BrowserShortcut::ReopenClosedTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x57, 128),
            Some(BrowserShortcut::CloseTab)
        );
        assert_eq!(
            browser_shortcut(true, 0x4B, 128),
            Some(BrowserShortcut::OpenCommandPalette)
        );
        assert_eq!(
            browser_shortcut(true, 0x4C, 4),
            Some(BrowserShortcut::OpenCommandPalette)
        );
        assert_eq!(
            browser_shortcut(true, 0x7B, 0),
            Some(BrowserShortcut::OpenDevTools)
        );
    }

    #[test]
    fn web_view_ignores_repeats_and_unrelated_modifiers() {
        assert_eq!(browser_shortcut(false, 0x54, 128), None);
        assert_eq!(browser_shortcut(true, 0x54, 128 | 8_192), None);
        assert_eq!(browser_shortcut(true, 0x54, 128 | 8), None);
        assert_eq!(browser_shortcut(true, 0x54, 0), None);
        assert_eq!(browser_shortcut(true, 0x54, 2), None);
        assert_eq!(browser_shortcut(true, 0x57, 128 | 2), None);
        assert_eq!(browser_shortcut(true, 0x58, 128), None);
    }
}
