//! SQLite storage. See `schema.rs` for the tables.

mod schema;

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::adapters::AdapterInfo;
use crate::error::{Error, Result};
use crate::incident::{Incident, Triggers};

/// A new `sample` row.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleRow<'a> {
    pub incident_id: i64,
    pub ts_utc: &'a str,
    pub cpu_total: f64,
    pub gpu_max_engine: f64,
    pub gpu_sum: f64,
    pub max_proc_core: f64,
    pub triggers: Triggers,
}

/// A new `sample_gpu_engine` row.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRow<'a> {
    pub sample_id: i64,
    pub adapter_luid: &'a str,
    pub phys: u32,
    pub eng: u32,
    pub engine_type: &'a str,
    pub utilization: f64,
}

/// A new `process` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRow<'a> {
    pub pid: u32,
    pub create_time_utc: &'a str,
    pub name: &'a str,
    pub image_path: Option<&'a str>,
    pub command_line: Option<&'a str>,
    pub user_name: Option<&'a str>,
    pub session_id: Option<u32>,
    pub parent_pid: Option<u32>,
    pub parent_process_id: Option<i64>,
    pub info_error: Option<&'a str>,
    pub seen_utc: &'a str,
}

/// A new `process_sample` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSampleRow<'a> {
    pub sample_id: i64,
    pub process_id: i64,
    pub cpu_pct_machine: f64,
    pub cpu_pct_core: f64,
    pub gpu_pct: f64,
    pub gpu_engine: Option<&'a str>,
    pub gpu_adapter_luid: Option<&'a str>,
    pub working_set_bytes: Option<u64>,
}

/// The cpumon database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and brings its schema up to
    /// date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// Opens a private in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: usize = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?
            as usize;
        let latest = schema::MIGRATIONS.len();
        if version > latest {
            return Err(Error::Schema(format!(
                "database schema version {version} is newer than this cpumon supports ({latest})"
            )));
        }
        for (i, sql) in schema::MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// The schema version.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// The underlying connection, for queries.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Runs `f` in a transaction, committing if it succeeds.
    pub fn in_transaction<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Records adapter names.
    pub fn upsert_adapters(&self, adapters: &[AdapterInfo]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO adapter (luid, description) VALUES (?1, ?2)
             ON CONFLICT (luid) DO UPDATE SET description = excluded.description",
        )?;
        for a in adapters {
            stmt.execute(params![a.luid.to_string(), a.description])?;
        }
        Ok(())
    }

    /// Closes incidents left open by a previous run that did not shut down cleanly,
    /// ending them at their last sample. Returns how many were closed.
    pub fn close_open_incidents(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE incident
             SET ended_utc = COALESCE(
                 (SELECT MAX(ts_utc) FROM sample WHERE sample.incident_id = incident.id),
                 started_utc)
             WHERE ended_utc IS NULL",
            [],
        )?)
    }

    /// Creates an incident and returns its id.
    pub fn insert_incident(&self, started_utc: &str) -> Result<i64> {
        self.conn
            .prepare_cached("INSERT INTO incident (started_utc) VALUES (?1)")?
            .execute([started_utc])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Updates an incident's summary, and its end time once it has ended.
    pub fn update_incident(
        &self,
        id: i64,
        incident: &Incident,
        ended_utc: Option<&str>,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE incident
                 SET triggers = ?2, samples = ?3, peak_cpu = ?4, peak_gpu = ?5,
                     peak_proc_core = ?6, ended_utc = COALESCE(?7, ended_utc)
                 WHERE id = ?1",
            )?
            .execute(params![
                id,
                incident.triggers.to_string(),
                incident.samples,
                incident.peak_cpu,
                incident.peak_gpu,
                incident.peak_proc_core,
                ended_utc,
            ])?;
        Ok(())
    }

    pub fn insert_sample(&self, row: &SampleRow<'_>) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO sample (incident_id, ts_utc, cpu_total, gpu_max_engine, gpu_sum,
                                     max_proc_core, triggers)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                row.incident_id,
                row.ts_utc,
                row.cpu_total,
                row.gpu_max_engine,
                row.gpu_sum,
                row.max_proc_core,
                row.triggers.to_string(),
            ])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_engine(&self, row: &EngineRow<'_>) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO sample_gpu_engine
                     (sample_id, adapter_luid, phys, eng, engine_type, utilization)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                row.sample_id,
                row.adapter_luid,
                row.phys,
                row.eng,
                row.engine_type,
                row.utilization,
            ])?;
        Ok(())
    }

    /// Finds a process by identity.
    pub fn find_process(&self, pid: u32, create_time_utc: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM process WHERE pid = ?1 AND create_time_utc = ?2")?
            .query_row(params![pid, create_time_utc], |r| r.get(0))
            .optional()?)
    }

    pub fn insert_process(&self, row: &ProcessRow<'_>) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO process (pid, create_time_utc, name, image_path, command_line,
                                      user_name, session_id, parent_pid, parent_process_id,
                                      info_error, first_seen_utc, last_seen_utc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            )?
            .execute(params![
                row.pid,
                row.create_time_utc,
                row.name,
                row.image_path,
                row.command_line,
                row.user_name,
                row.session_id,
                row.parent_pid,
                row.parent_process_id,
                row.info_error,
                row.seen_utc,
            ])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Updates when a process was last seen as an offender.
    pub fn touch_process(&self, id: i64, seen_utc: &str) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE process SET last_seen_utc = ?2 WHERE id = ?1")?
            .execute(params![id, seen_utc])?;
        Ok(())
    }

    pub fn insert_process_sample(&self, row: &ProcessSampleRow<'_>) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO process_sample
                     (sample_id, process_id, cpu_pct_machine, cpu_pct_core, gpu_pct,
                      gpu_engine, gpu_adapter_luid, working_set_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                row.sample_id,
                row.process_id,
                row.cpu_pct_machine,
                row.cpu_pct_core,
                row.gpu_pct,
                row.gpu_engine,
                row.gpu_adapter_luid,
                row.working_set_bytes.map(|b| b as i64),
            ])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::Luid;

    fn process_row<'a>(pid: u32, ct: &'a str, parent: Option<i64>) -> ProcessRow<'a> {
        ProcessRow {
            pid,
            create_time_utc: ct,
            name: "app.exe",
            image_path: Some(r"C:\app.exe"),
            command_line: Some("app.exe --flag"),
            user_name: Some(r"PC\me"),
            session_id: Some(1),
            parent_pid: Some(1),
            parent_process_id: parent,
            info_error: None,
            seen_utc: "2026-01-01T00:00:00.0000000Z",
        }
    }

    #[test]
    fn migrates_new_database_to_latest() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
    }

    #[test]
    fn reopening_file_database_keeps_data_and_version() {
        let dir = std::env::temp_dir().join(format!("cpumon-test-{}", std::process::id()));
        let path = dir.join("nested").join("test.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .insert_incident("2026-01-01T00:00:00.0000000Z")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM incident", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        assert!(matches!(Store::init(conn), Err(Error::Schema(_))));
    }

    #[test]
    fn records_incident_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .insert_incident("2026-01-01T00:00:00.0000000Z")
            .unwrap();
        let incident = Incident {
            triggers: Triggers {
                cpu: true,
                gpu: false,
                core: true,
            },
            samples: 3,
            peak_cpu: 90.0,
            peak_gpu: 10.0,
            peak_proc_core: 100.0,
        };
        store.update_incident(id, &incident, None).unwrap();
        let (triggers, ended): (String, Option<String>) = store
            .connection()
            .query_row("SELECT triggers, ended_utc FROM incident", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((triggers.as_str(), ended), ("cpu+core", None));

        store
            .update_incident(id, &incident, Some("2026-01-01T00:01:00.0000000Z"))
            .unwrap();
        // A later update without an end time keeps the end time.
        store.update_incident(id, &incident, None).unwrap();
        let ended: String = store
            .connection()
            .query_row("SELECT ended_utc FROM incident", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ended, "2026-01-01T00:01:00.0000000Z");
    }

    #[test]
    fn closes_incidents_left_open_at_last_sample() {
        let store = Store::open_in_memory().unwrap();
        let with_samples = store
            .insert_incident("2026-01-01T00:00:00.0000000Z")
            .unwrap();
        store
            .insert_incident("2026-01-02T00:00:00.0000000Z")
            .unwrap();
        for ts in [
            "2026-01-01T00:00:02.0000000Z",
            "2026-01-01T00:00:04.0000000Z",
        ] {
            store
                .insert_sample(&SampleRow {
                    incident_id: with_samples,
                    ts_utc: ts,
                    cpu_total: 1.0,
                    gpu_max_engine: 2.0,
                    gpu_sum: 3.0,
                    max_proc_core: 4.0,
                    triggers: Triggers::default(),
                })
                .unwrap();
        }
        assert_eq!(store.close_open_incidents().unwrap(), 2);
        let ended: Vec<String> = store
            .connection()
            .prepare("SELECT ended_utc FROM incident ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            ended,
            [
                "2026-01-01T00:00:04.0000000Z",
                "2026-01-02T00:00:00.0000000Z"
            ]
        );
        assert_eq!(store.close_open_incidents().unwrap(), 0);
    }

    #[test]
    fn stores_and_finds_processes_with_parent_links() {
        let store = Store::open_in_memory().unwrap();
        let parent = store
            .insert_process(&process_row(100, "2026-01-01T00:00:00.0000000Z", None))
            .unwrap();
        let child = store
            .insert_process(&process_row(
                200,
                "2026-01-01T00:00:01.0000000Z",
                Some(parent),
            ))
            .unwrap();
        assert_eq!(
            store
                .find_process(200, "2026-01-01T00:00:01.0000000Z")
                .unwrap(),
            Some(child)
        );
        // Same PID, different create time: a different process.
        assert_eq!(
            store
                .find_process(200, "2026-01-01T00:00:09.0000000Z")
                .unwrap(),
            None
        );
        // Duplicate identity is rejected.
        assert!(
            store
                .insert_process(&process_row(200, "2026-01-01T00:00:01.0000000Z", None))
                .is_err()
        );
        let linked: i64 = store
            .connection()
            .query_row(
                "SELECT parent_process_id FROM process WHERE id = ?1",
                [child],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked, parent);
    }

    #[test]
    fn records_samples_engines_and_offenders() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_adapters(&[AdapterInfo {
                luid: Luid { high: 0, low: 7 },
                description: "Test GPU".into(),
            }])
            .unwrap();
        let incident = store
            .insert_incident("2026-01-01T00:00:00.0000000Z")
            .unwrap();
        let sample = store
            .insert_sample(&SampleRow {
                incident_id: incident,
                ts_utc: "2026-01-01T00:00:00.0000000Z",
                cpu_total: 50.0,
                gpu_max_engine: 99.0,
                gpu_sum: 120.0,
                max_proc_core: 100.0,
                triggers: Triggers {
                    gpu: true,
                    ..Triggers::default()
                },
            })
            .unwrap();
        store
            .insert_engine(&EngineRow {
                sample_id: sample,
                adapter_luid: "0x00000000_0x00000007",
                phys: 0,
                eng: 0,
                engine_type: "3D",
                utilization: 99.0,
            })
            .unwrap();
        let process = store
            .insert_process(&process_row(100, "2026-01-01T00:00:00.0000000Z", None))
            .unwrap();
        store
            .in_transaction(|s| {
                s.insert_process_sample(&ProcessSampleRow {
                    sample_id: sample,
                    process_id: process,
                    cpu_pct_machine: 6.25,
                    cpu_pct_core: 100.0,
                    gpu_pct: 80.0,
                    gpu_engine: Some("3D"),
                    gpu_adapter_luid: Some("0x00000000_0x00000007"),
                    working_set_bytes: Some(1 << 20),
                })
            })
            .unwrap();

        let (adapter, gpu): (String, f64) = store
            .connection()
            .query_row(
                "SELECT a.description, ps.gpu_pct
                 FROM process_sample ps JOIN adapter a ON a.luid = ps.gpu_adapter_luid",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((adapter.as_str(), gpu), ("Test GPU", 80.0));
    }

    #[test]
    fn failed_transaction_rolls_back() {
        let store = Store::open_in_memory().unwrap();
        let result: Result<()> = store.in_transaction(|s| {
            s.insert_incident("2026-01-01T00:00:00.0000000Z")?;
            Err(Error::Schema("boom".into()))
        });
        assert!(result.is_err());
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM incident", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
