//! Writes incidents, samples and offenders (with their ancestry) to the store.

use std::collections::HashMap;

use crate::error::Result;
use crate::incident::{Incident, Observation, Triggers};
use crate::offenders::Offender;
use crate::procinfo::{ProcessDetails, ProcessInspector};
use crate::sampler::Sample;
use crate::snapshot::{ProcessKey, ProcessSnapshot};
use crate::store::{EngineRow, ProcessRow, ProcessSampleRow, SampleRow, Store};
use crate::time::format_utc;

/// GPU engines below this utilisation are not stored.
const MIN_ENGINE_PCT: f64 = 0.1;

/// An offender seen for the first time, with what was learned about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProcess {
    pub key: ProcessKey,
    pub details: ProcessDetails,
}

/// Records incidents to a [`Store`]. Process details are read once per process and
/// row ids are cached, so repeat offenders cost one row per sample.
pub struct Recorder {
    store: Store,
    inspector: ProcessInspector,
    process_ids: HashMap<ProcessKey, i64>,
    incident_id: Option<i64>,
}

/// Borrowed state used while writing one sample.
struct Writer<'a> {
    store: &'a Store,
    inspector: &'a mut ProcessInspector,
    cached: &'a HashMap<ProcessKey, i64>,
    snapshot: &'a ProcessSnapshot,
    seen_utc: &'a str,
    /// Ids looked up or inserted in this transaction; cached once it commits.
    resolved: Vec<(ProcessKey, i64)>,
    new_offenders: Vec<NewProcess>,
}

impl Recorder {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            inspector: ProcessInspector::new(),
            process_ids: HashMap::new(),
            incident_id: None,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Id of the incident being recorded, if any.
    pub fn incident_id(&self) -> Option<i64> {
        self.incident_id
    }

    /// Records one sample of the current incident, creating the incident row on the
    /// first call. Returns offenders recorded for the first time.
    pub fn record(
        &mut self,
        incident: &Incident,
        observation: &Observation,
        triggers: Triggers,
        sample: &Sample<'_>,
        offenders: &[Offender],
    ) -> Result<Vec<NewProcess>> {
        let ts = format_utc(sample.snapshot.system_time());
        let mut writer = Writer {
            store: &self.store,
            inspector: &mut self.inspector,
            cached: &self.process_ids,
            snapshot: sample.snapshot,
            seen_utc: &ts,
            resolved: Vec::new(),
            new_offenders: Vec::new(),
        };
        let existing_incident = self.incident_id;

        let incident_id = self.store.in_transaction(|store| {
            let incident_id = match existing_incident {
                Some(id) => id,
                None => store.insert_incident(&ts)?,
            };
            let sample_id = store.insert_sample(&SampleRow {
                incident_id,
                ts_utc: &ts,
                interval_s: sample.seconds,
                cpu_total: observation.cpu,
                gpu_max_engine: observation.gpu,
                gpu_sum: sample.gpu.sum(),
                max_proc_core: observation.max_proc_core,
                triggers,
                stall_ms: observation.stall_ms,
                health: sample.health,
                hung_windows: sample.hung.len(),
            })?;
            for adapter in &sample.gpu.adapters {
                let luid = adapter.adapter.to_string();
                for e in adapter
                    .engines
                    .iter()
                    .filter(|e| e.utilization >= MIN_ENGINE_PCT)
                {
                    store.insert_engine(&EngineRow {
                        sample_id,
                        adapter_luid: &luid,
                        phys: e.phys,
                        eng: e.eng,
                        engine_type: sample.engine_types.name(e.engine_type),
                        utilization: e.utilization,
                    })?;
                }
            }
            for offender in offenders {
                let process_id = writer.offender(offender.key)?;
                let luid = offender.gpu.map(|g| g.adapter.to_string());
                store.insert_process_sample(&ProcessSampleRow {
                    sample_id,
                    process_id,
                    cpu_pct_machine: offender.cpu_machine,
                    cpu_pct_core: offender.cpu_core,
                    gpu_pct: offender.gpu.map_or(0.0, |g| g.utilization),
                    gpu_engine: offender
                        .gpu
                        .map(|g| sample.engine_types.name(g.engine_type)),
                    gpu_adapter_luid: luid.as_deref(),
                    working_set_bytes: sample
                        .snapshot
                        .find(offender.key.pid)
                        .filter(|e| e.key() == offender.key)
                        .map(|e| e.working_set_bytes as u64),
                })?;
            }
            for window in sample.hung {
                // A process that exited since the scan cannot be identified reliably.
                let Some(entry) = sample.snapshot.find(window.pid) else {
                    continue;
                };
                let process_id = writer.offender(entry.key())?;
                store.insert_hung_window(
                    sample_id,
                    process_id,
                    &window.title,
                    window.foreground,
                )?;
            }
            store.update_incident(incident_id, incident, None)?;
            Ok(incident_id)
        })?;

        let Writer {
            resolved,
            new_offenders,
            ..
        } = writer;
        self.process_ids.extend(resolved);
        self.incident_id = Some(incident_id);
        Ok(new_offenders)
    }

    /// Marks the current incident as ended at `ended` (a FILETIME).
    pub fn end(&mut self, incident: &Incident, ended: i64) -> Result<()> {
        if let Some(id) = self.incident_id.take() {
            self.store
                .update_incident(id, incident, Some(&format_utc(ended)))?;
        }
        Ok(())
    }

    /// Drops cached ids of processes that have exited, bounding memory on long runs.
    pub fn forget_exited(&mut self, snapshot: &ProcessSnapshot) {
        self.process_ids
            .retain(|key, _| snapshot.find(key.pid).is_some_and(|e| e.key() == *key));
    }
}

impl Writer<'_> {
    /// Row id for an offender, inserting it and its ancestors if new.
    fn offender(&mut self, key: ProcessKey) -> Result<i64> {
        let id = match self.lookup(key)? {
            Some(id) => id,
            None => {
                let mut parent_id = None;
                for ancestor in self.snapshot.ancestors(key).iter().rev() {
                    parent_id = Some(match self.lookup(*ancestor)? {
                        Some(id) => id,
                        None => self.insert(*ancestor, parent_id)?.0,
                    });
                }
                let (id, details) = self.insert(key, parent_id)?;
                self.new_offenders.push(NewProcess { key, details });
                id
            }
        };
        self.store.touch_process(id, self.seen_utc)?;
        Ok(id)
    }

    fn lookup(&mut self, key: ProcessKey) -> Result<Option<i64>> {
        if let Some(&id) = self.cached.get(&key) {
            return Ok(Some(id));
        }
        let found = self
            .store
            .find_process(key.pid, &format_utc(key.create_time))?;
        if let Some(id) = found {
            self.resolved.push((key, id));
        }
        Ok(found)
    }

    fn insert(&mut self, key: ProcessKey, parent_id: Option<i64>) -> Result<(i64, ProcessDetails)> {
        let entry = self.snapshot.find(key.pid).filter(|e| e.key() == key);
        let details = self.inspector.inspect(key);
        let id = self.store.insert_process(&ProcessRow {
            pid: key.pid,
            create_time_utc: &format_utc(key.create_time),
            name: entry.map_or("<exited>", |e| self.snapshot.name(e)),
            image_path: details.image_path.as_deref(),
            command_line: details.command_line.as_deref(),
            user_name: details.user.as_deref(),
            session_id: entry.map(|e| e.session_id),
            parent_pid: entry.map(|e| e.parent_pid),
            parent_process_id: parent_id,
            info_error: details.error.as_deref(),
            seen_utc: self.seen_utc,
        })?;
        self.resolved.push((key, id));
        Ok((id, details))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::Sampler;
    use crate::time::now_filetime;

    fn count(store: &Store, sql: &str) -> i64 {
        store.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn records_live_offender_with_ancestry() {
        let mut sampler = Sampler::new().unwrap();
        let mut recorder = Recorder::new(Store::open_in_memory().unwrap());
        let incident = Incident {
            samples: 1,
            ..Incident::default()
        };
        let observation = Observation::default();

        for round in 0..2 {
            let sample = sampler.sample().unwrap();
            let me = sample.snapshot.find(std::process::id()).unwrap().key();
            let offender = Offender {
                key: me,
                cpu_machine: 6.0,
                cpu_core: 100.0,
                gpu: None,
            };
            let new = recorder
                .record(
                    &incident,
                    &observation,
                    Triggers::default(),
                    &sample,
                    &[offender],
                )
                .unwrap();
            if round == 0 {
                assert_eq!(new.len(), 1);
                assert_eq!(new[0].key, me);
                assert!(new[0].details.command_line.is_some());
            } else {
                assert!(new.is_empty(), "already recorded");
            }
        }

        let store = recorder.store();
        assert_eq!(count(store, "SELECT COUNT(*) FROM incident"), 1);
        assert_eq!(count(store, "SELECT COUNT(*) FROM sample"), 2);
        assert_eq!(count(store, "SELECT COUNT(*) FROM process_sample"), 2);
        // The test process plus at least its parent, linked together.
        assert!(count(store, "SELECT COUNT(*) FROM process") >= 2);
        let me = std::process::id();
        assert_eq!(
            count(
                store,
                &format!(
                    "SELECT COUNT(*) FROM process c JOIN process p ON p.id = c.parent_process_id
                     WHERE c.pid = {me} AND c.command_line IS NOT NULL"
                ),
            ),
            1
        );

        recorder.end(&incident, now_filetime()).unwrap();
        assert_eq!(recorder.incident_id(), None);
        assert_eq!(
            count(
                recorder.store(),
                "SELECT COUNT(*) FROM incident WHERE ended_utc IS NOT NULL"
            ),
            1
        );
    }
}
