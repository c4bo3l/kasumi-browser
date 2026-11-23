//! Favicon download bridge from CEF's `TID_UI` thread to the Rust UI thread.
//!
//! CEF reports favicon URLs in `DisplayHandler::on_favicon_urlchange`, but the
//! image itself must be downloaded asynchronously with `download_image`. The
//! callback below copies the decoded RGBA bitmap into a lock-free
//! `BrowserEvent` so no file or SQLite work happens on a Chromium thread.

use std::sync::Arc;

use cef::*;
use crossbeam_channel::Sender;

use crate::events::BrowserEvent;

wrap_download_image_callback! {
    pub struct FaviconDownloadCallback {
        tab_id: u64,
        event_sender: Sender<BrowserEvent>,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            image_url: Option<&CefString>,
            http_status_code: ::std::os::raw::c_int,
            image: Option<&mut Image>,
        ) {
            // CEF uses 0 for non-network images such as data: URLs.
            let ok_status = http_status_code == 0 || (200..300).contains(&http_status_code);
            if !ok_status {
                return;
            }

            let Some(image) = image else {
                return;
            };

            let mut width = 0;
            let mut height = 0;
            let Some(binary) = image.as_bitmap(
                1.0,
                ColorType::RGBA_8888,
                AlphaType::POSTMULTIPLIED,
                Some(&mut width),
                Some(&mut height),
            ) else {
                return;
            };

            if width <= 0 || height <= 0 {
                return;
            }

            let size = binary.size();
            let data = binary.raw_data();
            if size == 0 || data.is_null() {
                return;
            }

            // Copy before `binary` is dropped; the pointer is only valid for
            // the lifetime of the CEF BinaryValue.
            let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
            let rgba: Arc<[u8]> = Arc::from(bytes);

            let url = image_url.map(|url| url.to_string()).unwrap_or_default();
            if url.is_empty() {
                return;
            }

            let _ = self.event_sender.send(BrowserEvent::FaviconLoaded {
                tab_id: self.tab_id,
                url,
                rgba,
                width: width as u32,
                height: height as u32,
            });
        }
    }
}

pub fn create_favicon_download_callback(
    tab_id: u64,
    event_sender: Sender<BrowserEvent>,
) -> DownloadImageCallback {
    FaviconDownloadCallback::new(tab_id, event_sender).into()
}
