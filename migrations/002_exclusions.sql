-- Schema version 2: automatic Spotlight exclusion bookkeeping.
--
-- `exclusions` records only paths this application knows about.  `managed = 1`
-- means the path was added to Spotlight's Search Privacy list by this
-- application; `managed = 0` means it was already present (for example the user
-- added it in System Settings).  Reconcile never removes a `managed = 0` entry.
--
-- `exclusion_opt_outs` records explicit `spotsweep unexclude` requests so a
-- later reconcile does not immediately add the same directory back.

CREATE TABLE IF NOT EXISTS exclusions (
    path            TEXT PRIMARY KEY,
    directory_id    INTEGER REFERENCES directories(id) ON DELETE SET NULL,
    managed         INTEGER NOT NULL DEFAULT 0 CHECK (managed IN (0, 1)),
    state           TEXT NOT NULL
                    CHECK (state IN ('pending', 'excluded', 'failed', 'released')),
    source          TEXT NOT NULL DEFAULT 'auto'
                    CHECK (source IN ('auto', 'cli', 'user')),
    added_at        INTEGER NOT NULL,
    verified_at     INTEGER,
    last_checked_at INTEGER,
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_exclusions_state ON exclusions(state);
CREATE INDEX IF NOT EXISTS idx_exclusions_managed ON exclusions(managed);

CREATE TABLE IF NOT EXISTS exclusion_opt_outs (
    path       TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    reason     TEXT
);
