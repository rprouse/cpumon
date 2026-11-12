//! Versioned schema migrations, tracked with `PRAGMA user_version`.
//!
//! Each entry upgrades the schema from version `i` to `i + 1`. Never edit a migration
//! that has shipped; add a new one instead.

pub(super) const MIGRATIONS: &[&str] = &[V1];

/// Initial schema. All `*_utc` columns hold fixed-width UTC strings such as
/// `2026-11-12T14:05:52.1234567Z`.
const V1: &str = r"
CREATE TABLE adapter (
    luid        TEXT PRIMARY KEY,
    description TEXT NOT NULL
);

CREATE TABLE incident (
    id             INTEGER PRIMARY KEY,
    started_utc    TEXT NOT NULL,
    ended_utc      TEXT,
    triggers       TEXT NOT NULL DEFAULT '',   -- e.g. 'cpu+core'
    samples        INTEGER NOT NULL DEFAULT 0,
    peak_cpu       REAL,
    peak_gpu       REAL,
    peak_proc_core REAL
);
CREATE INDEX incident_started ON incident (started_utc);

CREATE TABLE sample (
    id             INTEGER PRIMARY KEY,
    incident_id    INTEGER NOT NULL REFERENCES incident (id) ON DELETE CASCADE,
    ts_utc         TEXT NOT NULL,
    cpu_total      REAL NOT NULL,
    gpu_max_engine REAL NOT NULL,
    gpu_sum        REAL NOT NULL,
    max_proc_core  REAL NOT NULL,
    triggers       TEXT NOT NULL
);
CREATE INDEX sample_incident ON sample (incident_id);
CREATE INDEX sample_ts ON sample (ts_utc);

-- Busy GPU engines per sample (engines under 0.1% are omitted).
CREATE TABLE sample_gpu_engine (
    sample_id    INTEGER NOT NULL REFERENCES sample (id) ON DELETE CASCADE,
    adapter_luid TEXT NOT NULL,
    phys         INTEGER NOT NULL,
    eng          INTEGER NOT NULL,
    engine_type  TEXT NOT NULL,
    utilization  REAL NOT NULL,
    PRIMARY KEY (sample_id, adapter_luid, phys, eng)
) WITHOUT ROWID;

CREATE TABLE process (
    id                INTEGER PRIMARY KEY,
    pid               INTEGER NOT NULL,
    create_time_utc   TEXT NOT NULL,
    name              TEXT NOT NULL,
    image_path        TEXT,
    command_line      TEXT,
    user_name         TEXT,
    session_id        INTEGER,
    parent_pid        INTEGER,
    parent_process_id INTEGER REFERENCES process (id) ON DELETE SET NULL,
    info_error        TEXT,
    first_seen_utc    TEXT NOT NULL,
    last_seen_utc     TEXT NOT NULL,
    UNIQUE (pid, create_time_utc)
);
CREATE INDEX process_parent ON process (parent_process_id);

CREATE TABLE process_sample (
    sample_id         INTEGER NOT NULL REFERENCES sample (id) ON DELETE CASCADE,
    process_id        INTEGER NOT NULL REFERENCES process (id),
    cpu_pct_machine   REAL NOT NULL,
    cpu_pct_core      REAL NOT NULL,
    gpu_pct           REAL NOT NULL,
    gpu_engine        TEXT,
    gpu_adapter_luid  TEXT,
    working_set_bytes INTEGER,
    PRIMARY KEY (sample_id, process_id)
) WITHOUT ROWID;
CREATE INDEX process_sample_process ON process_sample (process_id);
";
