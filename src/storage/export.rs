//! Portable exports of Kasumi bookmarks and history.
//!
//! Bookmarks use the Netscape bookmark HTML format understood by Chrome,
//! Edge, Firefox, and Safari. Browsers do not share a history import format,
//! so history is exported as CSV with both Unix and ISO-8601 UTC timestamps.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::db::{BookmarkItem, HistoryItem};

#[derive(Debug, Default)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
    pub errors: Vec<String>,
}

impl ExportResult {
    pub fn success(&self) -> bool {
        self.errors.is_empty() && !self.files.is_empty()
    }
}

#[derive(Default)]
struct FolderNode<'a> {
    bookmarks: Vec<&'a BookmarkItem>,
    children: BTreeMap<String, FolderNode<'a>>,
}

fn escape_html(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escape_csv(raw: &str) -> String {
    if raw.chars().any(|ch| matches!(ch, ',' | '"' | '\n' | '\r')) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

/// Convert Unix epoch seconds into a compact UTC timestamp without adding a
/// date-time dependency to the shell.
pub fn format_iso_utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let seconds = epoch % 86_400;

    // Howard Hinnant's civil_from_days algorithm, shifted to the Unix epoch.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn insert_bookmark<'a>(root: &mut FolderNode<'a>, bookmark: &'a BookmarkItem) {
    let folder = bookmark.folder.trim();
    let parts: Vec<&str> = if folder.is_empty() {
        vec!["Bookmarks"]
    } else {
        folder
            .split(" / ")
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect()
    };
    let parts = if parts.is_empty() {
        vec!["Bookmarks"]
    } else {
        parts
    };

    let mut node = root;
    for part in parts {
        node = node
            .children
            .entry(part.to_string())
            .or_insert_with(FolderNode::default);
    }
    node.bookmarks.push(bookmark);
}

fn render_folder<'a>(out: &mut String, name: &str, node: &FolderNode<'a>, depth: usize) {
    let indent = "    ".repeat(depth);
    out.push_str(&format!("{indent}<DT><H3>{}</H3>\n", escape_html(name)));
    out.push_str(&format!("{indent}<DL><p>\n"));

    let mut bookmarks = node.bookmarks.clone();
    bookmarks.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then_with(|| a.url.cmp(&b.url))
            .then(a.id.cmp(&b.id))
    });
    let child_indent = "    ".repeat(depth + 1);
    for bookmark in bookmarks {
        let title = if bookmark.title.trim().is_empty() {
            bookmark.url.as_str()
        } else {
            bookmark.title.as_str()
        };
        out.push_str(&format!(
            "{child_indent}<DT><A HREF=\"{}\" ADD_DATE=\"{}\">{}</A>\n",
            escape_html(&bookmark.url),
            bookmark.id.max(0),
            escape_html(title)
        ));
    }
    for (name, child) in &node.children {
        render_folder(out, name, child, depth + 1);
    }

    out.push_str(&format!("{indent}</DL><p>\n"));
}

/// Render bookmarks as a Netscape bookmark file that other browsers can import.
pub fn bookmarks_html(bookmarks: &[BookmarkItem]) -> String {
    let mut root = FolderNode::default();
    for bookmark in bookmarks {
        insert_bookmark(&mut root, bookmark);
    }

    let mut out = String::from(
        "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n\
         <!-- This is an automatically generated file. -->\n\
         <META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n\
         <TITLE>Bookmarks</TITLE>\n\
         <H1>Bookmarks</H1>\n\
         <DL><p>\n",
    );
    for (name, node) in &root.children {
        render_folder(&mut out, name, node, 1);
    }
    out.push_str("</DL><p>\n");
    out
}

/// Render history as portable CSV. `last_visited_epoch` is seconds since the
/// Unix epoch; `last_visited_utc` is the same instant in ISO-8601 UTC.
pub fn history_csv(history: &[HistoryItem]) -> String {
    let mut out = String::from("url,title,visit_count,last_visited_epoch,last_visited_utc\n");
    for item in history {
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            escape_csv(&item.url),
            escape_csv(&item.title),
            item.visit_count,
            item.last_visited,
            escape_csv(&format_iso_utc(item.last_visited))
        ));
    }
    out
}

/// Write the requested export files into `directory`. All disk work happens on
/// the caller's background thread; this function never touches UI state.
pub fn export_data(
    directory: &Path,
    stamp: u64,
    bookmarks: Option<&[BookmarkItem]>,
    history: Option<&[HistoryItem]>,
) -> ExportResult {
    let mut result = ExportResult::default();
    if let Err(error) = fs::create_dir_all(directory) {
        result
            .errors
            .push(format!("Cannot open export folder: {error}"));
        return result;
    }

    if let Some(bookmarks) = bookmarks {
        let path = directory.join(format!("kasumi-bookmarks-{stamp}.html"));
        match fs::write(&path, bookmarks_html(bookmarks)) {
            Ok(()) => result.files.push(path),
            Err(error) => result
                .errors
                .push(format!("Could not export bookmarks: {error}")),
        }
    }

    if let Some(history) = history {
        let path = directory.join(format!("kasumi-history-{stamp}.csv"));
        match fs::write(&path, history_csv(history)) {
            Ok(()) => result.files.push(path),
            Err(error) => result
                .errors
                .push(format!("Could not export history: {error}")),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark(id: i64, folder: &str, url: &str, title: &str) -> BookmarkItem {
        BookmarkItem {
            id,
            workspace_id: 1,
            url: url.to_string(),
            title: title.to_string(),
            folder: folder.to_string(),
        }
    }

    #[test]
    fn bookmarks_html_nests_folders_and_escapes_fields() {
        let bookmarks = vec![
            bookmark(2, "Imported / Dev", "https://a.example/?x=1&y=2", "A & B"),
            bookmark(1, "Imported / Dev", "https://b.example", "B <C>"),
            bookmark(3, "", "https://c.example", ""),
        ];
        let html = bookmarks_html(&bookmarks);

        assert!(html.starts_with("<!DOCTYPE NETSCAPE-Bookmark-file-1>"));
        assert!(html.contains("<DT><H3>Imported</H3>"));
        assert!(html.contains("<DT><H3>Dev</H3>"));
        assert!(html.contains("HREF=\"https://a.example/?x=1&amp;y=2\""));
        assert!(html.contains("A &amp; B"));
        assert!(html.contains("B &lt;C&gt;"));
        assert!(html.contains("<H3>Bookmarks</H3>"));
        assert!(html.contains("https://c.example</A>"));
        assert!(html.ends_with("</DL><p>\n"));
    }

    #[test]
    fn history_csv_quotes_fields_and_formats_utc() {
        assert_eq!(format_iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");

        let history = vec![HistoryItem {
            url: "https://a.example/?x=1,y=2".to_string(),
            title: "A \"quote\"\nline".to_string(),
            visit_count: 3,
            last_visited: 1_700_000_000,
        }];
        let csv = history_csv(&history);
        assert!(csv.starts_with("url,title,visit_count,last_visited_epoch,last_visited_utc\n"));
        assert!(csv.contains("\"https://a.example/?x=1,y=2\""));
        assert!(csv.contains("\"A \"\"quote\"\"\nline\""));
        assert!(csv.contains("3,1700000000,2023-11-14T22:13:20Z"));
    }

    #[test]
    fn export_data_writes_only_requested_files() {
        let directory = std::env::temp_dir().join(format!(
            "kasumi-export-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)
        ));
        let bookmarks = vec![bookmark(1, "Personal", "https://example", "Example")];
        let result = export_data(&directory, 42, Some(&bookmarks), None);

        assert!(result.success());
        assert_eq!(result.files.len(), 1);
        assert_eq!(
            result.files[0].file_name().and_then(|name| name.to_str()),
            Some("kasumi-bookmarks-42.html")
        );
        let contents = fs::read_to_string(&result.files[0]).expect("read exported bookmarks");
        assert!(contents.contains("https://example"));
        fs::remove_dir_all(&directory).ok();
    }
}
