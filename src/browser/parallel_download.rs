//! Native segmented HTTP downloader.
//!
//! CEF resolves the configured download destination, then Kasumi cancels the
//! CEF copy and takes over with `cef::Urlrequest` HTTP Range segments. All data
//! events are forwarded to the main UI thread; disk I/O happens on a dedicated
//! worker thread, so no CEF callback ever blocks on SQLite or the filesystem.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cef::*;
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::config::PARALLEL_DOWNLOAD_MIN_BYTES;
use crate::events::{BrowserEvent, ParallelDownloadEvent};

/// Inclusive byte range assigned to one native HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRange {
    pub start: i64,
    pub end: i64,
}

/// Split a known content length into balanced ranges. Small files use a single
/// request; otherwise the fan-out is capped by `PARALLEL_DOWNLOAD_SEGMENTS`.
pub fn plan_segments(total_bytes: i64, max_segments: u8) -> Vec<SegmentRange> {
    if total_bytes <= 0 || max_segments == 0 {
        return Vec::new();
    }
    let count = (max_segments as i64).min(total_bytes).max(1) as usize;
    let chunk = total_bytes / count as i64;
    let remainder = total_bytes % count as i64;

    let mut ranges = Vec::with_capacity(count);
    let mut cursor = 0_i64;
    for index in 0..count {
        let length = chunk + if index < remainder as usize { 1 } else { 0 };
        if length <= 0 {
            break;
        }
        ranges.push(SegmentRange {
            start: cursor,
            end: cursor + length - 1,
        });
        cursor += length;
    }
    ranges
}

/// Parse `Content-Range: bytes 0-0/1234` into the total resource length.
pub fn parse_content_range(value: &str) -> Option<i64> {
    let value = value.trim();
    let total = value.split('/').next_back()?;
    if total == "*" {
        return None;
    }
    let total = total.parse::<i64>().ok()?;
    (total > 0).then_some(total)
}

/// Parse a plain `Content-Length` header.
pub fn parse_content_length(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok().filter(|len| *len > 0)
}

/// Only byte-capable servers get multiple segments. HEAD is not universally
/// supported, so a missing header still allows a single native GET.
pub fn supports_byte_ranges(value: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case("bytes"))
}

/// Decide whether a HEAD probe should fan out or use one request.
pub fn should_parallelize(status: i32, supports_ranges: bool, total_bytes: i64) -> bool {
    (200..300).contains(&status) && supports_ranges && total_bytes >= PARALLEL_DOWNLOAD_MIN_BYTES
}

/// A ranged GET must return `206 Partial Content`; a plain GET accepts any
/// success code. This prevents a server ignoring `Range` from being treated as
/// a valid segment.
pub fn segment_status_is_success(status: i32, ranged: bool) -> bool {
    if ranged {
        status == 206
    } else {
        (200..300).contains(&status)
    }
}

/// Strip any directory component from a server-provided name.
pub fn safe_file_name(name: &str) -> String {
    let trimmed = name.trim();
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    if file_name.is_empty() {
        "download".to_string()
    } else {
        file_name.to_string()
    }
}

pub fn part_path_for(final_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.kasumi.part", final_path.display()))
}

/// HTTP(S) downloads can be segmented. CEF keeps ownership of exotic schemes
/// such as `blob:`, `data:`, and `file:`.
pub fn is_segmentable_url(url: &str) -> bool {
    matches!(
        url::Url::parse(url),
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https")
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Probe,
    Segment { index: u8, ranged: bool },
}

// URLRequest client for the probe and each segment. It only copies bytes into
// owned events; the `Urlrequest` handle itself lives on the UI thread.
wrap_urlrequest_client! {
    pub struct SegmentRequestClient {
        event_sender: Sender<BrowserEvent>,
        download_id: u32,
        generation: u64,
        kind: RequestKind,
        segment_start: i64,
        received: std::cell::Cell<i64>,
    }

    impl UrlrequestClient {
        fn on_request_complete(&self, request: Option<&mut Urlrequest>) {
            let Some(request) = request else {
                return;
            };
            let succeeded = request.request_status() == UrlrequestStatus::SUCCESS;
            let response = request.response();
            let status = response.as_ref().map(|value| value.status()).unwrap_or(0);

            match self.kind {
                RequestKind::Probe => {
                    let (accept_ranges, content_range, content_length, final_url) = response
                        .as_ref()
                        .map(|value| {
                            (
                                CefString::from(&value.header_by_name(Some(&CefString::from(
                                    "Accept-Ranges",
                                ))))
                                .to_string(),
                                CefString::from(&value.header_by_name(Some(&CefString::from(
                                    "Content-Range",
                                ))))
                                .to_string(),
                                CefString::from(&value.header_by_name(Some(&CefString::from(
                                    "Content-Length",
                                ))))
                                .to_string(),
                                CefString::from(&value.url()).to_string(),
                            )
                        })
                        .unwrap_or_default();
                    let total_bytes = parse_content_range(&content_range)
                        .or_else(|| parse_content_length(&content_length))
                        .unwrap_or(0);
                    let _ = self.event_sender.send(BrowserEvent::ParallelDownload(
                        ParallelDownloadEvent::ProbeResult {
                            download_id: self.download_id,
                            generation: self.generation,
                            status: if succeeded { status } else { -1 },
                            supports_ranges: supports_byte_ranges(&accept_ranges),
                            total_bytes,
                            final_url,
                        },
                    ));
                }
                RequestKind::Segment { index, ranged } => {
                    // A ranged request must actually return a partial body.
                    // If the server ignored `Range`, restarting as one plain
                    // GET avoids writing overlapping copies of the full file.
                    let success = succeeded && segment_status_is_success(status, ranged);
                    let _ = self.event_sender.send(BrowserEvent::ParallelDownload(
                        ParallelDownloadEvent::SegmentComplete {
                            download_id: self.download_id,
                            generation: self.generation,
                            segment_index: index,
                            received: self.received.get(),
                            success,
                        },
                    ));
                }
            }
        }

        fn on_download_data(
            &self,
            _request: Option<&mut Urlrequest>,
            data: *const u8,
            data_length: usize,
        ) {
            let RequestKind::Segment { index, .. } = self.kind else {
                return;
            };
            if data.is_null() || data_length == 0 {
                return;
            }
            let bytes: Arc<[u8]> = unsafe { std::slice::from_raw_parts(data, data_length) }
                .to_vec()
                .into();
            let offset = self.segment_start + self.received.get();
            self.received.set(self.received.get() + data_length as i64);
            let _ = self.event_sender.send(BrowserEvent::ParallelDownload(
                ParallelDownloadEvent::SegmentData {
                    download_id: self.download_id,
                    generation: self.generation,
                    segment_index: index,
                    offset,
                    bytes,
                },
            ));
        }
    }
}

fn new_request(
    url: &str,
    method: &str,
    range: Option<SegmentRange>,
    client: &mut UrlrequestClient,
    request_context: Option<&mut RequestContext>,
) -> Option<Urlrequest> {
    let mut request = request_create()?;
    request.set_url(Some(&CefString::from(url)));
    request.set_method(Some(&CefString::from(method)));
    if let Some(range) = range {
        let header = format!("bytes={}-{}", range.start, range.end);
        request.set_header_by_name(
            Some(&CefString::from("Range")),
            Some(&CefString::from(header.as_str())),
            1,
        );
    }
    urlrequest_create(Some(&mut request), Some(client), request_context)
}

/// Start a lightweight HEAD probe to learn length and Range support.
pub fn start_probe(
    event_sender: &Sender<BrowserEvent>,
    download_id: u32,
    generation: u64,
    url: &str,
    mut request_context: Option<RequestContext>,
) -> Option<Urlrequest> {
    let mut client = SegmentRequestClient::new(
        event_sender.clone(),
        download_id,
        generation,
        RequestKind::Probe,
        0,
        std::cell::Cell::new(0),
    );
    new_request(url, "HEAD", None, &mut client, request_context.as_mut())
}

/// Start a single segment. Passing a range for every segment (including a
/// whole-file range when ranges are unsupported) keeps the data path uniform.
pub fn start_segment(
    event_sender: &Sender<BrowserEvent>,
    download_id: u32,
    generation: u64,
    url: &str,
    segment: SegmentRange,
    index: u8,
    mut request_context: Option<RequestContext>,
    ranged: bool,
) -> Option<Urlrequest> {
    let mut client = SegmentRequestClient::new(
        event_sender.clone(),
        download_id,
        generation,
        RequestKind::Segment { index, ranged },
        segment.start,
        std::cell::Cell::new(0),
    );
    new_request(
        url,
        "GET",
        ranged.then_some(segment),
        &mut client,
        request_context.as_mut(),
    )
    .or_else(|| {
        tracing::warn!(
            "Could not create URL request for segment {index} of download {download_id}"
        );
        None
    })
}

#[derive(Debug)]
enum WriterCommand {
    Start {
        download_id: u32,
        part_path: PathBuf,
        final_path: PathBuf,
        total_bytes: i64,
        truncate: bool,
    },
    Write {
        download_id: u32,
        offset: i64,
        bytes: Arc<[u8]>,
    },
    Flush {
        download_id: u32,
    },
    Finish {
        download_id: u32,
    },
    Cancel {
        download_id: u32,
    },
    Shutdown,
}

struct OpenDownload {
    file: Option<File>,
    part_path: PathBuf,
    final_path: PathBuf,
}

/// Background filesystem actor. Downloads never write from a CEF or winit
/// callback thread; commands are queued in arrival order over crossbeam.
pub struct DownloadWriter {
    sender: Sender<WriterCommand>,
    done: Receiver<()>,
}

impl DownloadWriter {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded::<WriterCommand>();
        let (done_sender, done) = unbounded::<()>();
        if let Err(error) = thread::Builder::new()
            .name("kasumi-download-writer".to_string())
            .spawn(move || run_writer(receiver, done_sender))
        {
            tracing::warn!("Could not start download writer thread: {error}");
        }
        Self { sender, done }
    }

    pub fn start(
        &self,
        download_id: u32,
        part_path: PathBuf,
        final_path: PathBuf,
        total_bytes: i64,
        truncate: bool,
    ) {
        let _ = self.sender.send(WriterCommand::Start {
            download_id,
            part_path,
            final_path,
            total_bytes,
            truncate,
        });
    }

    pub fn write(&self, download_id: u32, offset: i64, bytes: Arc<[u8]>) {
        let _ = self.sender.send(WriterCommand::Write {
            download_id,
            offset,
            bytes,
        });
    }

    pub fn flush(&self, download_id: u32) {
        let _ = self.sender.send(WriterCommand::Flush { download_id });
    }

    pub fn finish(&self, download_id: u32) {
        let _ = self.sender.send(WriterCommand::Finish { download_id });
    }

    pub fn cancel(&self, download_id: u32) {
        let _ = self.sender.send(WriterCommand::Cancel { download_id });
    }

    /// Called once during shell shutdown. Waits briefly for queued flushes so
    /// an interrupted transfer still leaves a consistent `.kasumi.part` file.
    pub fn shutdown(&self) {
        if self.sender.send(WriterCommand::Shutdown).is_ok() {
            if let Err(error) = self.done.recv_timeout(Duration::from_secs(5)) {
                tracing::warn!("Download writer did not stop cleanly: {error}");
            }
        }
    }
}

impl Default for DownloadWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn run_writer(receiver: Receiver<WriterCommand>, done: Sender<()>) {
    let mut downloads: HashMap<u32, OpenDownload> = HashMap::new();

    while let Ok(command) = receiver.recv() {
        match command {
            WriterCommand::Start {
                download_id,
                part_path,
                final_path,
                total_bytes,
                truncate,
            } => {
                if let Some(parent) = part_path.parent() {
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        tracing::warn!("Could not create download directory {parent:?}: {error}");
                    }
                }
                if truncate {
                    let _ = std::fs::remove_file(&part_path);
                }
                let mut options = OpenOptions::new();
                options.create(true).write(true);
                if truncate {
                    options.truncate(true);
                }
                match options.open(&part_path) {
                    Ok(file) => {
                        if truncate && total_bytes > 0 && file.set_len(total_bytes as u64).is_err()
                        {
                            tracing::warn!("Could not preallocate native download file");
                        }
                        downloads.insert(
                            download_id,
                            OpenDownload {
                                file: Some(file),
                                part_path,
                                final_path,
                            },
                        );
                    }
                    Err(error) => tracing::warn!("Could not open {part_path:?}: {error}"),
                }
            }
            WriterCommand::Write {
                download_id,
                offset,
                bytes,
            } => {
                if let Some(download) = downloads.get_mut(&download_id) {
                    if let Some(file) = download.file.as_mut() {
                        if let Err(error) = file.seek(SeekFrom::Start(offset.max(0) as u64)) {
                            tracing::warn!("Could not seek native download {download_id}: {error}");
                            continue;
                        }
                        if let Err(error) = file.write_all(&bytes) {
                            tracing::warn!(
                                "Could not write native download {download_id}: {error}"
                            );
                        }
                    }
                }
            }
            WriterCommand::Flush { download_id } => {
                if let Some(file) = downloads
                    .get_mut(&download_id)
                    .and_then(|download| download.file.as_mut())
                {
                    if let Err(error) = file.flush() {
                        tracing::warn!("Could not flush native download {download_id}: {error}");
                    }
                }
            }
            WriterCommand::Finish { download_id } => {
                if let Some(mut download) = downloads.remove(&download_id) {
                    drop(download.file.take());
                    if std::fs::rename(&download.part_path, &download.final_path).is_err() {
                        // Windows cannot rename over an existing destination.
                        if download.final_path.exists() {
                            let _ = std::fs::remove_file(&download.final_path);
                        }
                        if let Err(error) =
                            std::fs::rename(&download.part_path, &download.final_path)
                        {
                            tracing::warn!("Could not finalize {:?}: {error}", download.final_path);
                        }
                    }
                }
            }
            WriterCommand::Cancel { download_id } => {
                if let Some(mut download) = downloads.remove(&download_id) {
                    drop(download.file.take());
                    let _ = std::fs::remove_file(&download.part_path);
                }
            }
            WriterCommand::Shutdown => {
                for download in downloads.values_mut() {
                    if let Some(file) = download.file.as_mut() {
                        let _ = file.flush();
                    }
                }
                break;
            }
        }
    }
    let _ = done.send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PARALLEL_DOWNLOAD_SEGMENTS;

    #[test]
    fn segment_plan_covers_every_byte_exactly_once() {
        let ranges = plan_segments(1000, 4);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, 999);
        let covered: i64 = ranges.iter().map(|range| range.end - range.start + 1).sum();
        assert_eq!(covered, 1000);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start);
        }
    }

    #[test]
    fn degenerate_segment_requests_are_ignored() {
        // `should_parallelize` keeps sub-threshold files out of this planner;
        // the planner itself still covers any positive length exactly once.
        let ranges = plan_segments(3, PARALLEL_DOWNLOAD_SEGMENTS);
        let covered: i64 = ranges.iter().map(|range| range.end - range.start + 1).sum();
        assert_eq!(covered, 3);
        assert!(plan_segments(0, 8).is_empty());
        assert!(plan_segments(100, 0).is_empty());
    }

    #[test]
    fn range_and_length_headers_are_parsed() {
        assert_eq!(parse_content_range("bytes 0-0/4096"), Some(4096));
        assert_eq!(parse_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_content_length(" 2048 "), Some(2048));
        assert_eq!(parse_content_length("nope"), None);
    }

    #[test]
    fn only_byte_ranges_are_parallelized() {
        assert!(supports_byte_ranges("bytes"));
        assert!(supports_byte_ranges("none, bytes"));
        assert!(!supports_byte_ranges("none"));
        assert!(should_parallelize(206, true, PARALLEL_DOWNLOAD_MIN_BYTES));
        assert!(!should_parallelize(206, false, PARALLEL_DOWNLOAD_MIN_BYTES));
        assert!(!should_parallelize(404, true, PARALLEL_DOWNLOAD_MIN_BYTES));
        assert!(!should_parallelize(
            206,
            true,
            PARALLEL_DOWNLOAD_MIN_BYTES - 1
        ));
        assert!(segment_status_is_success(206, true));
        assert!(!segment_status_is_success(200, true));
        assert!(segment_status_is_success(200, false));
        assert!(!segment_status_is_success(404, false));
    }

    #[test]
    fn server_file_names_cannot_escape_the_download_directory() {
        assert_eq!(safe_file_name("archive.zip"), "archive.zip");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("  "), "download");
    }

    #[test]
    fn only_http_urls_are_segmentable() {
        assert!(is_segmentable_url("https://example.com/file.zip"));
        assert!(is_segmentable_url("http://example.com/file.zip"));
        assert!(!is_segmentable_url("blob:https://example.com/uuid"));
        assert!(!is_segmentable_url("file:///tmp/file.zip"));
        assert!(!is_segmentable_url("not a url"));
    }

    #[test]
    fn part_path_keeps_the_original_file_name() {
        let part = part_path_for(Path::new("/Downloads/archive.zip"));
        assert_eq!(part, PathBuf::from("/Downloads/archive.zip.kasumi.part"));
    }

    #[test]
    fn part_writer_finalizes_at_exact_offsets() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let directory = std::env::temp_dir().join(format!("kasumi-download-writer-{unique}"));
        let _ = std::fs::create_dir_all(&directory);
        let final_path = directory.join("blob.bin");
        let part_path = part_path_for(&final_path);

        let writer = DownloadWriter::new();
        writer.start(99, part_path.clone(), final_path.clone(), 8, true);
        writer.write(99, 0, Arc::from(&b"abcd"[..]));
        writer.write(99, 4, Arc::from(&b"efgh"[..]));
        writer.flush(99);
        writer.finish(99);
        writer.shutdown();

        assert_eq!(
            std::fs::read(&final_path).expect("finalized file"),
            b"abcdefgh"
        );
        assert!(!part_path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
