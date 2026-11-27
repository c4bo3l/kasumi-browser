//! Browser data import.
//!
//! Profile discovery and parsing run on a background thread. The UI receives
//! plain, owned records and forwards them to the SQLite worker in one batch, so
//! no filesystem or SQLite work happens on the CEF or winit threads.

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_BOOKMARKS: usize = 50_000;
const MAX_HISTORY: usize = 200_000;
const MAX_TITLE_LEN: usize = 512;
const MAX_FOLDER_LEN: usize = 256;
const MAX_FOLDER_PART_LEN: usize = 64;
const CHROME_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Browser vendors whose local profiles Kasumi can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserSource {
    Chrome,
    Edge,
    Firefox,
    Safari,
}

impl BrowserSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome / Chromium",
            Self::Edge => "Microsoft Edge",
            Self::Firefox => "Firefox",
            Self::Safari => "Safari",
        }
    }
}

/// A discovered local browser profile. Paths are optional because a profile
/// can contain bookmarks but no history file, as is the case for Safari.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfile {
    pub source: BrowserSource,
    pub bookmarks_path: Option<PathBuf>,
    pub history_path: Option<PathBuf>,
}

impl BrowserProfile {
    pub fn supports_bookmarks(&self) -> bool {
        self.bookmarks_path.is_some()
    }

    pub fn supports_history(&self) -> bool {
        self.history_path.is_some()
    }
}

/// Bookmark record ready for the SQLite worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedBookmark {
    pub url: String,
    pub title: String,
    pub folder: String,
}

/// History record ready for the SQLite worker. `last_visited` is Unix epoch
/// seconds, matching the existing `history.last_visited` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedVisit {
    pub url: String,
    pub title: String,
    pub visit_count: u32,
    pub last_visited: i64,
}

/// One parsed import batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedBrowserData {
    pub bookmarks: Vec<ImportedBookmark>,
    pub history: Vec<ImportedVisit>,
}

impl ImportedBrowserData {
    pub fn is_empty(&self) -> bool {
        self.bookmarks.is_empty() && self.history.is_empty()
    }
}

/// Outcome reported to native chrome after the SQLite worker applies a batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub bookmarks_found: usize,
    pub bookmarks_added: usize,
    pub history_found: usize,
    pub history_added: usize,
    pub canceled: bool,
    pub errors: Vec<String>,
}

impl ImportReport {
    pub fn canceled() -> Self {
        Self {
            canceled: true,
            ..Self::default()
        }
    }

    pub fn success(&self) -> bool {
        !self.canceled && self.errors.is_empty()
    }
}

/// Find local profiles that actually contain importable files. This performs
/// filesystem reads and must be called off the UI thread.
pub fn detect_profiles() -> Vec<BrowserProfile> {
    let mut profiles = Vec::new();
    for source in [
        BrowserSource::Chrome,
        BrowserSource::Edge,
        BrowserSource::Firefox,
        BrowserSource::Safari,
    ] {
        if let Some(profile) = detect_source(source) {
            profiles.push(profile);
        }
    }
    profiles
}

fn detect_source(source: BrowserSource) -> Option<BrowserProfile> {
    match source {
        BrowserSource::Chrome | BrowserSource::Edge => detect_chromium(source),
        BrowserSource::Firefox => detect_firefox(),
        BrowserSource::Safari => detect_safari(),
    }
}

fn detect_chromium(source: BrowserSource) -> Option<BrowserProfile> {
    let roots = chromium_roots(source);
    for root in roots {
        for profile_dir in chromium_profile_dirs(&root) {
            let bookmarks_path = profile_dir.join("Bookmarks");
            let history_path = profile_dir.join("History");
            let has_bookmarks = bookmarks_path.is_file();
            let has_history = history_path.is_file();
            if has_bookmarks || has_history {
                return Some(BrowserProfile {
                    source,
                    bookmarks_path: has_bookmarks.then_some(bookmarks_path),
                    history_path: has_history.then_some(history_path),
                });
            }
        }
    }
    None
}

fn chromium_roots(source: BrowserSource) -> Vec<PathBuf> {
    let home = home_dir();
    let local_app_data = local_app_data_dir();
    let mut roots = Vec::new();

    match source {
        BrowserSource::Chrome => {
            if let Some(home) = &home {
                roots.push(home.join("Library/Application Support/Google/Chrome"));
                roots.push(home.join(".config/google-chrome"));
                roots.push(home.join("snap/chromium/common/chromium"));
            }
            if let Some(local) = &local_app_data {
                roots.push(local.join("Google/Chrome/User Data"));
            }
        }
        BrowserSource::Edge => {
            if let Some(home) = &home {
                roots.push(home.join("Library/Application Support/Microsoft Edge"));
                roots.push(home.join(".config/microsoft-edge"));
            }
            if let Some(local) = &local_app_data {
                roots.push(local.join("Microsoft/Edge/User Data"));
            }
        }
        BrowserSource::Firefox | BrowserSource::Safari => {}
    }
    roots
}

fn chromium_profile_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.join("Default")];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("Profile ") {
                dirs.push(entry.path());
            }
        }
    }
    dirs
}

fn detect_firefox() -> Option<BrowserProfile> {
    let mut roots = Vec::new();
    if let Some(local) = local_app_data_dir() {
        roots.push(local.join("Mozilla/Firefox/Profiles"));
    }
    if let Some(home) = home_dir() {
        roots.extend([
            home.join("Library/Application Support/Firefox/Profiles"),
            home.join(".mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ]);
    }

    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join("places.sqlite");
            if path.is_file() {
                return Some(BrowserProfile {
                    source: BrowserSource::Firefox,
                    bookmarks_path: Some(path.clone()),
                    history_path: Some(path),
                });
            }
        }
    }
    None
}

fn detect_safari() -> Option<BrowserProfile> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let path = home_dir()?.join("Library/Safari/Bookmarks.plist");
    path.is_file().then_some(BrowserProfile {
        source: BrowserSource::Safari,
        bookmarks_path: Some(path),
        history_path: None,
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn local_app_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
}

/// Parse the selected parts of a profile. `progress` is called with short,
/// user-facing stage descriptions; the caller forwards them to native chrome.
pub fn read_profile(
    profile: &BrowserProfile,
    want_bookmarks: bool,
    want_history: bool,
    progress: impl Fn(&str),
) -> Result<ImportedBrowserData, String> {
    let mut data = ImportedBrowserData::default();

    if want_bookmarks {
        progress("Reading bookmarks");
        let Some(path) = profile.bookmarks_path.as_deref() else {
            return Err(format!("{} has no bookmarks file", profile.source.label()));
        };
        match profile.source {
            BrowserSource::Chrome | BrowserSource::Edge => {
                data.bookmarks = parse_chromium_bookmarks(path)?;
            }
            BrowserSource::Firefox => {
                data.bookmarks = with_readonly_copy(path, parse_firefox_bookmarks)?;
            }
            BrowserSource::Safari => {
                data.bookmarks = parse_safari_bookmarks(path)?;
            }
        }
    }

    if want_history {
        progress("Reading history");
        let Some(path) = profile.history_path.as_deref() else {
            return Err(format!(
                "{} history import is not available",
                profile.source.label()
            ));
        };
        match profile.source {
            BrowserSource::Chrome | BrowserSource::Edge => {
                data.history = with_readonly_copy(path, parse_chromium_history)?;
            }
            BrowserSource::Firefox => {
                data.history = with_readonly_copy(path, parse_firefox_history)?;
            }
            BrowserSource::Safari => {
                return Err("Safari history import is not available".to_string());
            }
        }
    }

    progress("Applying imported data");
    Ok(data)
}

#[derive(Debug, Deserialize, Default)]
struct ChromeBookmarksFile {
    #[serde(default)]
    roots: BTreeMap<String, ChromeBookmarkNode>,
}

#[derive(Debug, Deserialize, Default)]
struct ChromeBookmarkNode {
    #[serde(default)]
    children: Vec<ChromeBookmarkNode>,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    node_type: String,
    #[serde(default)]
    url: String,
}

fn parse_chromium_bookmarks(path: &Path) -> Result<Vec<ImportedBookmark>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("Could not read bookmarks: {error}"))?;
    let file: ChromeBookmarksFile = serde_json::from_str(&content)
        .map_err(|error| format!("Could not parse bookmarks: {error}"))?;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (key, root) in &file.roots {
        let label = if root.name.trim().is_empty() {
            key.clone()
        } else {
            root.name.clone()
        };
        let folder = join_folder("Imported Browser", &[label]);
        walk_chromium_children(&root.children, &folder, &mut out, &mut seen);
    }
    Ok(out)
}

fn walk_chromium_children(
    children: &[ChromeBookmarkNode],
    folder: &str,
    out: &mut Vec<ImportedBookmark>,
    seen: &mut HashSet<String>,
) {
    for node in children {
        if node.node_type == "url" || !node.url.trim().is_empty() {
            push_bookmark(out, seen, &node.url, &node.name, folder);
        }
        if node.node_type == "folder" || !node.children.is_empty() {
            let label = sanitize_text(&node.name, MAX_FOLDER_PART_LEN);
            let child_folder = if label.is_empty() {
                folder.to_string()
            } else {
                join_folder(folder, &[label])
            };
            walk_chromium_children(&node.children, &child_folder, out, seen);
        }
    }
}

fn parse_chromium_history(conn: &Connection) -> Result<Vec<ImportedVisit>, String> {
    let mut statement = conn
        .prepare(
            "SELECT url, IFNULL(title, ''), visit_count, last_visit_time \
             FROM urls WHERE url LIKE 'http%' AND last_visit_time > 0 \
             ORDER BY last_visit_time DESC LIMIT ?1",
        )
        .map_err(|error| format!("Could not read history: {error}"))?;
    let rows = statement
        .query_map([MAX_HISTORY as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| format!("Could not read history rows: {error}"))?;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for row in rows.flatten() {
        let Some(url) = normalize_url(&row.0) else {
            continue;
        };
        let last_visited = chrome_time_to_unix(row.3);
        push_visit(
            &mut out,
            &mut seen,
            url,
            &row.1,
            row.2.max(1) as u32,
            last_visited,
        );
    }
    Ok(out)
}

#[derive(Debug)]
struct FirefoxBookmarkNode {
    parent: i64,
    title: String,
    url: Option<String>,
}

fn parse_firefox_bookmarks(conn: &Connection) -> Result<Vec<ImportedBookmark>, String> {
    let mut statement = conn
        .prepare(
            "SELECT b.id, b.parent, IFNULL(b.title, ''), b.type, IFNULL(p.url, '') \
             FROM moz_bookmarks b LEFT JOIN moz_places p ON p.id = b.fk",
        )
        .map_err(|error| format!("Could not read Firefox bookmarks: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| format!("Could not read Firefox bookmark rows: {error}"))?;

    let mut nodes = HashMap::new();
    let mut urls = Vec::new();
    for row in rows.flatten() {
        let url = if row.4.is_empty() { None } else { Some(row.4) };
        let node = FirefoxBookmarkNode {
            parent: row.1,
            title: row.2,
            url,
        };
        if row.3 == 1 && node.url.is_some() {
            urls.push(row.0);
        }
        nodes.insert(row.0, node);
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in urls {
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let Some(url) = node.url.as_deref() else {
            continue;
        };
        let parts = firefox_folder_parts(&nodes, node.parent);
        let folder = join_folder("Imported Firefox", &parts);
        push_bookmark(&mut out, &mut seen, url, &node.title, &folder);
    }
    Ok(out)
}

fn firefox_folder_parts(nodes: &HashMap<i64, FirefoxBookmarkNode>, mut parent: i64) -> Vec<String> {
    let mut parts = Vec::new();
    let mut visited = HashSet::new();
    while parent > 0 && visited.insert(parent) {
        let Some(node) = nodes.get(&parent) else {
            break;
        };
        let title = sanitize_text(&node.title, MAX_FOLDER_PART_LEN);
        if !title.is_empty() && !matches!(title.as_str(), "menu" | "toolbar" | "unfiled" | "mobile")
        {
            parts.push(title);
        }
        parent = node.parent;
    }
    parts.reverse();
    parts
}

fn parse_firefox_history(conn: &Connection) -> Result<Vec<ImportedVisit>, String> {
    let mut statement = conn
        .prepare(
            "SELECT url, IFNULL(title, ''), visit_count, last_visit_date \
             FROM moz_places WHERE url LIKE 'http%' AND last_visit_date IS NOT NULL \
             ORDER BY last_visit_date DESC LIMIT ?1",
        )
        .map_err(|error| format!("Could not read Firefox history: {error}"))?;
    let rows = statement
        .query_map([MAX_HISTORY as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| format!("Could not read Firefox history rows: {error}"))?;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for row in rows.flatten() {
        let Some(url) = normalize_url(&row.0) else {
            continue;
        };
        let last_visited = (row.3 / 1_000_000).clamp(0, now_epoch_secs());
        push_visit(
            &mut out,
            &mut seen,
            url,
            &row.1,
            row.2.max(1) as u32,
            last_visited,
        );
    }
    Ok(out)
}

fn parse_safari_bookmarks(path: &Path) -> Result<Vec<ImportedBookmark>, String> {
    if !cfg!(target_os = "macos") {
        return Err("Safari bookmarks are only available on macOS".to_string());
    }
    let output = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .output()
        .map_err(|error| format!("Could not run plutil: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "plutil could not read Safari bookmarks".to_string()
        } else {
            detail
        });
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Could not parse Safari bookmarks: {error}"))?;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk_safari_value(&value, "Imported Safari", &mut out, &mut seen);
    Ok(out)
}

fn walk_safari_value(
    value: &Value,
    folder: &str,
    out: &mut Vec<ImportedBookmark>,
    seen: &mut HashSet<String>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_safari_value(item, folder, out, seen);
            }
        }
        Value::Object(object) => {
            let kind = object
                .get("WebBookmarkType")
                .and_then(Value::as_str)
                .unwrap_or("");
            let title = string_field(object, "Title").or_else(|| {
                object
                    .get("URIDictionary")
                    .and_then(Value::as_object)
                    .and_then(|uri| string_field(uri, "title"))
            });
            let url = string_field(object, "URLString");

            if kind == "WebBookmarkTypeLeaf" || url.is_some() {
                if let Some(url) = url {
                    push_bookmark(out, seen, &url, title.as_deref().unwrap_or(""), folder);
                }
            }

            if let Some(children) = object.get("Children").and_then(Value::as_array) {
                let child_folder = match title {
                    Some(title) if kind == "WebBookmarkTypeList" => {
                        let label = sanitize_text(&title, MAX_FOLDER_PART_LEN);
                        if label.is_empty() {
                            folder.to_string()
                        } else {
                            join_folder(folder, &[label])
                        }
                    }
                    _ => folder.to_string(),
                };
                for child in children {
                    walk_safari_value(child, &child_folder, out, seen);
                }
            }
        }
        _ => {}
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

fn with_readonly_copy<T>(
    path: &Path,
    read: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = std::env::temp_dir().join(format!(
        "kasumi-import-{}-{counter}.sqlite",
        std::process::id()
    ));
    std::fs::copy(path, &temp)
        .map_err(|error| format!("Could not copy browser database: {error}"))?;

    let result = Connection::open_with_flags(
        &temp,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Could not open browser database: {error}"))
    .and_then(|conn| read(&conn));

    remove_temp_db(&temp);
    result
}

fn remove_temp_db(path: &Path) {
    let mut suffixes = [String::new(), "-wal".to_string(), "-shm".to_string()];
    for suffix in &mut suffixes {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix.as_str());
        let _ = std::fs::remove_file(PathBuf::from(candidate));
    }
}

fn push_bookmark(
    out: &mut Vec<ImportedBookmark>,
    seen: &mut HashSet<String>,
    raw_url: &str,
    raw_title: &str,
    folder: &str,
) {
    if out.len() >= MAX_BOOKMARKS {
        return;
    }
    let Some(url) = normalize_url(raw_url) else {
        return;
    };
    if !seen.insert(url.clone()) {
        return;
    }
    out.push(ImportedBookmark {
        url,
        title: sanitize_text(raw_title, MAX_TITLE_LEN),
        folder: sanitize_text(folder, MAX_FOLDER_LEN),
    });
}

fn push_visit(
    out: &mut Vec<ImportedVisit>,
    seen: &mut HashSet<String>,
    url: String,
    raw_title: &str,
    visit_count: u32,
    last_visited: i64,
) {
    if out.len() >= MAX_HISTORY {
        return;
    }
    if let Some(existing) = out.iter_mut().find(|visit| visit.url == url) {
        existing.visit_count = existing
            .visit_count
            .saturating_add(visit_count)
            .min(u32::MAX);
        existing.last_visited = existing.last_visited.max(last_visited);
        if existing.title.is_empty() {
            existing.title = sanitize_text(raw_title, MAX_TITLE_LEN);
        }
        return;
    }
    if !seen.insert(url.clone()) {
        return;
    }
    out.push(ImportedVisit {
        url,
        title: sanitize_text(raw_title, MAX_TITLE_LEN),
        visit_count: visit_count.max(1),
        last_visited: last_visited.clamp(0, now_epoch_secs()),
    });
}

fn normalize_url(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().unwrap_or("").is_empty() {
        return None;
    }
    Some(parsed.to_string())
}

fn sanitize_text(raw: &str, max_chars: usize) -> String {
    raw.trim().chars().take(max_chars).collect()
}

fn join_folder(prefix: &str, parts: &[String]) -> String {
    let mut pieces = vec![prefix.to_string()];
    pieces.extend(
        parts
            .iter()
            .map(|part| sanitize_text(part, MAX_FOLDER_PART_LEN))
            .filter(|part| !part.is_empty()),
    );
    sanitize_text(&pieces.join(" / "), MAX_FOLDER_LEN)
}

fn chrome_time_to_unix(micros: i64) -> i64 {
    if micros <= 0 {
        return 0;
    }
    (micros / 1_000_000)
        .saturating_sub(CHROME_EPOCH_OFFSET_SECS)
        .clamp(0, now_epoch_secs())
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        chrome_time_to_unix, join_folder, parse_chromium_bookmarks, parse_chromium_history,
        parse_firefox_bookmarks, parse_firefox_history, push_visit, BrowserProfile, BrowserSource,
        ImportedBrowserData,
    };
    use rusqlite::Connection;
    use std::collections::HashSet;

    #[test]
    fn chrome_time_converts_from_1601_epoch() {
        assert_eq!(chrome_time_to_unix(0), 0);
        assert_eq!(chrome_time_to_unix(11_644_473_600_000_000), 0);
        assert_eq!(chrome_time_to_unix(13_000_000_000_000_000), 1_355_526_400);
    }

    #[test]
    fn parses_chromium_bookmark_tree_and_skips_non_web_urls() {
        let json = r#"{
            "roots": {
                "bookmark_bar": {
                    "name": "Bookmarks bar",
                    "type": "folder",
                    "children": [
                        {"type": "url", "name": "Rust", "url": "https://www.rust-lang.org/"},
                        {"type": "folder", "name": "Dev", "children": [
                            {"type": "url", "name": "Docs", "url": "https://doc.rust-lang.org/book/"},
                            {"type": "url", "name": "Bad", "url": "javascript:alert(1)"}
                        ]}
                    ]
                }
            }
        }"#;
        let path = std::env::temp_dir().join(format!(
            "kasumi-import-bookmarks-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).expect("write fixture");
        let bookmarks = parse_chromium_bookmarks(&path).expect("parse bookmarks");
        std::fs::remove_file(&path).expect("remove fixture");

        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].url, "https://www.rust-lang.org/");
        assert_eq!(bookmarks[0].folder, "Imported Browser / Bookmarks bar");
        assert_eq!(bookmarks[1].title, "Docs");
        assert!(bookmarks[1].folder.ends_with("Bookmarks bar / Dev"));
    }

    #[test]
    fn parses_chromium_history_and_deduplicates_by_url() {
        let conn = Connection::open_in_memory().expect("open db");
        conn.execute_batch(
            "CREATE TABLE urls (url TEXT, title TEXT, visit_count INTEGER, last_visit_time INTEGER);
             INSERT INTO urls VALUES
               ('https://example.com/', 'Example', 2, 13300000000000000),
               ('https://example.com/', 'Example Again', 3, 13400000000000000),
               ('file:///tmp/nope', 'Nope', 1, 13400000000000000);",
        )
        .expect("seed db");
        let history = parse_chromium_history(&conn).expect("parse history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].visit_count, 5);
        assert_eq!(history[0].last_visited, 1_755_526_400);
    }

    #[test]
    fn parses_firefox_bookmarks_with_folder_paths() {
        let conn = Connection::open_in_memory().expect("open db");
        conn.execute_batch(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
             CREATE TABLE moz_bookmarks (
                id INTEGER PRIMARY KEY, parent INTEGER, title TEXT, type INTEGER, fk INTEGER);
             INSERT INTO moz_places VALUES (1, 'https://example.com/a');
             INSERT INTO moz_bookmarks VALUES
                (1, 0, 'menu', 2, NULL),
                (2, 1, 'Reading', 2, NULL),
                (3, 2, 'Example', 1, 1);",
        )
        .expect("seed db");
        let bookmarks = parse_firefox_bookmarks(&conn).expect("parse bookmarks");
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].folder, "Imported Firefox / Reading");
    }

    #[test]
    fn parses_firefox_history_with_unix_microseconds() {
        let conn = Connection::open_in_memory().expect("open db");
        conn.execute_batch(
            "CREATE TABLE moz_places (url TEXT, title TEXT, visit_count INTEGER, last_visit_date INTEGER);
             INSERT INTO moz_places VALUES
               ('https://example.com/', 'Example', 4, 1700000000000000);",
        )
        .expect("seed db");
        let history = parse_firefox_history(&conn).expect("parse history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].visit_count, 4);
        assert_eq!(history[0].last_visited, 1_700_000_000);
    }

    #[test]
    fn folder_joins_are_bounded() {
        let folder = join_folder("Imported Browser", &["A".repeat(100).into()]);
        assert!(folder.chars().count() <= 256);
        assert!(folder.starts_with("Imported Browser / "));
    }

    #[test]
    fn canceled_report_is_not_a_success() {
        let report = super::ImportReport::canceled();
        assert!(report.canceled);
        assert!(!report.success());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn imported_data_reports_emptiness() {
        assert!(ImportedBrowserData::default().is_empty());
        let mut seen = HashSet::new();
        let mut data = ImportedBrowserData::default();
        push_visit(
            &mut data.history,
            &mut seen,
            "https://example.com".to_string(),
            "Example",
            1,
            1,
        );
        assert!(!data.is_empty());
    }

    #[test]
    fn safari_profiles_have_no_history_path() {
        let profile = BrowserProfile {
            source: BrowserSource::Safari,
            bookmarks_path: None,
            history_path: None,
        };
        assert!(!profile.supports_bookmarks());
        assert!(!profile.supports_history());
    }
}
