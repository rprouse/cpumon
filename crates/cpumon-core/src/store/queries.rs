//! Read queries for reports, and retention pruning.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, params};

use super::Store;
use crate::error::Result;

/// Seconds each sample covers: `interval_s`, or for rows recorded before it existed,
/// the gap since the previous sample of the same incident (0 for the first).
const SAMPLE_SECONDS: &str = "
    sample_seconds AS (
        SELECT s.id, s.incident_id, s.ts_utc,
               COALESCE(s.interval_s,
                        (julianday(s.ts_utc) - julianday(LAG(s.ts_utc) OVER (
                            PARTITION BY s.incident_id ORDER BY s.ts_utc))) * 86400,
                        0) AS seconds
        FROM sample s)";

/// One incident with its peaks and the process that contributed most.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentSummary {
    pub id: i64,
    pub started_utc: String,
    /// `None` while the incident is still in progress.
    pub ended_utc: Option<String>,
    pub triggers: String,
    pub samples: i64,
    pub peak_cpu: f64,
    pub peak_gpu: f64,
    pub peak_proc_core: f64,
    /// Longest thread wake-up delay, milliseconds (`None` before schema v3).
    pub peak_stall_ms: Option<f64>,
    /// Worst single CPU's DPC + interrupt percent (`None` before schema v3).
    pub peak_dpc: Option<f64>,
    /// Name of the process with the most CPU (capped at one core) plus GPU over the
    /// incident.
    pub top_process: Option<String>,
    /// Name of the process whose window was not responding in the most samples.
    pub hung_process: Option<String>,
}

/// A window that was not responding during an incident.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentHungWindow {
    pub process: ProcessInfo,
    pub title: String,
    /// Samples in which it was not responding.
    pub samples: i64,
    /// It was the foreground window in at least one of them.
    pub foreground: bool,
    pub first_utc: String,
    pub last_utc: String,
}

/// A recorded process instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub id: i64,
    pub pid: u32,
    pub create_time_utc: String,
    pub name: String,
    pub image_path: Option<String>,
    pub command_line: Option<String>,
    pub user_name: Option<String>,
    pub info_error: Option<String>,
    pub parent_process_id: Option<i64>,
}

/// A process's usage over one incident.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentProcess {
    pub process: ProcessInfo,
    pub samples: i64,
    /// CPU time in core-seconds (100% of one core for 1 s = 1).
    pub cpu_seconds: f64,
    /// GPU busy time in seconds (100% of an engine for 1 s = 1).
    pub gpu_seconds: f64,
    pub peak_cpu_core: f64,
    pub peak_cpu_machine: f64,
    pub peak_gpu: f64,
    /// Engine type and adapter at the process's GPU peak.
    pub gpu_engine: Option<String>,
    pub gpu_adapter: Option<String>,
    pub peak_working_set_bytes: Option<i64>,
}

/// One sample's machine totals.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleSummary {
    pub ts_utc: String,
    pub seconds: f64,
    pub cpu_total: f64,
    pub gpu_max_engine: f64,
    pub gpu_sum: f64,
    pub max_proc_core: f64,
    pub triggers: String,
    /// Responsiveness figures; `None` for samples recorded before schema v3 or where
    /// the counter is unavailable.
    pub stall_ms: Option<f64>,
    pub max_core_dpc: Option<f64>,
    pub cpu_performance: Option<f64>,
    pub temperature_c: Option<f64>,
    pub memory_available_mb: Option<f64>,
    pub memory_committed: Option<f64>,
    pub pages_input_per_sec: Option<f64>,
    pub hung_windows: Option<i64>,
}

/// A process's usage within one incident, the unit reports aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessUsage {
    pub process_id: i64,
    pub incident_id: i64,
    pub samples: i64,
    pub cpu_seconds: f64,
    pub gpu_seconds: f64,
    pub peak_cpu_core: f64,
    pub peak_gpu: f64,
    pub last_seen_utc: String,
}

/// What [`Store::prune`] deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub incidents: usize,
    pub processes: usize,
}

const PROCESS_COLUMNS: &str = "p.id, p.pid, p.create_time_utc, p.name, p.image_path, \
     p.command_line, p.user_name, p.info_error, p.parent_process_id";

fn process_from(row: &Row<'_>, at: usize) -> rusqlite::Result<ProcessInfo> {
    Ok(ProcessInfo {
        id: row.get(at)?,
        pid: row.get(at + 1)?,
        create_time_utc: row.get(at + 2)?,
        name: row.get(at + 3)?,
        image_path: row.get(at + 4)?,
        command_line: row.get(at + 5)?,
        user_name: row.get(at + 6)?,
        info_error: row.get(at + 7)?,
        parent_process_id: row.get(at + 8)?,
    })
}

const INCIDENT_SELECT: &str = "
    WITH ranked AS (
        SELECT s.incident_id, p.name,
               ROW_NUMBER() OVER (
                   PARTITION BY s.incident_id
                   ORDER BY SUM(MAX(MIN(ps.cpu_pct_core, 100), ps.gpu_pct)) DESC, p.name
               ) AS rank
        FROM process_sample ps
        JOIN sample s ON s.id = ps.sample_id
        JOIN process p ON p.id = ps.process_id
        GROUP BY s.incident_id, ps.process_id),
    hung AS (
        SELECT s.incident_id, p.name,
               ROW_NUMBER() OVER (
                   PARTITION BY s.incident_id ORDER BY COUNT(*) DESC, p.name
               ) AS rank
        FROM sample_hung_window h
        JOIN sample s ON s.id = h.sample_id
        JOIN process p ON p.id = h.process_id
        GROUP BY s.incident_id, h.process_id)
    SELECT i.id, i.started_utc, i.ended_utc, i.triggers, i.samples,
           COALESCE(i.peak_cpu, 0), COALESCE(i.peak_gpu, 0), COALESCE(i.peak_proc_core, 0),
           r.name, i.peak_stall_ms, i.peak_dpc, h.name
    FROM incident i
    LEFT JOIN ranked r ON r.incident_id = i.id AND r.rank = 1
    LEFT JOIN hung h ON h.incident_id = i.id AND h.rank = 1";

fn incident_from(row: &Row<'_>) -> rusqlite::Result<IncidentSummary> {
    Ok(IncidentSummary {
        id: row.get(0)?,
        started_utc: row.get(1)?,
        ended_utc: row.get(2)?,
        triggers: row.get(3)?,
        samples: row.get(4)?,
        peak_cpu: row.get(5)?,
        peak_gpu: row.get(6)?,
        peak_proc_core: row.get(7)?,
        top_process: row.get(8)?,
        peak_stall_ms: row.get(9)?,
        peak_dpc: row.get(10)?,
        hung_process: row.get(11)?,
    })
}

impl Store {
    /// Incidents that started at or after `since_utc`, newest first.
    pub fn incidents_since(&self, since_utc: &str) -> Result<Vec<IncidentSummary>> {
        let sql =
            format!("{INCIDENT_SELECT} WHERE i.started_utc >= ?1 ORDER BY i.started_utc DESC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([since_utc], incident_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One incident.
    pub fn incident(&self, id: i64) -> Result<Option<IncidentSummary>> {
        let sql = format!("{INCIDENT_SELECT} WHERE i.id = ?1");
        Ok(self.conn.query_row(&sql, [id], incident_from).optional()?)
    }

    /// The samples of an incident, oldest first.
    pub fn incident_samples(&self, incident_id: i64) -> Result<Vec<SampleSummary>> {
        let sql = format!(
            "WITH {SAMPLE_SECONDS}
             SELECT s.ts_utc, ss.seconds, s.cpu_total, s.gpu_max_engine, s.gpu_sum,
                    s.max_proc_core, s.triggers, s.stall_ms, s.max_core_dpc,
                    s.cpu_performance, s.temperature_c, s.memory_available_mb,
                    s.memory_committed, s.pages_input_per_sec, s.hung_windows
             FROM sample s JOIN sample_seconds ss ON ss.id = s.id
             WHERE s.incident_id = ?1
             ORDER BY s.ts_utc"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([incident_id], |r| {
            Ok(SampleSummary {
                ts_utc: r.get(0)?,
                seconds: r.get(1)?,
                cpu_total: r.get(2)?,
                gpu_max_engine: r.get(3)?,
                gpu_sum: r.get(4)?,
                max_proc_core: r.get(5)?,
                triggers: r.get(6)?,
                stall_ms: r.get(7)?,
                max_core_dpc: r.get(8)?,
                cpu_performance: r.get(9)?,
                temperature_c: r.get(10)?,
                memory_available_mb: r.get(11)?,
                memory_committed: r.get(12)?,
                pages_input_per_sec: r.get(13)?,
                hung_windows: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Windows that were not responding during an incident, longest first.
    pub fn incident_hung_windows(&self, incident_id: i64) -> Result<Vec<IncidentHungWindow>> {
        let sql = format!(
            "SELECT {PROCESS_COLUMNS}, h.title, COUNT(*), MAX(h.foreground),
                    MIN(s.ts_utc), MAX(s.ts_utc)
             FROM sample_hung_window h
             JOIN sample s ON s.id = h.sample_id
             JOIN process p ON p.id = h.process_id
             WHERE s.incident_id = ?1
             GROUP BY h.process_id, h.title
             ORDER BY COUNT(*) DESC, p.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([incident_id], |r| {
            Ok(IncidentHungWindow {
                process: process_from(r, 0)?,
                title: r.get(9)?,
                samples: r.get(10)?,
                foreground: r.get(11)?,
                first_utc: r.get(12)?,
                last_utc: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The processes recorded in an incident, by CPU (capped at one core) plus GPU
    /// time, highest first.
    pub fn incident_processes(&self, incident_id: i64) -> Result<Vec<IncidentProcess>> {
        let sql = format!(
            "WITH {SAMPLE_SECONDS},
             usage AS (
                 SELECT ps.process_id,
                        COUNT(*) AS samples,
                        SUM(ps.cpu_pct_core * ss.seconds) / 100 AS cpu_seconds,
                        SUM(ps.gpu_pct * ss.seconds) / 100 AS gpu_seconds,
                        SUM(MAX(MIN(ps.cpu_pct_core, 100), ps.gpu_pct) * ss.seconds) AS weight,
                        MAX(ps.cpu_pct_core) AS peak_cpu_core,
                        MAX(ps.cpu_pct_machine) AS peak_cpu_machine,
                        MAX(ps.gpu_pct) AS peak_gpu,
                        MAX(ps.working_set_bytes) AS peak_ws
                 FROM process_sample ps JOIN sample_seconds ss ON ss.id = ps.sample_id
                 WHERE ss.incident_id = ?1
                 GROUP BY ps.process_id),
             gpu_peak AS (
                 SELECT ps.process_id, ps.gpu_engine,
                        COALESCE(a.description, ps.gpu_adapter_luid) AS adapter,
                        ROW_NUMBER() OVER (
                            PARTITION BY ps.process_id ORDER BY ps.gpu_pct DESC) AS rank
                 FROM process_sample ps
                 JOIN sample s ON s.id = ps.sample_id
                 LEFT JOIN adapter a ON a.luid = ps.gpu_adapter_luid
                 WHERE s.incident_id = ?1 AND ps.gpu_engine IS NOT NULL)
             SELECT {PROCESS_COLUMNS}, u.samples, u.cpu_seconds, u.gpu_seconds,
                    u.peak_cpu_core, u.peak_cpu_machine, u.peak_gpu,
                    g.gpu_engine, g.adapter, u.peak_ws
             FROM usage u
             JOIN process p ON p.id = u.process_id
             LEFT JOIN gpu_peak g ON g.process_id = u.process_id AND g.rank = 1
             ORDER BY u.weight DESC, u.peak_cpu_core DESC, p.id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([incident_id], |r| {
            Ok(IncidentProcess {
                process: process_from(r, 0)?,
                samples: r.get(9)?,
                cpu_seconds: r.get(10)?,
                gpu_seconds: r.get(11)?,
                peak_cpu_core: r.get(12)?,
                peak_cpu_machine: r.get(13)?,
                peak_gpu: r.get(14)?,
                gpu_engine: r.get(15)?,
                gpu_adapter: r.get(16)?,
                peak_working_set_bytes: r.get(17)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The stored ancestors of a process, parent first.
    pub fn ancestors(&self, process_id: i64) -> Result<Vec<ProcessInfo>> {
        // UNION (not UNION ALL) stops on a cycle, which links should never form anyway.
        let sql = format!(
            "WITH RECURSIVE chain(id, depth) AS (
                 SELECT parent_process_id, 1 FROM process WHERE id = ?1
                 UNION
                 SELECT p.parent_process_id, c.depth + 1
                 FROM process p JOIN chain c ON p.id = c.id
                 WHERE c.depth < 64)
             SELECT {PROCESS_COLUMNS} FROM chain c JOIN process p ON p.id = c.id
             ORDER BY c.depth"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([process_id], |r| process_from(r, 0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All recorded processes, by id. Pruning keeps this bounded.
    pub fn processes(&self) -> Result<HashMap<i64, ProcessInfo>> {
        let sql = format!("SELECT {PROCESS_COLUMNS} FROM process p");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| process_from(r, 0))?;
        rows.map(|r| r.map(|p| (p.id, p)).map_err(Into::into))
            .collect()
    }

    /// Per-process, per-incident usage in samples taken at or after `since_utc`.
    pub fn process_usage_since(&self, since_utc: &str) -> Result<Vec<ProcessUsage>> {
        let sql = format!(
            "WITH {SAMPLE_SECONDS}
             SELECT ps.process_id, ss.incident_id, COUNT(*),
                    SUM(ps.cpu_pct_core * ss.seconds) / 100,
                    SUM(ps.gpu_pct * ss.seconds) / 100,
                    MAX(ps.cpu_pct_core), MAX(ps.gpu_pct), MAX(ss.ts_utc)
             FROM process_sample ps JOIN sample_seconds ss ON ss.id = ps.sample_id
             WHERE ss.ts_utc >= ?1
             GROUP BY ps.process_id, ss.incident_id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([since_utc], |r| {
            Ok(ProcessUsage {
                process_id: r.get(0)?,
                incident_id: r.get(1)?,
                samples: r.get(2)?,
                cpu_seconds: r.get(3)?,
                gpu_seconds: r.get(4)?,
                peak_cpu_core: r.get(5)?,
                peak_gpu: r.get(6)?,
                last_seen_utc: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes incidents that ended before `before_utc` (with their samples), then
    /// processes no longer referenced by any sample, directly or as an ancestor.
    /// Incidents still in progress are kept.
    pub fn prune(&self, before_utc: &str) -> Result<PruneStats> {
        self.in_transaction(|store| {
            let incidents = store.conn.execute(
                "DELETE FROM incident WHERE ended_utc IS NOT NULL AND ended_utc < ?1",
                params![before_utc],
            )?;
            let processes = store.conn.execute(
                "WITH RECURSIVE keep(id) AS (
                     SELECT process_id FROM process_sample
                     UNION
                     SELECT process_id FROM sample_hung_window
                     UNION
                     SELECT p.parent_process_id FROM process p JOIN keep k ON p.id = k.id
                     WHERE p.parent_process_id IS NOT NULL)
                 DELETE FROM process WHERE id NOT IN (SELECT id FROM keep)",
                [],
            )?;
            Ok(PruneStats {
                incidents,
                processes,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterInfo;
    use crate::gpu::Luid;
    use crate::incident::{Incident, Triggers};
    use crate::store::{ProcessRow, ProcessSampleRow, SampleRow};
    use crate::totals::Health;

    const T0: &str = "2026-01-01T00:00:00.0000000Z";
    const T1: &str = "2026-01-01T00:00:02.0000000Z";
    const T2: &str = "2026-01-01T00:00:04.0000000Z";
    const LATER: &str = "2026-02-01T00:00:00.0000000Z";

    fn process(store: &Store, pid: u32, name: &str, parent: Option<i64>) -> i64 {
        store
            .insert_process(&ProcessRow {
                pid,
                create_time_utc: T0,
                name,
                image_path: None,
                command_line: Some(name),
                user_name: None,
                session_id: None,
                parent_pid: None,
                parent_process_id: parent,
                info_error: None,
                seen_utc: T0,
            })
            .unwrap()
    }

    fn sample(store: &Store, incident: i64, ts: &str, interval_s: Option<f64>) -> i64 {
        let id = store
            .insert_sample(&SampleRow {
                incident_id: incident,
                ts_utc: ts,
                interval_s: interval_s.unwrap_or(0.0),
                cpu_total: 50.0,
                gpu_max_engine: 90.0,
                gpu_sum: 100.0,
                max_proc_core: 100.0,
                triggers: Triggers {
                    gpu: true,
                    ..Triggers::default()
                },
                stall_ms: 0.0,
                health: Health::default(),
                hung_windows: 0,
            })
            .unwrap();
        if interval_s.is_none() {
            // Simulate a row recorded before schema version 2.
            store
                .conn
                .execute("UPDATE sample SET interval_s = NULL WHERE id = ?1", [id])
                .unwrap();
        }
        id
    }

    fn usage(store: &Store, sample: i64, process: i64, cpu_core: f64, gpu: f64) {
        store
            .insert_process_sample(&ProcessSampleRow {
                sample_id: sample,
                process_id: process,
                cpu_pct_machine: cpu_core / 16.0,
                cpu_pct_core: cpu_core,
                gpu_pct: gpu,
                gpu_engine: (gpu > 0.0).then_some("3D"),
                gpu_adapter_luid: (gpu > 0.0).then_some("0x00000000_0x00000007"),
                working_set_bytes: Some(1000),
            })
            .unwrap();
    }

    fn end(store: &Store, id: i64, at: &str) {
        let incident = Incident {
            triggers: Triggers {
                gpu: true,
                ..Triggers::default()
            },
            samples: 2,
            peak_cpu: 50.0,
            peak_gpu: 90.0,
            peak_proc_core: 100.0,
            ..Incident::default()
        };
        store.update_incident(id, &incident, Some(at)).unwrap();
    }

    /// explorer <- app <- (worker, renderer); one incident with two samples.
    struct Fixture {
        store: Store,
        incident: i64,
        explorer: i64,
        app: i64,
        worker: i64,
        renderer: i64,
    }

    fn fixture() -> Fixture {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_adapters(&[AdapterInfo {
                luid: Luid { high: 0, low: 7 },
                description: "Test GPU".into(),
            }])
            .unwrap();
        let explorer = process(&store, 10, "explorer.exe", None);
        let app = process(&store, 20, "app.exe", Some(explorer));
        let worker = process(&store, 30, "worker.exe", Some(app));
        let renderer = process(&store, 40, "renderer.exe", Some(app));
        let incident = store.insert_incident(T0).unwrap();
        let s1 = sample(&store, incident, T1, Some(2.0));
        let s2 = sample(&store, incident, T2, Some(2.0));
        usage(&store, s1, worker, 100.0, 0.0);
        usage(&store, s2, worker, 50.0, 0.0);
        usage(&store, s1, renderer, 10.0, 90.0);
        usage(&store, s2, renderer, 10.0, 80.0);
        end(&store, incident, T2);
        Fixture {
            store,
            incident,
            explorer,
            app,
            worker,
            renderer,
        }
    }

    #[test]
    fn lists_incidents_with_top_process() {
        let f = fixture();
        let incidents = f.store.incidents_since(T0).unwrap();
        assert_eq!(incidents.len(), 1);
        let i = &incidents[0];
        assert_eq!(i.id, f.incident);
        assert_eq!(i.ended_utc.as_deref(), Some(T2));
        assert_eq!(i.triggers, "gpu");
        // renderer: max(10, 90) + max(10, 80) = 170 beats worker: 100 + 50 = 150.
        assert_eq!(i.top_process.as_deref(), Some("renderer.exe"));
        assert!(f.store.incidents_since(LATER).unwrap().is_empty());
        assert_eq!(f.store.incident(f.incident).unwrap().as_ref(), Some(i));
        assert_eq!(f.store.incident(999).unwrap(), None);
    }

    #[test]
    fn summarises_incident_processes() {
        let f = fixture();
        let processes = f.store.incident_processes(f.incident).unwrap();
        let names: Vec<_> = processes.iter().map(|p| p.process.name.as_str()).collect();
        assert_eq!(names, ["renderer.exe", "worker.exe"]);

        let renderer = &processes[0];
        assert_eq!(renderer.samples, 2);
        assert!((renderer.gpu_seconds - 3.4).abs() < 1e-9); // (90 + 80)% x 2 s
        assert!((renderer.cpu_seconds - 0.4).abs() < 1e-9);
        assert_eq!(renderer.peak_gpu, 90.0);
        assert_eq!(renderer.gpu_engine.as_deref(), Some("3D"));
        assert_eq!(renderer.gpu_adapter.as_deref(), Some("Test GPU"));

        let worker = &processes[1];
        assert!((worker.cpu_seconds - 3.0).abs() < 1e-9); // (100 + 50)% x 2 s
        assert_eq!(worker.peak_cpu_core, 100.0);
        assert_eq!(worker.gpu_engine, None);
    }

    #[test]
    fn lists_samples_and_falls_back_to_gaps_without_interval() {
        let f = fixture();
        let samples = f.store.incident_samples(f.incident).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].seconds, 2.0);

        let incident = f.store.insert_incident(LATER).unwrap();
        sample(&f.store, incident, "2026-02-01T00:00:01.0000000Z", None);
        sample(&f.store, incident, "2026-02-01T00:00:04.5000000Z", None);
        let seconds: Vec<f64> = f
            .store
            .incident_samples(incident)
            .unwrap()
            .iter()
            .map(|s| s.seconds)
            .collect();
        assert_eq!(seconds[0], 0.0);
        assert!((seconds[1] - 3.5).abs() < 0.01, "{seconds:?}");
    }

    #[test]
    fn walks_stored_ancestry() {
        let f = fixture();
        let chain: Vec<_> = f
            .store
            .ancestors(f.worker)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(chain, [f.app, f.explorer]);
        assert!(f.store.ancestors(f.explorer).unwrap().is_empty());
    }

    #[test]
    fn reports_usage_per_process_and_incident() {
        let f = fixture();
        let mut usage = f.store.process_usage_since(T0).unwrap();
        usage.sort_by_key(|u| u.process_id);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].process_id, f.worker);
        assert_eq!(usage[0].incident_id, f.incident);
        assert_eq!(usage[0].last_seen_utc, T2);
        // Only the second sample is at or after T2.
        let recent = f.store.process_usage_since(T2).unwrap();
        assert!(recent.iter().all(|u| u.samples == 1));
        assert_eq!(f.store.processes().unwrap().len(), 4);
    }

    fn first_sample(store: &Store, incident: i64) -> i64 {
        store
            .conn
            .query_row(
                "SELECT MIN(id) FROM sample WHERE incident_id = ?1",
                [incident],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn lists_hung_windows_and_names_the_hung_process() {
        let f = fixture();
        let frozen = process(&f.store, 60, "frozen.exe", Some(f.explorer));
        let s1 = first_sample(&f.store, f.incident);
        f.store
            .insert_hung_window(s1, frozen, "Meeting", false)
            .unwrap();
        // The same window again in this sample, now in the foreground: merged.
        f.store
            .insert_hung_window(s1, frozen, "Meeting", true)
            .unwrap();
        f.store
            .insert_hung_window(s1 + 1, frozen, "Meeting", false)
            .unwrap();

        let hung = f.store.incident_hung_windows(f.incident).unwrap();
        assert_eq!(hung.len(), 1);
        assert_eq!(hung[0].process.name, "frozen.exe");
        assert_eq!((hung[0].samples, hung[0].foreground), (2, true));
        assert_eq!(
            (hung[0].first_utc.as_str(), hung[0].last_utc.as_str()),
            (T1, T2)
        );

        let incident = f.store.incident(f.incident).unwrap().unwrap();
        assert_eq!(incident.hung_process.as_deref(), Some("frozen.exe"));
        // Peaks written by update_incident (schema v3).
        assert_eq!(incident.peak_stall_ms, Some(0.0));

        // A process seen only as a hung window survives pruning while its incident does.
        let open = f.store.insert_incident(LATER).unwrap();
        let s = sample(&f.store, open, LATER, Some(2.0));
        f.store
            .insert_hung_window(s, frozen, "Meeting", false)
            .unwrap();
        f.store.prune("2026-01-15T00:00:00.0000000Z").unwrap();
        assert!(f.store.processes().unwrap().contains_key(&frozen));
    }

    #[test]
    fn reads_responsiveness_columns_of_samples() {
        let f = fixture();
        let samples = f.store.incident_samples(f.incident).unwrap();
        assert_eq!(samples[0].stall_ms, Some(0.0));
        assert_eq!(samples[0].hung_windows, Some(0));
        // Health::default() has no memory reading.
        assert_eq!(samples[0].memory_available_mb, None);
    }

    #[test]
    fn prunes_old_incidents_and_unreferenced_processes() {
        let f = fixture();
        // An unrelated process that was never an offender or ancestor.
        process(&f.store, 50, "stray.exe", None);
        // A newer incident, still open, referencing only the worker.
        let open = f.store.insert_incident(LATER).unwrap();
        let s = sample(&f.store, open, LATER, Some(2.0));
        usage(&f.store, s, f.worker, 100.0, 0.0);

        let stats = f.store.prune("2026-01-15T00:00:00.0000000Z").unwrap();
        assert_eq!(
            stats,
            PruneStats {
                incidents: 1,
                processes: 2, // renderer and stray; the worker's ancestors stay
            }
        );
        let remaining = f.store.processes().unwrap();
        assert!(remaining.contains_key(&f.worker));
        assert!(remaining.contains_key(&f.app));
        assert!(remaining.contains_key(&f.explorer));
        assert!(!remaining.contains_key(&f.renderer));
        assert_eq!(f.store.incidents_since(T0).unwrap().len(), 1);

        // The open incident survives any cutoff.
        assert_eq!(f.store.prune("9999").unwrap().incidents, 0);
    }
}
