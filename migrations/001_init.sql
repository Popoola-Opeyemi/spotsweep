-- Schema version 1.
-- Times are Unix milliseconds.  All user data is local to this application;
-- nothing in this database changes Spotlight configuration.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS scan_roots (
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at      INTEGER NOT NULL,
    last_scanned_at INTEGER,
    last_scan_ok    INTEGER CHECK (last_scan_ok IN (0, 1) OR last_scan_ok IS NULL)
);

CREATE TABLE IF NOT EXISTS rules (
    id         INTEGER PRIMARY KEY,
    pattern    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    source     TEXT NOT NULL DEFAULT 'config',
    updated_at INTEGER NOT NULL,
    UNIQUE (pattern, kind)
);

CREATE TABLE IF NOT EXISTS directories (
    id                    INTEGER PRIMARY KEY,
    path                  TEXT NOT NULL UNIQUE,
    name                  TEXT NOT NULL,
    rule                  TEXT NOT NULL,
    rule_kind             TEXT NOT NULL,
    scan_root_id          INTEGER REFERENCES scan_roots(id) ON DELETE SET NULL,
    project_root          TEXT NOT NULL,
    first_seen_at         INTEGER NOT NULL,
    last_seen_at          INTEGER NOT NULL,
    status                TEXT NOT NULL DEFAULT 'discovered'
                          CHECK (status IN (
                            'discovered',
                            'pending_manual_exclusion',
                            'exported',
                            'manually_applied',
                            'verified',
                            'missing'
                          )),
    status_before_missing TEXT,
    last_export_id        INTEGER,
    exported_at           INTEGER,
    manually_applied_at   INTEGER,
    verified_at           INTEGER,
    missing_at            INTEGER,
    retry_count           INTEGER NOT NULL DEFAULT 0,
    last_error            TEXT
);

CREATE INDEX IF NOT EXISTS idx_directories_status
    ON directories(status);
CREATE INDEX IF NOT EXISTS idx_directories_scan_root
    ON directories(scan_root_id);
CREATE INDEX IF NOT EXISTS idx_directories_project
    ON directories(project_root);
CREATE INDEX IF NOT EXISTS idx_directories_last_seen
    ON directories(last_seen_at);

CREATE TABLE IF NOT EXISTS scans (
    id              INTEGER PRIMARY KEY,
    kind            TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    roots_configured INTEGER NOT NULL DEFAULT 0,
    roots_scanned   INTEGER NOT NULL DEFAULT 0,
    roots_missing   INTEGER NOT NULL DEFAULT 0,
    dirs_visited    INTEGER NOT NULL DEFAULT 0,
    matched         INTEGER NOT NULL DEFAULT 0,
    new_dirs        INTEGER NOT NULL DEFAULT 0,
    missing_dirs    INTEGER NOT NULL DEFAULT 0,
    errors          INTEGER NOT NULL DEFAULT 0,
    interrupted     INTEGER NOT NULL DEFAULT 0 CHECK (interrupted IN (0, 1)),
    note            TEXT
);

CREATE INDEX IF NOT EXISTS idx_scans_started ON scans(started_at);

CREATE TABLE IF NOT EXISTS exports (
    id          INTEGER PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    path        TEXT NOT NULL,
    selection   TEXT NOT NULL,
    item_count  INTEGER NOT NULL,
    note        TEXT
);

CREATE TABLE IF NOT EXISTS export_items (
    export_id    INTEGER NOT NULL REFERENCES exports(id) ON DELETE CASCADE,
    directory_id INTEGER NOT NULL REFERENCES directories(id) ON DELETE CASCADE,
    PRIMARY KEY (export_id, directory_id)
);

CREATE TABLE IF NOT EXISTS verification_runs (
    id             INTEGER PRIMARY KEY,
    started_at     INTEGER NOT NULL,
    finished_at    INTEGER,
    method         TEXT NOT NULL,
    outcome        TEXT NOT NULL,
    message        TEXT,
    checked_count  INTEGER NOT NULL DEFAULT 0,
    verified_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_verification_started
    ON verification_runs(started_at);

CREATE TABLE IF NOT EXISTS event_log (
    id     INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    kind   TEXT NOT NULL,
    detail TEXT
);

CREATE INDEX IF NOT EXISTS idx_event_log_at ON event_log(at);
