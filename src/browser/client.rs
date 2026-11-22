use crate::events::BrowserEvent;
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

        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            tracing::info!("CEF do_close for tab {}", self.tab_id);
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
                if !url_str.is_empty() {
                    let _ = self.event_sender.send(BrowserEvent::NewWindow { url: url_str });
                }
            }
            1 // Cancel default popup window
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

wrap_client! {
    pub struct TabClient {
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
        display_handler: DisplayHandler,
        request_handler: RequestHandler,
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
    let request_handler =
        super::adblocker::create_tab_request_handler(tab_id, event_sender, adblock_engine);

    TabClient::new(
        life_span_handler.into(),
        load_handler.into(),
        display_handler.into(),
        request_handler.into(),
    )
}
