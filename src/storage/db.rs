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

use crate::config::{BrowserPreferences, PREFERENCES_DB_KEY};

#[derive(Debug, Clone)]
pub struct SessionTabSnapshot {
    pub url: String,
    pub title: String,
    pub is_active: bool,
    pub is_hibernated: bool,
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
    AddBookmark {
        url: String,
        title: String,
        folder: String,
    },
    SavePreferences {
        preferences: BrowserPreferences,
    },
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

    pub fn add_bookmark(&self, url: String, title: String, folder: String) {
        self.send(DbCommand::AddBookmark { url, title, folder });
    }

    pub fn save_preferences(&self, preferences: BrowserPreferences) {
        self.send(DbCommand::SavePreferences { preferences });
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

    /// Initialize SQLite connection and apply WAL schema.
    pub fn init_db(db_path: &Path) -> Result<Connection, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(db_path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(conn)
    }

    /// Load the previous session tabs on startup (instant startup with hibernated tabs).
    pub fn load_session(db_path: &Path) -> Result<Vec<SessionTabSnapshot>, rusqlite::Error> {
        let conn = Self::init_db(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT url, title, is_active, is_hibernated FROM session_tabs ORDER BY tab_index ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(SessionTabSnapshot {
                url: row.get(0)?,
                title: row.get(1)?,
                is_active: row.get::<_, i32>(2)? != 0,
                is_hibernated: row.get::<_, i32>(3)? != 0,
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
                    "INSERT INTO session_tabs (tab_index, url, title, is_active, is_hibernated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        idx as i64,
                        tab.url,
                        tab.title,
                        if tab.is_active { 1 } else { 0 },
                        if tab.is_hibernated { 1 } else { 0 },
                    ],
                );
            }

            if let Err(e) = tx.commit() {
                tracing::warn!("Failed to commit session save transaction: {}", e);
            }
        }
        DbCommand::AddBookmark { url, title, folder } => {
            let now = now_epoch_secs();
            let res = conn.execute(
                "INSERT INTO bookmarks (url, title, folder, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(url) DO UPDATE SET title = ?2, folder = ?3",
                params![url, title, folder, now],
            );
            if let Err(e) = res {
                tracing::warn!("Failed to add bookmark: {}", e);
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
    use crate::config::BrowserPreferences;
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
            vec!["bookmarks", "history", "preferences", "session_tabs"]
        );

        drop(conn);
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
                        url: "https://active.example".to_string(),
                        title: "Active".to_string(),
                        is_active: true,
                        is_hibernated: false,
                    },
                    SessionTabSnapshot {
                        url: "view-source:https://example.com".to_string(),
                        title: "Source".to_string(),
                        is_active: false,
                        is_hibernated: true,
                    },
                ],
            },
        );

        let snapshots = StorageManager::load_session(&path).expect("saved session load");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].url, "https://active.example");
        assert!(snapshots[0].is_active);
        assert!(!snapshots[0].is_hibernated);
        assert_eq!(snapshots[1].url, "view-source:https://example.com");
        assert!(!snapshots[1].is_active);
        assert!(snapshots[1].is_hibernated);

        apply_command(
            &mut conn,
            DbCommand::SaveSession {
                tabs: vec![SessionTabSnapshot {
                    url: "https://only.example".to_string(),
                    title: "Only".to_string(),
                    is_active: true,
                    is_hibernated: false,
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
                url: "https://example.com".to_string(),
                title: "First".to_string(),
                folder: "Toolbar".to_string(),
            },
        );
        apply_command(
            &mut conn,
            DbCommand::AddBookmark {
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
}
