PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 67108864;

CREATE TABLE IF NOT EXISTS workspaces (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    icon TEXT NOT NULL DEFAULT 'work',
    color TEXT NOT NULL DEFAULT 'blue',
    is_default INTEGER NOT NULL DEFAULT 0,
    is_active INTEGER NOT NULL DEFAULT 0,
    sort_index INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    visit_count INTEGER NOT NULL DEFAULT 1,
    last_visited INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_last_visited ON history(last_visited DESC);

CREATE TABLE IF NOT EXISTS bookmarks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id INTEGER NOT NULL DEFAULT 1,
    url TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT 'Personal',
    created_at INTEGER NOT NULL,
    UNIQUE(workspace_id, url)
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_folder ON bookmarks(folder);

CREATE TABLE IF NOT EXISTS session_tabs (
    tab_index INTEGER PRIMARY KEY,
    workspace_id INTEGER NOT NULL DEFAULT 1,
    url TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    is_active INTEGER NOT NULL DEFAULT 0,
    is_hibernated INTEGER NOT NULL DEFAULT 0,
    is_pinned INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS preferences (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
