//! Ad and tracker blocking engine powered by Brave's native `adblock-rust`.
//!
//! Evaluates network requests in CEF's `ResourceRequestHandler::on_before_resource_load`
//! on `TID_IO` and cancels matches before scripts/media are downloaded.

#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use cef::*;
use crossbeam_channel::Sender;

use crate::events::BrowserEvent;

/// Built-in fallback starter rules covering prominent ad networks and telemetry
const STARTER_RULES: &[&str] = &[
    "||doubleclick.net^",
    "||googlesyndication.com^",
    "||google-analytics.com^",
    "||adservice.google.com^",
    "||facebook.com/tr^",
    "||adnxs.com^",
    "||criteo.com^",
    "||criteo.net^",
    "||taboola.com^",
    "||outbrain.com^",
    "||amazon-adsystem.com^",
    "||scorecardresearch.com^",
    "||quantserve.com^",
    "||popads.net^",
    "||propellerads.com^",
    "||rubiconproject.com^",
    "||pubmatic.com^",
    "||openx.net^",
    "||adroll.com^",
    "||casalemedia.com^",
    "||moatads.com^",
    "||advertising.com^",
];

/// Create and compile the adblock engine.
pub fn create_adblock_engine() -> Arc<Engine> {
    let mut filter_set = FilterSet::default();
    let parse_opts = ParseOptions::default();

    let easylist_path = Path::new("assets/filters/easylist.txt");
    let easyprivacy_path = Path::new("assets/filters/easyprivacy.txt");

    let mut rules_loaded = 0;

    if easylist_path.exists() {
        if let Ok(content) = fs::read_to_string(easylist_path) {
            filter_set.add_filter_list(content, parse_opts);
            rules_loaded += 1;
            tracing::info!("Loaded EasyList from {:?}", easylist_path);
        }
    }

    if easyprivacy_path.exists() {
        if let Ok(content) = fs::read_to_string(easyprivacy_path) {
            filter_set.add_filter_list(content, parse_opts);
            rules_loaded += 1;
            tracing::info!("Loaded EasyPrivacy from {:?}", easyprivacy_path);
        }
    }

    if rules_loaded == 0 {
        tracing::info!(
            "No filter lists found on disk; initializing built-in starter rules ({} domains)",
            STARTER_RULES.len()
        );
        let combined = STARTER_RULES.join("\n");
        filter_set.add_filter_list(combined, parse_opts);
    }

    let engine = Engine::new_with_filter_set(filter_set);
    Arc::new(engine)
}

fn map_resource_type(rt: ResourceType) -> &'static str {
    match rt {
        ResourceType::SCRIPT => "script",
        ResourceType::IMAGE => "image",
        ResourceType::STYLESHEET => "stylesheet",
        ResourceType::XHR => "xmlhttprequest",
        ResourceType::SUB_FRAME => "sub_frame",
        ResourceType::MEDIA => "media",
        ResourceType::FONT_RESOURCE => "font",
        ResourceType::FAVICON => "image",
        ResourceType::PING => "ping",
        ResourceType::MAIN_FRAME => "main_frame",
        _ => "other",
    }
}

wrap_resource_request_handler! {
    pub struct TabResourceRequestHandler {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
        engine: Arc<Engine>,
    }

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut cef::Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut cef::Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let req = match request {
                Some(r) => r,
                None => return ReturnValue::CONTINUE,
            };

            let url = CefString::from(&req.url()).to_string();
            if url.is_empty() {
                return ReturnValue::CONTINUE;
            }

            let first_party_url = frame
                .map(|f| CefString::from(&f.url()).to_string())
                .unwrap_or_default();

            let req_type = map_resource_type(req.resource_type());

            if let Ok(adblock_req) = Request::new(&url, &first_party_url, req_type, "") {
                let result = self.engine.check_network_request(&adblock_req);
                if result.filter.is_some() && result.exception.is_none() {
                    let _ = self.event_sender.send(BrowserEvent::RequestBlocked {
                        tab_id: self.tab_id,
                    });
                    tracing::debug!(
                        "[Adblock] Tab {}: blocked {} ({})",
                        self.tab_id,
                        url,
                        req_type
                    );
                    return ReturnValue::CANCEL;
                }
            }

            ReturnValue::CONTINUE
        }
    }
}

wrap_request_handler! {
    pub struct TabRequestHandler {
        resource_request_handler: ResourceRequestHandler,
    }

    impl RequestHandler {
        fn resource_request_handler(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut cef::Request>,
            _is_navigation: i32,
            _is_download: i32,
            _request_initiator: Option<&CefStringUtf16>,
            _disable_default_handling: Option<&mut i32>,
        ) -> Option<ResourceRequestHandler> {
            Some(self.resource_request_handler.clone())
        }
    }
}

pub fn create_tab_request_handler(
    tab_id: u64,
    event_sender: Sender<BrowserEvent>,
    engine: Arc<Engine>,
) -> RequestHandler {
    let res_handler = TabResourceRequestHandler::new(tab_id, event_sender, engine);
    TabRequestHandler::new(res_handler.into())
}

#[cfg(test)]
mod tests {
    use super::{create_adblock_engine, map_resource_type};
    use adblock::request::Request;
    use cef::ResourceType;

    #[test]
    fn resource_types_map_to_adblock_names() {
        assert_eq!(map_resource_type(ResourceType::MAIN_FRAME), "main_frame");
        assert_eq!(map_resource_type(ResourceType::SUB_FRAME), "sub_frame");
        assert_eq!(map_resource_type(ResourceType::STYLESHEET), "stylesheet");
        assert_eq!(map_resource_type(ResourceType::SCRIPT), "script");
        assert_eq!(map_resource_type(ResourceType::IMAGE), "image");
        assert_eq!(map_resource_type(ResourceType::FAVICON), "image");
        assert_eq!(map_resource_type(ResourceType::FONT_RESOURCE), "font");
        assert_eq!(map_resource_type(ResourceType::MEDIA), "media");
        assert_eq!(map_resource_type(ResourceType::XHR), "xmlhttprequest");
        assert_eq!(map_resource_type(ResourceType::PING), "ping");
        assert_eq!(map_resource_type(ResourceType::WORKER), "other");
        assert_eq!(map_resource_type(ResourceType::SUB_RESOURCE), "other");
    }

    #[test]
    fn starter_rules_block_known_trackers() {
        let engine = create_adblock_engine();

        for (url, source) in [
            (
                "https://doubleclick.net/pagead/ad.js",
                "https://example.com/",
            ),
            (
                "https://www.google-analytics.com/analytics.js",
                "https://example.com/",
            ),
            (
                "https://adnxs.com/ads?callback=1",
                "https://news.example.org/",
            ),
            ("https://facebook.com/tr?ev=PageView", "https://blog.test/"),
        ] {
            let request = Request::new(url, source, "script", "GET")
                .unwrap_or_else(|err| panic!("valid test request {url}: {err:?}"));
            let result = engine.check_network_request(&request);
            assert!(result.filter.is_some(), "expected block for {url}");
            assert!(result.exception.is_none(), "unexpected exception for {url}");
        }
    }

    #[test]
    fn starter_rules_allow_first_party_resources() {
        let engine = create_adblock_engine();
        let request = Request::new(
            "https://example.com/app.js",
            "https://example.com/",
            "script",
            "GET",
        )
        .expect("valid first-party request");
        let result = engine.check_network_request(&request);
        assert!(result.filter.is_none());
        assert!(result.exception.is_none());
    }

    #[test]
    fn malformed_urls_do_not_panic() {
        let engine = create_adblock_engine();
        assert!(Request::new("not a url", "https://example.com/", "script", "GET").is_err());
        // Defensive check: a valid but unmatched query must not panic.
        let request = Request::new(
            "https://example.com/",
            "https://example.com/",
            "other",
            "GET",
        )
        .expect("valid unmatched request");
        let _ = engine.check_network_request(&request);
    }
}
