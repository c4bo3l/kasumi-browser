//! Async SQLite persistence layer (WAL mode).
//!
//! Handles:
//! - Crash-safe session tab restore on startup.
//! - Visited URL recording (`history`).
//! - Bookmarks.
//! - Background worker via `tokio::sync::mpsc::UnboundedSender<DbCommand>` to ensure
//!   zero disk contention on the UI thread or CEF threads.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::{
    BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon, PREFERENCES_DB_KEY,
};
use crate::events::{DownloadEngine, DownloadInfo, DownloadState};

#[derive(Debug, Clone)]
pub struct SessionTabSnapshot {
    pub workspace_id: u64,
    pub url: String,
    pub title: String,
    pub is_active: bool,
    pub is_hibernated: bool,
    pub is_pinned: bool,
}

/// A globally visible bookmark. `folder` is the owning workspace's name, so a
/// saved page stays reachable from every other workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkItem {
    pub id: i64,
    pub workspace_id: u64,
    pub url: String,
    pub title: String,
    pub folder: String,
}

#[derive(Debug, Clone)]
pub struct HistoryItem {
    pub url: String,
    pub title: String,
    pub visit_count: u32,
    pub last_visited: u64,
}

#[derive(Debug)]
pub enum DbCommand {
    RecordVisit {
        url: String,
        title: String,
    },
    SaveSession {
        tabs: Vec<SessionTabSnapshot>,
    },
    SaveWorkspaces {
        workspaces: Vec<Workspace>,
    },
    AddBookmark {
        workspace_id: u64,
        url: String,
        title: String,
        folder: String,
    },
    RenameBookmarkFolder {
        workspace_id: u64,
        from: String,
        to: String,
    },
    SavePreferences {
        preferences: BrowserPreferences,
    },
    SaveDownload {
        item: DownloadInfo,
    },
    RemoveDownload {
        download_id: u32,
    },
    ClearCompletedDownloads,
    FlushWal,
}

#[derive(Clone)]
pub struct StorageHandle {
    sender: UnboundedSender<DbCommand>,
}

impl StorageHandle {
    pub fn send(&self, cmd: DbCommand) {
        let _ = self.sender.send(cmd);
    }

    pub fn record_visit(&self, url: String, title: String) {
        self.send(DbCommand::RecordVisit { url, title });
    }

    pub fn save_session(&self, tabs: Vec<SessionTabSnapshot>) {
        self.send(DbCommand::SaveSession { tabs });
    }

    pub fn save_workspaces(&self, workspaces: Vec<Workspace>) {
        self.send(DbCommand::SaveWorkspaces { workspaces });
    }

    pub fn add_bookmark(&self, workspace_id: u64, url: String, title: String, folder: String) {
        self.send(DbCommand::AddBookmark {
            workspace_id,
            url,
            title,
            folder,
        });
    }

    pub fn rename_bookmark_folder(&self, workspace_id: u64, from: String, to: String) {
        self.send(DbCommand::RenameBookmarkFolder {
            workspace_id,
            from,
            to,
        });
    }

    pub fn save_preferences(&self, preferences: BrowserPreferences) {
        self.send(DbCommand::SavePreferences { preferences });
    }

    pub fn save_download(&self, item: DownloadInfo) {
        self.send(DbCommand::SaveDownload { item });
    }

    pub fn remove_download(&self, download_id: u32) {
        self.send(DbCommand::RemoveDownload { download_id });
    }

    pub fn clear_completed_downloads(&self) {
        self.send(DbCommand::ClearCompletedDownloads);
    }

    pub fn flush_wal(&self) {
        self.send(DbCommand::FlushWal);
    }
}

pub struct StorageManager {
    db_path: PathBuf,
}

impl StorageManager {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Initialize SQLite connection and apply WAL schema, including in-place
    /// migrations for databases created before workspaces existed.
    pub fn init_db(db_path: &Path) -> Result<Connection, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(db_path)?;
        conn.execute_batch(include_str!("schema.sql"))?;

        // The native segmented engine was added in the same 0.7 release as the
        // downloads table. Existing development databases still get an in-place
        // migration so `SaveDownload` never references a missing column.
        if !table_has_column(&conn, "downloads", "engine")? {
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN engine TEXT NOT NULL DEFAULT 'cef';",
            )?;
        }
        if !table_has_column(&conn, "downloads", "segments")? {
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN segments INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
        if !table_has_column(&conn, "downloads", "temp_path")? {
            conn.execute_batch(
                "ALTER TABLE downloads ADD COLUMN temp_path TEXT NOT NULL DEFAULT '';",
            )?;
        }

        // Databases created by the first Workspaces build predate the default
        // workspace flag. Add it before any statement references the column;
        // otherwise the INSERT below fails with "table workspaces has no
        // column named is_default" and the storage worker never starts.
        if !table_has_column(&conn, "workspaces", "is_default")? {
            conn.execute_batch(
                "ALTER TABLE workspaces ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // Every database, fresh or legacy, gets a stable default workspace.
        // Existing tabs and bookmarks are assigned to it below.
        conn.execute(
            "INSERT OR IGNORE INTO workspaces
             (id, name, icon, color, is_default, is_active, sort_index)
             VALUES (?1, ?2, ?3, ?4, 1, 1, 0)",
            params![
                Workspace::DEFAULT_ID as i64,
                Workspace::DEFAULT_NAME,
                WorkspaceIcon::default().storage_key(),
                WorkspaceColor::default().storage_key(),
            ],
        )?;

        if !table_has_column(&conn, "session_tabs", "is_pinned")? {
            conn.execute_batch(
                "ALTER TABLE session_tabs ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if !table_has_column(&conn, "session_tabs", "workspace_id")? {
            conn.execute_batch(
                "ALTER TABLE session_tabs ADD COLUMN workspace_id INTEGER NOT NULL DEFAULT 1;",
            )?;
        }

        if !table_has_column(&conn, "bookmarks", "workspace_id")? {
            // Legacy bookmarks used a global UNIQUE(url) constraint. Recreate
            // the table so the same URL can be saved once per workspace.
            let _ = conn.execute_batch("DROP INDEX IF EXISTS idx_bookmarks_folder;");
            conn.execute_batch("ALTER TABLE bookmarks RENAME TO bookmarks_legacy;")?;
            conn.execute_batch(
                "CREATE TABLE bookmarks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    workspace_id INTEGER NOT NULL DEFAULT 1,
                    url TEXT NOT NULL,
                    title TEXT NOT NULL DEFAULT '',
                    folder TEXT NOT NULL DEFAULT 'Personal',
                    created_at INTEGER NOT NULL,
                    UNIQUE(workspace_id, url)
                 );
                 INSERT OR REPLACE INTO bookmarks
                    (workspace_id, url, title, folder, created_at)
                 SELECT 1, url, title, 'Personal', created_at FROM bookmarks_legacy;
                 DROP TABLE bookmarks_legacy;
                 CREATE INDEX IF NOT EXISTS idx_bookmarks_folder ON bookmarks(folder);",
            )?;
        }

        // Older releases used a "Toolbar" folder; workspaces use the workspace
        // name as the folder so bookmarks remain globally browsable.
        conn.execute(
            "UPDATE bookmarks SET folder = ?1
             WHERE workspace_id = ?2 AND folder = 'Toolbar'",
            params![Workspace::DEFAULT_NAME, Workspace::DEFAULT_ID as i64],
        )?;

        // Keep exactly one active workspace even if an older session wrote
        // several active flags or none at all.
        let active_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE is_active = 1",
            [],
            |row| row.get(0),
        )?;
        if active_count != 1 {
            conn.execute_batch("UPDATE workspaces SET is_active = 0;")?;
            conn.execute(
                "UPDATE workspaces SET is_active = 1 WHERE id = ?1",
                params![Workspace::DEFAULT_ID as i64],
            )?;
        }

        // Keep exactly one default workspace. Unlike `is_active`, this flag is
        // only changed when the user picks a startup workspace in Settings.
        let default_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE is_default = 1",
            [],
            |row| row.get(0),
        )?;
        if default_count != 1 {
            conn.execute_batch("UPDATE workspaces SET is_default = 0;")?;
            let fallback_id: i64 = conn
                .query_row(
                    "SELECT id FROM workspaces WHERE is_active = 1 ORDER BY id LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(Workspace::DEFAULT_ID as i64);
            conn.execute(
                "UPDATE workspaces SET is_default = 1 WHERE id = ?1",
                params![fallback_id],
            )?;
        }

        // Created after migrations so legacy databases gain the column first.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_session_tabs_workspace
             ON session_tabs(workspace_id, tab_index);",
        )?;

        Ok(conn)
    }

    /// Load the previous session tabs on startup (instant startup with hibernated tabs).
    pub fn load_session(db_path: &Path) -> Result<Vec<SessionTabSnapshot>, rusqlite::Error> {
        let conn = Self::init_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT workspace_id, url, title, is_active, is_hibernated, is_pinned
             FROM session_tabs ORDER BY workspace_id ASC, tab_index ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(SessionTabSnapshot {
                workspace_id: row.get::<_, i64>(0)? as u64,
                url: row.get(1)?,
                title: row.get(2)?,
                is_active: row.get::<_, i32>(3)? != 0,
                is_hibernated: row.get::<_, i32>(4)? != 0,
                is_pinned: row.get::<_, i32>(5)? != 0,
            })
        })?;

        let mut snapshots = Vec::new();
        for r in rows {
            if let Ok(item) = r {
                snapshots.push(item);
            }
        }
        Ok(snapshots)
    }

    /// Load all workspaces. A missing database still yields the default workspace.
    pub fn load_workspaces(db_path: &Path) -> Result<Vec<Workspace>, rusqlite::Error> {
        let conn = Self::init_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT id, name, icon, color, is_default, is_active
             FROM workspaces ORDER BY sort_index ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Workspace {
                id: row.get::<_, i64>(0)? as u64,
                name: row.get(1)?,
                icon: WorkspaceIcon::from_storage_key(&row.get::<_, String>(2)?),
                color: WorkspaceColor::from_storage_key(&row.get::<_, String>(3)?),
                is_default: row.get::<_, i32>(4)? != 0,
                is_active: row.get::<_, i32>(5)? != 0,
            })
        })?;

        let mut workspaces = Vec::new();
        for row in rows {
            if let Ok(workspace) = row {
                workspaces.push(workspace);
            }
        }
        if workspaces.is_empty() {
            workspaces.push(Workspace::default_workspace());
        }
        Ok(workspaces)
    }

    /// Load bookmarks across every workspace folder.
    pub fn load_bookmarks(db_path: &Path) -> Result<Vec<BookmarkItem>, rusqlite::Error> {
        let conn = Self::init_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, url, title, folder
             FROM bookmarks ORDER BY folder ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BookmarkItem {
                id: row.get(0)?,
                workspace_id: row.get::<_, i64>(1)? as u64,
                url: row.get(2)?,
                title: row.get(3)?,
                folder: row.get(4)?,
            })
        })?;

        let mut bookmarks = Vec::new();
        for row in rows {
            if let Ok(bookmark) = row {
                bookmarks.push(bookmark);
            }
        }
        Ok(bookmarks)
    }

    /// Load persisted downloads, newest first, for the native downloads bubble.
    pub fn load_downloads(db_path: &Path) -> Result<Vec<DownloadInfo>, rusqlite::Error> {
        let conn = Self::init_db(db_path)?;
        // CEF download callbacks never survive a restart. Normalize stale
        // active rows so the bubble offers Retry instead of dead controls.
        conn.execute(
            "UPDATE downloads
             SET state = 'interrupted', current_speed = 0
             WHERE state IN ('in_progress', 'paused')",
            [],
        )?;
        let mut stmt = conn.prepare(
            "SELECT download_id, tab_id, url, suggested_name, full_path, state,
                    received_bytes, total_bytes, percent_complete, current_speed,
                    engine, segments, temp_path, started_at, updated_at
             FROM downloads ORDER BY updated_at DESC, download_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DownloadInfo {
                id: row.get::<_, i64>(0)? as u32,
                tab_id: row.get::<_, i64>(1)? as u64,
                url: row.get(2)?,
                suggested_name: row.get(3)?,
                full_path: row.get(4)?,
                state: DownloadState::from_str(&row.get::<_, String>(5)?),
                received_bytes: row.get(6)?,
                total_bytes: row.get(7)?,
                percent_complete: row.get::<_, i64>(8)? as i32,
                current_speed: row.get(9)?,
                engine: DownloadEngine::from_str(&row.get::<_, String>(10)?),
                segments: row.get::<_, i64>(11)? as u8,
                temp_path: row.get(12)?,
                started_at: row.get::<_, i64>(13)? as u64,
                updated_at: row.get::<_, i64>(14)? as u64,
            })
        })?;

        let mut downloads = Vec::new();
        for row in rows {
            if let Ok(item) = row {
                downloads.push(item);
            }
        }
        Ok(downloads)
    }

    /// Load persisted preferences. Corrupt or legacy data falls back to the
    /// shipped defaults instead of blocking startup.
    pub fn load_preferences(db_path: &Path) -> BrowserPreferences {
        let Ok(conn) = Self::init_db(db_path) else {
            return BrowserPreferences::default();
        };
        let Ok(json) = conn.query_row(
            "SELECT value FROM preferences WHERE key = ?1",
            params![PREFERENCES_DB_KEY],
            |row| row.get::<_, String>(0),
        ) else {
            return BrowserPreferences::default();
        };

        match BrowserPreferences::from_json(&json) {
            Ok(preferences) => preferences,
            Err(e) => {
                tracing::warn!("Ignoring invalid stored preferences: {e}");
                BrowserPreferences::default()
            }
        }
    }

    /// Search history by prefix for omnibox autocomplete.
    pub fn query_history_prefix(
        db_path: &Path,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<HistoryItem>, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        let pattern = format!("{}%", prefix);
        let mut stmt = conn.prepare(
            "SELECT url, title, visit_count, last_visited FROM history 
             WHERE url LIKE ?1 OR title LIKE ?1 
             ORDER BY visit_count DESC, last_visited DESC LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(HistoryItem {
                url: row.get(0)?,
                title: row.get(1)?,
                visit_count: row.get(2)?,
                last_visited: row.get(3)?,
            })
        })?;

        let mut items = Vec::new();
        for r in rows {
            if let Ok(item) = r {
                items.push(item);
            }
        }
        Ok(items)
    }

    /// Spawn dedicated background database worker thread.
    pub fn start_worker(&self) -> StorageHandle {
        let (tx, rx) = unbounded_channel::<DbCommand>();
        let db_path = self.db_path.clone();

        std::thread::Builder::new()
            .name("sqlite-worker".to_string())
            .spawn(move || {
                run_worker_loop(db_path, rx);
            })
            .expect("Failed to spawn sqlite worker thread");

        StorageHandle { sender: tx }
    }
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in columns {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply one persisted command. Kept synchronous and side-effect-free outside
/// SQLite so the worker loop and unit tests exercise identical behavior.
fn apply_command(conn: &mut Connection, cmd: DbCommand) {
    match cmd {
        DbCommand::RecordVisit { url, title } => {
            let now = now_epoch_secs();
            let res = conn.execute(
                "INSERT INTO history (url, title, visit_count, last_visited)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(url) DO UPDATE SET
                     visit_count = visit_count + 1,
                     last_visited = ?3,
                     title = CASE WHEN ?2 != '' THEN ?2 ELSE title END",
                params![url, title, now],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to record history visit: {}", e);
            }
        }
        DbCommand::SaveSession { tabs } => {
            let tx = match conn.transaction() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("Failed to begin session save transaction: {}", e);
                    return;
                }
            };

            let _ = tx.execute("DELETE FROM session_tabs", []);

            for (idx, tab) in tabs.iter().enumerate() {
                let _ = tx.execute(
                    "INSERT INTO session_tabs
                     (tab_index, workspace_id, url, title, is_active, is_hibernated, is_pinned)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        idx as i64,
                        tab.workspace_id as i64,
                        tab.url,
                        tab.title,
                        if tab.is_active { 1 } else { 0 },
                        if tab.is_hibernated { 1 } else { 0 },
                        if tab.is_pinned { 1 } else { 0 },
                    ],
                );
            }

            if let Err(e) = tx.commit() {
                tracing::warn!("Failed to commit session save transaction: {}", e);
            }
        }
        DbCommand::SaveWorkspaces { workspaces } => {
            let tx = match conn.transaction() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("Failed to begin workspace save transaction: {}", e);
                    return;
                }
            };
            let _ = tx.execute("DELETE FROM workspaces", []);
            for workspace in &workspaces {
                let _ = tx.execute(
                    "INSERT INTO workspaces
                     (id, name, icon, color, is_default, is_active, sort_index)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        workspace.id as i64,
                        Workspace::normalize_name(&workspace.name),
                        workspace.icon.storage_key(),
                        workspace.color.storage_key(),
                        if workspace.is_default { 1 } else { 0 },
                        if workspace.is_active { 1 } else { 0 },
                        workspace.id as i64,
                    ],
                );
            }
            if let Err(e) = tx.commit() {
                tracing::warn!("Failed to commit workspace save transaction: {}", e);
            }
        }
        DbCommand::AddBookmark {
            workspace_id,
            url,
            title,
            folder,
        } => {
            let now = now_epoch_secs();
            let res = conn.execute(
                "INSERT INTO bookmarks (workspace_id, url, title, folder, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(workspace_id, url) DO UPDATE SET title = ?3, folder = ?4",
                params![workspace_id as i64, url, title, folder, now],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to add bookmark: {}", e);
            }
        }
        DbCommand::RenameBookmarkFolder {
            workspace_id,
            from,
            to,
        } => {
            let res = conn.execute(
                "UPDATE bookmarks SET folder = ?3
                 WHERE workspace_id = ?1 AND folder = ?2",
                params![workspace_id as i64, from, to],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to rename bookmark folder: {}", e);
            }
        }
        DbCommand::SavePreferences { preferences } => {
            let json = match preferences.to_json() {
                Ok(json) => json,
                Err(e) => {
                    tracing::warn!("Failed to serialize preferences: {e}");
                    return;
                }
            };
            let res = conn.execute(
                "INSERT INTO preferences (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![PREFERENCES_DB_KEY, json],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to save preferences: {e}");
            }
        }
        DbCommand::SaveDownload { item } => {
            let res = conn.execute(
                "INSERT INTO downloads
                 (download_id, tab_id, url, suggested_name, full_path, state,
                  received_bytes, total_bytes, percent_complete, current_speed,
                  engine, segments, temp_path, started_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(download_id) DO UPDATE SET
                     tab_id = excluded.tab_id,
                     url = excluded.url,
                     suggested_name = excluded.suggested_name,
                     full_path = excluded.full_path,
                     state = excluded.state,
                     received_bytes = excluded.received_bytes,
                     total_bytes = excluded.total_bytes,
                     percent_complete = excluded.percent_complete,
                     current_speed = excluded.current_speed,
                     engine = excluded.engine,
                     segments = excluded.segments,
                     temp_path = excluded.temp_path,
                     started_at = excluded.started_at,
                     updated_at = excluded.updated_at",
                params![
                    item.id as i64,
                    item.tab_id as i64,
                    item.url,
                    item.suggested_name,
                    item.full_path,
                    item.state.as_str(),
                    item.received_bytes,
                    item.total_bytes,
                    item.percent_complete as i64,
                    item.current_speed,
                    item.engine.as_str(),
                    item.segments as i64,
                    item.temp_path,
                    item.started_at as i64,
                    item.updated_at as i64,
                ],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to save download: {}", e);
            }
        }
        DbCommand::RemoveDownload { download_id } => {
            let res = conn.execute(
                "DELETE FROM downloads WHERE download_id = ?1",
                params![download_id as i64],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to remove download: {}", e);
            }
        }
        DbCommand::ClearCompletedDownloads => {
            let res = conn.execute(
                "DELETE FROM downloads WHERE state IN ('completed', 'canceled', 'interrupted')",
                [],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to clear completed downloads: {}", e);
            }
        }
        DbCommand::FlushWal => {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }
}

fn run_worker_loop(db_path: PathBuf, mut rx: UnboundedReceiver<DbCommand>) {
    let mut conn = match StorageManager::init_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to open SQLite database at {:?}: {}", db_path, e);
            return;
        }
    };

    tracing::info!("SQLite storage worker started at {:?}", db_path);

    while let Some(cmd) = rx.blocking_recv() {
        apply_command(&mut conn, cmd);
    }

    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    tracing::info!("SQLite storage worker shut down cleanly");
}

#[cfg(test)]
mod tests {
    use super::{apply_command, DbCommand, SessionTabSnapshot, StorageManager};
    use crate::config::{BrowserPreferences, Workspace, WorkspaceColor, WorkspaceIcon};
    use crate::events::{DownloadEngine, DownloadInfo, DownloadState};
    use rusqlite::params;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_db_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "kasumi-db-test-{label}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn remove_test_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(candidate));
        }
    }

    fn fresh_db(label: &str) -> (PathBuf, rusqlite::Connection) {
        let path = test_db_path(label);
        let conn = StorageManager::init_db(&path).expect("test database should initialize");
        (path, conn)
    }

    #[test]
    fn init_db_creates_schema_in_wal_mode() {
        let (path, conn) = fresh_db("schema");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode query");
        assert_eq!(mode, "wal");

        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("sqlite_master query")
            .query_map([], |row| row.get(0))
            .expect("table rows")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            tables,
            vec![
                "bookmarks",
                "downloads",
                "history",
                "preferences",
                "session_tabs",
                "workspaces",
            ]
        );

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn init_db_migrates_workspaces_without_default_flag() {
        let path = test_db_path("default-workspace-migration");
        {
            // This is the schema shipped by the first Workspaces build.
            let conn = rusqlite::Connection::open(&path).expect("legacy database");
            conn.execute_batch(
                "CREATE TABLE workspaces (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     name TEXT NOT NULL UNIQUE,
                     icon TEXT NOT NULL DEFAULT 'work',
                     color TEXT NOT NULL DEFAULT 'blue',
                     is_active INTEGER NOT NULL DEFAULT 0,
                     sort_index INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces (id, name, icon, color, is_active, sort_index)
                 VALUES
                     (1, 'Atreus', 'work', 'blue', 0, 0),
                     (2, 'Atreus 2', 'school', 'purple', 1, 1);",
            )
            .expect("legacy workspaces schema");
        }

        let workspaces = StorageManager::load_workspaces(&path).expect("migrated workspaces load");
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "Atreus");
        assert!(!workspaces[0].is_default);
        assert!(!workspaces[0].is_active);
        assert_eq!(workspaces[1].name, "Atreus 2");
        assert!(workspaces[1].is_default);
        assert!(workspaces[1].is_active);

        remove_test_db(&path);
    }

    #[test]
    fn init_db_migrates_legacy_session_tabs_with_pinned_column() {
        let path = test_db_path("session-migration");
        {
            let conn = rusqlite::Connection::open(&path).expect("legacy database");
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE session_tabs (
                     tab_index INTEGER PRIMARY KEY,
                     url TEXT NOT NULL,
                     title TEXT NOT NULL DEFAULT '',
                     is_active INTEGER NOT NULL DEFAULT 0,
                     is_hibernated INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO session_tabs (tab_index, url, title, is_active, is_hibernated)
                 VALUES (0, 'https://legacy.example', 'Legacy', 1, 0);",
            )
            .expect("legacy schema");
        }

        let snapshots = StorageManager::load_session(&path).expect("migrated session load");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].url, "https://legacy.example");
        assert!(!snapshots[0].is_pinned);

        remove_test_db(&path);
    }

    #[test]
    fn session_saves_and_loads_flags_in_order() {
        let (path, mut conn) = fresh_db("session");
        assert!(StorageManager::load_session(&path)
            .expect("empty load")
            .is_empty());

        apply_command(
            &mut conn,
            DbCommand::SaveSession {
                tabs: vec![
                    SessionTabSnapshot {
                        workspace_id: 1,
                        url: "https://active.example".to_string(),
                        title: "Active".to_string(),
                        is_active: true,
                        is_hibernated: false,
                        is_pinned: true,
                    },
                    SessionTabSnapshot {
                        workspace_id: 1,
                        url: "view-source:https://example.com".to_string(),
                        title: "Source".to_string(),
                        is_active: false,
                        is_hibernated: true,
                        is_pinned: false,
                    },
                ],
            },
        );

        let snapshots = StorageManager::load_session(&path).expect("saved session load");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].url, "https://active.example");
        assert!(snapshots[0].is_active);
        assert!(!snapshots[0].is_hibernated);
        assert!(snapshots[0].is_pinned);
        assert_eq!(snapshots[1].url, "view-source:https://example.com");
        assert!(!snapshots[1].is_active);
        assert!(snapshots[1].is_hibernated);
        assert!(!snapshots[1].is_pinned);

        apply_command(
            &mut conn,
            DbCommand::SaveSession {
                tabs: vec![SessionTabSnapshot {
                    workspace_id: 1,
                    url: "https://only.example".to_string(),
                    title: "Only".to_string(),
                    is_active: true,
                    is_hibernated: false,
                    is_pinned: false,
                }],
            },
        );
        let replaced = StorageManager::load_session(&path).expect("replaced session load");
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].url, "https://only.example");

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn workspaces_save_and_load_with_active_state() {
        let (path, mut conn) = fresh_db("workspaces");
        let default = StorageManager::load_workspaces(&path).expect("default workspace");
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].name, Workspace::DEFAULT_NAME);
        assert!(default[0].is_default);
        assert!(default[0].is_active);

        apply_command(
            &mut conn,
            DbCommand::SaveWorkspaces {
                workspaces: vec![
                    Workspace {
                        id: 1,
                        name: "Personal".to_string(),
                        icon: WorkspaceIcon::Work,
                        color: WorkspaceColor::Blue,
                        is_default: false,
                        is_active: false,
                    },
                    Workspace {
                        id: 2,
                        name: "Studio".to_string(),
                        icon: WorkspaceIcon::Palette,
                        color: WorkspaceColor::Purple,
                        is_default: true,
                        is_active: true,
                    },
                ],
            },
        );

        let saved = StorageManager::load_workspaces(&path).expect("saved workspaces");
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].name, "Studio");
        assert_eq!(saved[1].icon, WorkspaceIcon::Palette);
        assert_eq!(saved[1].color, WorkspaceColor::Purple);
        assert!(saved[1].is_default);
        assert!(saved[1].is_active);
        assert!(!saved[0].is_default);
        assert!(!saved[0].is_active);

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn bookmarks_are_grouped_by_workspace_folder() {
        let (path, mut conn) = fresh_db("bookmark-folders");
        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
                workspace_id: 2,
                url: "https://one.example".to_string(),
                title: "One".to_string(),
                folder: "Work".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
                workspace_id: 1,
                url: "https://two.example".to_string(),
                title: "Two".to_string(),
                folder: "Personal".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
                workspace_id: 1,
                url: "https://archived.example".to_string(),
                title: "Archived".to_string(),
                folder: "Archived Work".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::RenameBookmarkFolder {
                workspace_id: 2,
                from: "Work".to_string(),
                to: "Studio".to_string(),
            },
        );

        let bookmarks = StorageManager::load_bookmarks(&path).expect("bookmarks");
        assert_eq!(bookmarks.len(), 3);
        assert!(bookmarks.iter().any(|bookmark| {
            bookmark.folder == "Studio" && bookmark.url == "https://one.example"
        }));
        assert!(bookmarks.iter().any(|bookmark| {
            bookmark.folder == "Personal" && bookmark.url == "https://two.example"
        }));
        assert!(bookmarks.iter().any(|bookmark| {
            bookmark.folder == "Archived Work" && bookmark.url == "https://archived.example"
        }));

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn preferences_save_and_load_with_fallback() {
        let (path, mut conn) = fresh_db("preferences");
        assert_eq!(
            StorageManager::load_preferences(&path),
            BrowserPreferences::default()
        );

        let prefs = BrowserPreferences {
            theme: crate::config::ThemePreference::Dark,
            rail_expanded: false,
            adblock_enabled: false,
            idle_timeout_minutes: 15,
            search_engine: crate::config::SearchEngine::Google,
        };
        apply_command(
            &mut conn,
            DbCommand::SavePreferences {
                preferences: prefs.clone(),
            },
        );
        assert_eq!(StorageManager::load_preferences(&path), prefs);

        conn.execute(
            "UPDATE preferences SET value = 'not json' WHERE key = ?1",
            params![crate::config::PREFERENCES_DB_KEY],
        )
        .expect("corrupt preferences row");
        assert_eq!(
            StorageManager::load_preferences(&path),
            BrowserPreferences::default()
        );

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn visits_increment_count_and_preserve_title_when_blank() {
        let (path, mut conn) = fresh_db("visits");

        apply_command(
            &mut conn,
            DbCommand::RecordVisit {
                url: "https://example.com/a".to_string(),
                title: "Example A".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::RecordVisit {
                url: "https://example.com/a".to_string(),
                title: String::new(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::RecordVisit {
                url: "https://example.com/b".to_string(),
                title: "Example B".to_string(),
            },
        );

        let items = StorageManager::query_history_prefix(&path, "https://example.com", 10)
            .expect("history");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "https://example.com/a");
        assert_eq!(items[0].visit_count, 2);
        assert_eq!(items[0].title, "Example A");

        let limited =
            StorageManager::query_history_prefix(&path, "https://example.com", 1).expect("limit");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].url, "https://example.com/a");

        let miss = StorageManager::query_history_prefix(&path, "https://missing", 10)
            .expect("missing prefix");
        assert!(miss.is_empty());

        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn bookmarks_upsert_by_url() {
        let (path, mut conn) = fresh_db("bookmarks");

        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
                workspace_id: 1,
                url: "https://example.com".to_string(),
                title: "First".to_string(),
                folder: "Toolbar".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
                workspace_id: 1,
                url: "https://example.com".to_string(),
                title: "Updated".to_string(),
                folder: "Reading".to_string(),
            },
        );

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))
            .expect("bookmark count");
        assert_eq!(count, 1);

        let (title, folder): (String, String) = conn
            .query_row(
                "SELECT title, folder FROM bookmarks WHERE url = ?1",
                params!["https://example.com"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("bookmark row");
        assert_eq!(title, "Updated");
        assert_eq!(folder, "Reading");

        apply_command(&mut conn, DbCommand::FlushWal);
        drop(conn);
        remove_test_db(&path);
    }

    fn download(state: DownloadState, id: u32, updated_at: u64) -> DownloadInfo {
        DownloadInfo {
            id,
            tab_id: 4,
            url: "https://example.com/file.zip".to_string(),
            suggested_name: "file.zip".to_string(),
            full_path: "/Downloads/file.zip".to_string(),
            received_bytes: 50,
            total_bytes: 100,
            percent_complete: 50,
            current_speed: 2048,
            state,
            engine: DownloadEngine::Parallel,
            segments: 4,
            temp_path: "/Downloads/file.zip.kasumi.part".to_string(),
            started_at: 100,
            updated_at,
        }
    }

    #[test]
    fn downloads_upsert_and_load_in_update_order() {
        let (path, mut conn) = fresh_db("downloads-upsert");
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::InProgress, 1, 10),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Paused, 2, 30),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Completed, 1, 40),
            },
        );

        let loaded = StorageManager::load_downloads(&path).expect("load downloads");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, 1);
        assert_eq!(loaded[0].state, DownloadState::Completed);
        assert_eq!(loaded[0].updated_at, 40);
        assert_eq!(loaded[0].engine, DownloadEngine::Parallel);
        assert_eq!(loaded[0].segments, 4);
        assert_eq!(loaded[0].temp_path, "/Downloads/file.zip.kasumi.part");
        assert_eq!(loaded[1].id, 2);

        apply_command(&mut conn, DbCommand::FlushWal);
        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn init_db_migrates_legacy_downloads_with_parallel_columns() {
        let path = test_db_path("downloads-migration");
        {
            let conn = rusqlite::Connection::open(&path).expect("legacy database");
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE downloads (
                     download_id INTEGER PRIMARY KEY,
                     tab_id INTEGER NOT NULL DEFAULT 0,
                     url TEXT NOT NULL,
                     suggested_name TEXT NOT NULL DEFAULT '',
                     full_path TEXT NOT NULL DEFAULT '',
                     state TEXT NOT NULL DEFAULT 'in_progress',
                     received_bytes INTEGER NOT NULL DEFAULT 0,
                     total_bytes INTEGER NOT NULL DEFAULT 0,
                     percent_complete INTEGER NOT NULL DEFAULT 0,
                     current_speed INTEGER NOT NULL DEFAULT 0,
                     started_at INTEGER NOT NULL DEFAULT 0,
                     updated_at INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO downloads
                     (download_id, url, suggested_name, full_path, state, updated_at)
                 VALUES (7, 'https://legacy.example/file.zip', 'file.zip',
                         '/Downloads/file.zip', 'completed', 9);",
            )
            .expect("legacy downloads schema");
        }

        let loaded = StorageManager::load_downloads(&path).expect("migrated downloads load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 7);
        assert_eq!(loaded[0].engine, DownloadEngine::Cef);
        assert_eq!(loaded[0].segments, 1);
        assert_eq!(loaded[0].temp_path, "");

        remove_test_db(&path);
    }

    #[test]
    fn stale_downloads_are_marked_interrupted_on_restart() {
        let (path, mut conn) = fresh_db("downloads-restart");
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::InProgress, 1, 10),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Paused, 2, 20),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Completed, 3, 30),
            },
        );

        let loaded = StorageManager::load_downloads(&path).expect("load downloads");
        assert_eq!(loaded.len(), 3);
        assert!(loaded
            .iter()
            .all(|item| item.state != DownloadState::InProgress
                && item.state != DownloadState::Paused));
        assert_eq!(loaded[0].id, 3);
        assert_eq!(loaded[0].state, DownloadState::Completed);
        assert!(loaded[1..]
            .iter()
            .all(|item| item.state == DownloadState::Interrupted && item.current_speed == 0));

        apply_command(&mut conn, DbCommand::FlushWal);
        drop(conn);
        remove_test_db(&path);
    }

    #[test]
    fn completed_downloads_can_be_removed_or_cleared() {
        let (path, mut conn) = fresh_db("downloads-cleanup");
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::InProgress, 1, 10),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Completed, 2, 20),
            },
        );

        apply_command(&mut conn, DbCommand::ClearCompletedDownloads);
        let loaded = StorageManager::load_downloads(&path).expect("load downloads");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 1);

        apply_command(
            &mut conn,
            DbCommand::SaveDownload {
                item: download(DownloadState::Canceled, 3, 30),
            },
        );
        apply_command(&mut conn, DbCommand::RemoveDownload { download_id: 3 });
        let loaded = StorageManager::load_downloads(&path).expect("load downloads");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 1);

        apply_command(&mut conn, DbCommand::FlushWal);
        drop(conn);
        remove_test_db(&path);
    }
}
