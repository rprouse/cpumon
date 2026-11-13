//! Machine-wide CPU usage and system health signals, read from one PDH query.

use crate::error::Result;
use crate::pdh::CounterArray;

/// Counters in the query, in order. Only the first is required.
const COUNTERS: &[(&str, bool)] = &[
    // Task Manager's overall CPU figure (Windows 8+). Accounts for frequency scaling,
    // so it can exceed 100 when turbo boosting.
    (r"\Processor Information(_Total)\% Processor Utility", true),
    (r"\Processor Information(*)\% DPC Time", false),
    (r"\Processor Information(*)\% Interrupt Time", false),
    // Current speed relative to base frequency: well under 100 while busy suggests
    // thermal or power throttling.
    (
        r"\Processor Information(_Total)\% Processor Performance",
        false,
    ),
    // Kelvin. ACPI thermal zones; often coarse, and missing on some machines.
    (r"\Thermal Zone Information(*)\Temperature", false),
    (r"\Memory\Available MBytes", false),
    (r"\Memory\% Committed Bytes In Use", false),
    // Pages read from disk to resolve hard page faults.
    (r"\Memory\Pages Input/sec", false),
];
const UTILITY: usize = 0;
const DPC: usize = 1;
const INTERRUPT: usize = 2;
const PERFORMANCE: usize = 3;
const TEMPERATURE: usize = 4;
const AVAILABLE_MB: usize = 5;
const COMMITTED: usize = 6;
const PAGES_INPUT: usize = 7;

/// System health over one interval. `None` where the counter is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Health {
    /// Time spent in deferred procedure calls, percent of the whole machine.
    pub dpc: f64,
    /// Time spent servicing hardware interrupts, percent of the whole machine.
    pub interrupt: f64,
    /// The worst single logical CPU's DPC + interrupt time, percent of that CPU. A
    /// misbehaving driver usually shows here first.
    pub max_core_dpc: f64,
    /// CPU speed as percent of base frequency.
    pub cpu_performance: Option<f64>,
    /// Hottest thermal zone, degrees Celsius.
    pub temperature_c: Option<f64>,
    pub memory_available_mb: Option<f64>,
    /// Committed memory as percent of the commit limit.
    pub memory_committed: Option<f64>,
    /// Pages read from disk per second to resolve hard faults.
    pub pages_input_per_sec: Option<f64>,
}

/// Samples overall CPU utilisation and health. Keep one alive and call
/// [`sample`](Self::sample) periodically.
pub struct MachineCpu {
    counters: CounterArray,
    /// Per logical CPU (instance name, DPC + interrupt %), reused between samples.
    per_cpu: Vec<(String, f64)>,
}

impl MachineCpu {
    /// Opens the counters and takes the baseline reading.
    pub fn new() -> Result<Self> {
        Ok(Self {
            counters: CounterArray::open_many(COUNTERS)?,
            per_cpu: Vec::new(),
        })
    }

    /// CPU utilisation since the previous call, in percent (not capped at 100), and
    /// health over the same interval.
    pub fn sample(&mut self) -> Result<(f64, Health)> {
        self.counters.collect_all()?;
        let mut total = 0.0;
        self.counters.read(UTILITY, |_, v| total = v)?;

        let mut health = Health::default();
        for (_, value) in &mut self.per_cpu {
            *value = 0.0;
        }
        for (index, field) in [(DPC, 0), (INTERRUPT, 1)] {
            let per_cpu = &mut self.per_cpu;
            self.counters.read(index, |name, value| {
                if is_total(name) {
                    if name == "_Total" {
                        if field == 0 {
                            health.dpc = value;
                        } else {
                            health.interrupt = value;
                        }
                    }
                    return;
                }
                add_per_cpu(per_cpu, name, value);
            })?;
        }
        health.max_core_dpc = self.per_cpu.iter().map(|(_, v)| *v).fold(0.0, f64::max);

        let mut single = |index: usize| -> Result<Option<f64>> {
            let mut out = None;
            self.counters.read(index, |_, v| {
                out = Some(out.map_or(v, |o: f64| o.max(v)));
            })?;
            Ok(out)
        };
        health.cpu_performance = single(PERFORMANCE)?;
        // Zones without a sensor report 0 K.
        health.temperature_c = single(TEMPERATURE)?
            .filter(|k| *k > 0.0)
            .map(|k| k - 273.15);
        health.memory_available_mb = single(AVAILABLE_MB)?;
        health.memory_committed = single(COMMITTED)?;
        health.pages_input_per_sec = single(PAGES_INPUT)?;
        Ok((total, health))
    }
}

/// `_Total` and per-group totals such as `0,_Total`.
fn is_total(instance: &str) -> bool {
    instance.ends_with("_Total")
}

/// Adds `value` to the entry for `name`, creating it if new (allocates only then).
fn add_per_cpu(per_cpu: &mut Vec<(String, f64)>, name: &str, value: f64) {
    match per_cpu.iter_mut().find(|(n, _)| n == name) {
        Some((_, v)) => *v += value,
        None => per_cpu.push((name.to_owned(), value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_total_instances() {
        assert!(is_total("_Total"));
        assert!(is_total("0,_Total"));
        assert!(!is_total("0,15"));
    }

    #[test]
    fn sums_per_cpu_values_by_name() {
        let mut per_cpu = Vec::new();
        add_per_cpu(&mut per_cpu, "0,1", 2.0);
        add_per_cpu(&mut per_cpu, "0,2", 1.0);
        add_per_cpu(&mut per_cpu, "0,1", 3.0);
        assert_eq!(per_cpu, [("0,1".to_owned(), 5.0), ("0,2".to_owned(), 1.0)]);
    }

    #[test]
    fn samples_machine_cpu_and_health() {
        let mut cpu = MachineCpu::new().expect("open CPU counters");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (value, health) = cpu.sample().expect("sample CPU counters");
        assert!(value >= 0.0);
        assert!(health.dpc >= 0.0 && health.max_core_dpc >= 0.0);
        let available = health.memory_available_mb.expect("available memory");
        assert!(available > 0.0);
        let committed = health.memory_committed.expect("committed memory");
        assert!((0.0..=100.0).contains(&committed));
        assert!(health.cpu_performance.is_some());
    }
}
