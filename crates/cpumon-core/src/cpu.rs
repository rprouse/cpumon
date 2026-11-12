//! Per-process CPU usage from the difference between consecutive snapshots.

use std::collections::HashMap;

use windows::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

use crate::snapshot::{ProcessKey, ProcessSnapshot};

/// CPU usage of one process over the last interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessCpu {
    pub key: ProcessKey,
    /// Percent of one logical CPU (100 = one core fully busy; can exceed 100).
    pub pct_core: f64,
    /// Percent of the whole machine, as Task Manager shows it (0–100).
    pub pct_machine: f64,
}

/// Number of logical CPUs across all processor groups.
pub fn logical_cpu_count() -> u32 {
    // SAFETY: no pointer arguments; returns 0 only on failure.
    let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    count.max(1)
}

/// Tracks per-process CPU time between snapshots. Maps are reused between updates.
#[derive(Debug)]
pub struct CpuTracker {
    logical_cpus: u32,
    previous: HashMap<ProcessKey, i64>,
    current: HashMap<ProcessKey, i64>,
    previous_time: Option<i64>,
    usage: Vec<ProcessCpu>,
}

impl CpuTracker {
    pub fn new(logical_cpus: u32) -> Self {
        Self {
            logical_cpus: logical_cpus.max(1),
            previous: HashMap::new(),
            current: HashMap::new(),
            previous_time: None,
            usage: Vec::new(),
        }
    }

    /// Updates from a snapshot and returns usage since the previous one, busiest
    /// first. The first call only records a baseline and returns nothing.
    pub fn update_from(&mut self, snapshot: &ProcessSnapshot) -> &[ProcessCpu] {
        self.update(
            snapshot.system_time(),
            snapshot.entries().iter().map(|e| (e.key(), e.cpu_time())),
        )
    }

    /// Updates from `(process, total CPU time)` pairs taken at `now` (a FILETIME;
    /// CPU times in 100 ns units). The System Idle Process (PID 0) is ignored.
    pub fn update(
        &mut self,
        now: i64,
        processes: impl IntoIterator<Item = (ProcessKey, i64)>,
    ) -> &[ProcessCpu] {
        self.current.clear();
        self.current
            .extend(processes.into_iter().filter(|(k, _)| k.pid != 0));
        self.usage.clear();

        if let Some(previous_time) = self.previous_time {
            let elapsed = now - previous_time;
            // A clock step backwards gives no usable interval; just re-baseline.
            if elapsed > 0 {
                for (&key, &cpu_time) in &self.current {
                    let before = match self.previous.get(&key) {
                        Some(&before) => before,
                        // Started during the interval: all its CPU time is new.
                        None if key.create_time >= previous_time => 0,
                        None => continue,
                    };
                    let delta = (cpu_time - before).max(0);
                    if delta == 0 {
                        continue;
                    }
                    let pct_core = delta as f64 / elapsed as f64 * 100.0;
                    self.usage.push(ProcessCpu {
                        key,
                        pct_core,
                        pct_machine: pct_core / f64::from(self.logical_cpus),
                    });
                }
                self.usage.sort_by(|a, b| {
                    b.pct_core
                        .total_cmp(&a.pct_core)
                        .then(a.key.pid.cmp(&b.key.pid))
                });
            }
        }

        std::mem::swap(&mut self.previous, &mut self.current);
        self.previous_time = Some(now);
        &self.usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second in FILETIME units.
    const SEC: i64 = 10_000_000;
    const T0: i64 = 1_000 * SEC;

    fn key(pid: u32, create_time: i64) -> ProcessKey {
        ProcessKey { pid, create_time }
    }

    #[test]
    fn first_update_is_a_baseline() {
        let mut tracker = CpuTracker::new(4);
        assert!(tracker.update(T0, [(key(10, 1), 5 * SEC)]).is_empty());
    }

    #[test]
    fn computes_core_and_machine_percent() {
        let mut tracker = CpuTracker::new(4);
        tracker.update(T0, [(key(10, 1), 5 * SEC), (key(20, 1), 0)]);
        // Over 2 s: pid 10 used 3 s of CPU (150% of a core), pid 20 used 0.5 s.
        let usage = tracker
            .update(T0 + 2 * SEC, [(key(10, 1), 8 * SEC), (key(20, 1), SEC / 2)])
            .to_vec();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].key.pid, 10);
        assert_eq!(usage[0].pct_core, 150.0);
        assert_eq!(usage[0].pct_machine, 37.5);
        assert_eq!(usage[1].key.pid, 20);
        assert_eq!(usage[1].pct_core, 25.0);
    }

    #[test]
    fn omits_idle_processes() {
        let mut tracker = CpuTracker::new(1);
        tracker.update(T0, [(key(10, 1), SEC)]);
        assert!(tracker.update(T0 + SEC, [(key(10, 1), SEC)]).is_empty());
    }

    #[test]
    fn counts_all_time_of_process_started_during_interval() {
        let mut tracker = CpuTracker::new(1);
        tracker.update(T0, []);
        let usage = tracker.update(T0 + SEC, [(key(30, T0 + SEC / 2), SEC / 4)]);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].pct_core, 25.0);
    }

    #[test]
    fn reused_pid_is_not_diffed_against_old_process() {
        let mut tracker = CpuTracker::new(1);
        // Old process 40 had 100 s of CPU time.
        tracker.update(T0, [(key(40, 1), 100 * SEC)]);
        // A new process reused PID 40 during the interval and used 0.5 s.
        let usage = tracker.update(T0 + SEC, [(key(40, T0 + 1), SEC / 2)]);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].key.create_time, T0 + 1);
        assert_eq!(usage[0].pct_core, 50.0);
    }

    #[test]
    fn ignores_unseen_process_that_predates_previous_snapshot() {
        let mut tracker = CpuTracker::new(1);
        tracker.update(T0, []);
        assert!(
            tracker
                .update(T0 + SEC, [(key(50, 1), 10 * SEC)])
                .is_empty()
        );
    }

    #[test]
    fn ignores_system_idle_process() {
        let mut tracker = CpuTracker::new(1);
        tracker.update(T0, [(key(0, 0), 0)]);
        assert!(tracker.update(T0 + SEC, [(key(0, 0), SEC)]).is_empty());
    }

    #[test]
    fn clock_going_backwards_rebaselines() {
        let mut tracker = CpuTracker::new(1);
        tracker.update(T0, [(key(10, 1), 0)]);
        assert!(tracker.update(T0 - SEC, [(key(10, 1), SEC)]).is_empty());
        // The next interval is measured from the new baseline.
        let usage = tracker.update(T0, [(key(10, 1), SEC + SEC / 2)]);
        assert_eq!(usage[0].pct_core, 50.0);
    }

    #[test]
    fn logical_cpu_count_is_positive() {
        assert!(logical_cpu_count() >= 1);
    }
}
