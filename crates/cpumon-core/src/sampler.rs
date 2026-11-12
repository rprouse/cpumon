//! Combined CPU and GPU sampling: one call per interval gives machine totals and
//! per-process usage.

use crate::cpu::{CpuTracker, ProcessCpu, logical_cpu_count};
use crate::error::Result;
use crate::gpu::{EngineTypes, GpuSampler, GpuSummary, summarize};
use crate::snapshot::ProcessSnapshot;
use crate::totals::MachineCpu;

/// Everything measured in one interval.
#[derive(Debug)]
pub struct Sample<'a> {
    /// Seconds covered by this sample (since the previous one).
    pub seconds: f64,
    /// Overall CPU utilisation in percent (Task Manager's figure; may exceed 100).
    pub cpu_total: f64,
    /// Processes that used CPU, busiest first.
    pub cpu: &'a [ProcessCpu],
    /// GPU usage per adapter, engine and process.
    pub gpu: &'a GpuSummary,
    /// Names for the engine type ids in `gpu`.
    pub engine_types: &'a EngineTypes,
    /// The processes at the end of the interval, for names and ancestry.
    pub snapshot: &'a ProcessSnapshot,
}

/// Samples CPU and GPU usage. Create once, then call [`sample`](Self::sample) every
/// interval; each sample covers the time since the previous one. State and buffers
/// are reused between samples.
pub struct Sampler {
    machine_cpu: MachineCpu,
    gpu: GpuSampler,
    snapshot: ProcessSnapshot,
    cpu: CpuTracker,
    gpu_summary: GpuSummary,
    logical_cpus: u32,
    previous_time: i64,
}

impl Sampler {
    /// Opens the counters and records the baseline.
    pub fn new() -> Result<Self> {
        let logical_cpus = logical_cpu_count();
        let machine_cpu = MachineCpu::new()?;
        let gpu = GpuSampler::new()?;
        let snapshot = ProcessSnapshot::new()?;
        let mut cpu = CpuTracker::new(logical_cpus);
        cpu.update_from(&snapshot);
        let previous_time = snapshot.system_time();
        Ok(Self {
            machine_cpu,
            gpu,
            snapshot,
            cpu,
            gpu_summary: GpuSummary::default(),
            logical_cpus,
            previous_time,
        })
    }

    /// Number of logical CPUs used for "% of machine".
    pub fn logical_cpus(&self) -> u32 {
        self.logical_cpus
    }

    /// Measures usage since the previous sample.
    pub fn sample(&mut self) -> Result<Sample<'_>> {
        // Take the readings back to back so they cover the same interval.
        let cpu_total = self.machine_cpu.sample()?;
        let gpu = self.gpu.sample()?;
        self.gpu_summary = summarize(gpu.usages);
        self.snapshot.refresh()?;
        let cpu = self.cpu.update_from(&self.snapshot);
        let now = self.snapshot.system_time();
        let seconds = (now - self.previous_time).max(0) as f64 / 10_000_000.0;
        self.previous_time = now;
        Ok(Sample {
            seconds,
            cpu_total,
            cpu,
            gpu: &self.gpu_summary,
            engine_types: self.gpu.engine_types(),
            snapshot: &self.snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_live_system() {
        let mut sampler = Sampler::new().expect("create sampler");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let sample = sampler.sample().expect("sample");
        assert!(
            sample.seconds > 0.1 && sample.seconds < 60.0,
            "{}",
            sample.seconds
        );
        assert!(sample.cpu_total >= 0.0);
        assert!(sample.cpu.iter().all(|p| p.pct_core >= 0.0));
        assert!(sample.snapshot.find(std::process::id()).is_some());
    }
}
