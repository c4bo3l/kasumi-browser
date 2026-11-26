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

CREATE TABLE IF NOT EXISTS downloads (
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
    engine TEXT NOT NULL DEFAULT 'cef',
    segments INTEGER NOT NULL DEFAULT 1,
    temp_path TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_downloads_state_updated
    ON downloads(state, updated_at DESC);

CREATE TABLE IF NOT EXISTS preferences (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Remembered camera/microphone decisions. Private tabs never write here.
CREATE TABLE IF NOT EXISTS site_permissions (
    origin TEXT NOT NULL,
    permission TEXT NOT NULL,
    allowed INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (origin, permission)
);
