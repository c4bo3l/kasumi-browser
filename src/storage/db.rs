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
                        continue;
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
            DbCommand::FlushWal => {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            }
        }
    }

    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    tracing::info!("SQLite storage worker shut down cleanly");
}
