//! Choosing which processes to record during an incident.

use std::collections::HashMap;

use crate::cpu::ProcessCpu;
use crate::gpu::ProcessGpu;
use crate::snapshot::ProcessKey;

/// Which processes count as offenders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OffenderLimits {
    /// Percent of the machine's CPU.
    pub proc_cpu: f64,
    /// Percent of one core.
    pub proc_core: f64,
    /// Percent of the process's busiest GPU engine.
    pub proc_gpu: f64,
    /// Always include at least this many of the top CPU and top GPU users.
    pub top: usize,
}

/// One process's usage in a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offender {
    pub key: ProcessKey,
    /// Percent of the whole machine.
    pub cpu_machine: f64,
    /// Percent of one core.
    pub cpu_core: f64,
    /// GPU usage (busiest engine), if any.
    pub gpu: Option<ProcessGpu>,
}

impl Offender {
    /// How saturated the process is: the larger of its share of one core (capped at
    /// 100) and its busiest GPU engine.
    pub fn severity(&self) -> f64 {
        let gpu = self.gpu.map_or(0.0, |g| g.utilization);
        self.cpu_core.min(100.0).max(gpu)
    }
}

/// Selects the processes to record: everyone over a limit, plus the top `limits.top`
/// CPU and GPU users. `cpu` and `gpu` must be sorted busiest first (as the sampler
/// returns them). `key_for_pid` maps GPU PIDs to process identities; GPU users that
/// have already exited are skipped. Returns offenders sorted by severity.
pub fn select_offenders(
    cpu: &[ProcessCpu],
    gpu: &[ProcessGpu],
    limits: &OffenderLimits,
    key_for_pid: impl Fn(u32) -> Option<ProcessKey>,
) -> Vec<Offender> {
    let mut by_pid: HashMap<u32, Offender> = HashMap::new();

    for (rank, p) in cpu.iter().enumerate() {
        let selected =
            rank < limits.top || p.pct_machine >= limits.proc_cpu || p.pct_core >= limits.proc_core;
        if selected {
            by_pid.insert(
                p.key.pid,
                Offender {
                    key: p.key,
                    cpu_machine: p.pct_machine,
                    cpu_core: p.pct_core,
                    gpu: None,
                },
            );
        }
    }

    for (rank, g) in gpu.iter().enumerate() {
        if g.utilization <= 0.0 {
            continue;
        }
        let selected = rank < limits.top || g.utilization >= limits.proc_gpu;
        if let Some(existing) = by_pid.get_mut(&g.pid) {
            existing.gpu = Some(*g);
        } else if selected && let Some(key) = key_for_pid(g.pid) {
            // Pick up its CPU usage too, if it had any.
            let cpu = cpu.iter().find(|p| p.key == key);
            by_pid.insert(
                g.pid,
                Offender {
                    key,
                    cpu_machine: cpu.map_or(0.0, |p| p.pct_machine),
                    cpu_core: cpu.map_or(0.0, |p| p.pct_core),
                    gpu: Some(*g),
                },
            );
        }
    }

    let mut offenders: Vec<Offender> = by_pid.into_values().collect();
    offenders.sort_by(|a, b| {
        b.severity()
            .total_cmp(&a.severity())
            // Several processes can saturate a core; rank the heavier one first.
            .then(b.cpu_core.total_cmp(&a.cpu_core))
            .then(a.key.pid.cmp(&b.key.pid))
    });
    offenders
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{EngineTypes, Luid};

    const LIMITS: OffenderLimits = OffenderLimits {
        proc_cpu: 10.0,
        proc_core: 90.0,
        proc_gpu: 10.0,
        top: 1,
    };

    fn key(pid: u32) -> ProcessKey {
        ProcessKey {
            pid,
            create_time: i64::from(pid) * 100,
        }
    }

    fn cpu(pid: u32, pct_core: f64) -> ProcessCpu {
        ProcessCpu {
            key: key(pid),
            pct_core,
            pct_machine: pct_core / 16.0,
        }
    }

    fn gpu(pid: u32, utilization: f64) -> ProcessGpu {
        let mut types = EngineTypes::default();
        ProcessGpu {
            pid,
            adapter: Luid { high: 0, low: 1 },
            engine_type: types.intern("3D"),
            utilization,
        }
    }

    fn pids(offenders: &[Offender]) -> Vec<u32> {
        offenders.iter().map(|o| o.key.pid).collect()
    }

    #[test]
    fn selects_over_limit_and_top_users() {
        let offenders = select_offenders(
            // 1: top CPU user; 2: pegged single core (6% of machine); 3: below limits.
            &[cpu(1, 400.0), cpu(2, 99.0), cpu(3, 50.0)],
            // 4: top GPU user; 5: over GPU limit; 6: below limit.
            &[gpu(4, 90.0), gpu(5, 20.0), gpu(6, 5.0)],
            &LIMITS,
            |pid| Some(key(pid)),
        );
        assert_eq!(pids(&offenders), [1, 2, 4, 5]);
    }

    #[test]
    fn merges_cpu_and_gpu_for_the_same_process() {
        let offenders = select_offenders(
            &[cpu(1, 200.0), cpu(2, 30.0)],
            &[gpu(1, 50.0), gpu(2, 3.0)],
            &OffenderLimits { top: 1, ..LIMITS },
            |pid| Some(key(pid)),
        );
        assert_eq!(pids(&offenders), [1]);
        assert_eq!(offenders[0].gpu.map(|g| g.utilization), Some(50.0));
        assert_eq!(offenders[0].cpu_core, 200.0);
    }

    #[test]
    fn gpu_only_offender_picks_up_its_cpu_usage() {
        let offenders = select_offenders(
            &[cpu(1, 300.0), cpu(2, 5.0)],
            &[gpu(2, 95.0)],
            &OffenderLimits { top: 1, ..LIMITS },
            |pid| Some(key(pid)),
        );
        let two = offenders.iter().find(|o| o.key.pid == 2).unwrap();
        assert_eq!(two.cpu_core, 5.0);
    }

    #[test]
    fn skips_gpu_users_that_exited() {
        let offenders = select_offenders(&[], &[gpu(9, 99.0)], &LIMITS, |_| None);
        assert!(offenders.is_empty());
    }

    #[test]
    fn sorts_by_severity() {
        let offenders = select_offenders(
            &[cpu(1, 40.0), cpu(2, 99.0)],
            &[gpu(3, 70.0)],
            &OffenderLimits { top: 5, ..LIMITS },
            |pid| Some(key(pid)),
        );
        assert_eq!(pids(&offenders), [2, 3, 1]);
    }

    #[test]
    fn multi_core_user_outranks_single_core_user() {
        let offenders = select_offenders(
            // Both saturate at least one core; pid 2 uses ten.
            &[cpu(2, 1000.0), cpu(1, 140.0)],
            &[],
            &OffenderLimits { top: 5, ..LIMITS },
            |pid| Some(key(pid)),
        );
        assert_eq!(pids(&offenders), [2, 1]);
    }
}
