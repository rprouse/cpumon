//! GPU usage per process, adapter and engine.
//!
//! Usage comes from the `GPU Engine(*)\Utilization Percentage` performance counters.
//! Each counter instance describes one engine of one adapter as used by one process,
//! for example `pid_1234_luid_0x00000000_0x0000C2A1_phys_0_eng_3_engtype_3D`.
//!
//! Task Manager reports a process's GPU usage as its busiest engine, and an adapter's
//! usage as its busiest engine across all processes. Some tools instead add engines
//! together, which can reach 100% while no single engine is busy, so
//! [`GpuSummary`] keeps both figures.

use std::collections::HashMap;
use std::fmt;

use crate::error::Result;
use crate::pdh::CounterArray;

/// Counter path for per-process, per-engine GPU utilisation.
const GPU_ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";

/// Locally unique identifier of a GPU adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Luid {
    pub high: u32,
    pub low: u32,
}

impl fmt::Display for Luid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}_0x{:08X}", self.high, self.low)
    }
}

/// The parts of a `GPU Engine` counter instance name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceName<'a> {
    pub pid: u32,
    pub adapter: Luid,
    pub phys: u32,
    pub eng: u32,
    /// Engine type such as `3D`, `Copy` or `VideoDecode`. May be empty.
    pub engine_type: &'a str,
}

/// Parses a `GPU Engine` instance name such as
/// `pid_1234_luid_0x00000000_0x0000C2A1_phys_0_eng_3_engtype_3D`.
pub fn parse_instance_name(name: &str) -> Option<InstanceName<'_>> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once("_luid_")?;
    let (high, rest) = rest.split_once('_')?;
    let (low, rest) = rest.split_once("_phys_")?;
    let (phys, rest) = rest.split_once("_eng_")?;
    let (eng, engine_type) = rest.split_once("_engtype_")?;
    Some(InstanceName {
        pid: pid.parse().ok()?,
        adapter: Luid {
            high: parse_hex(high)?,
            low: parse_hex(low)?,
        },
        phys: phys.parse().ok()?,
        eng: eng.parse().ok()?,
        engine_type,
    })
}

fn parse_hex(s: &str) -> Option<u32> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u32::from_str_radix(digits, 16).ok()
}

/// Index of an interned engine type name; see [`EngineTypes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineTypeId(u16);

/// Interned engine type names, so samples don't allocate a string per instance.
#[derive(Debug, Default)]
pub struct EngineTypes {
    names: Vec<String>,
}

impl EngineTypes {
    /// Returns the id for `name`, adding it if new.
    pub fn intern(&mut self, name: &str) -> EngineTypeId {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return EngineTypeId(i as u16);
        }
        self.names.push(name.to_owned());
        EngineTypeId((self.names.len() - 1) as u16)
    }

    /// Name for an id returned by [`intern`](Self::intern). Empty names display as `?`.
    pub fn name(&self, id: EngineTypeId) -> &str {
        match self.names.get(id.0 as usize).map(String::as_str) {
            Some("") | None => "?",
            Some(name) => name,
        }
    }
}

/// Utilisation of one engine by one process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineUsage {
    pub pid: u32,
    pub adapter: Luid,
    pub phys: u32,
    pub eng: u32,
    pub engine_type: EngineTypeId,
    /// Percent of the engine's time, 0–100 (can slightly exceed 100 due to timing).
    pub utilization: f64,
}

/// Total utilisation of one engine across all processes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineTotal {
    pub adapter: Luid,
    pub phys: u32,
    pub eng: u32,
    pub engine_type: EngineTypeId,
    pub utilization: f64,
}

/// Usage of one adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterSummary {
    pub adapter: Luid,
    /// Engines sorted busiest first.
    pub engines: Vec<EngineTotal>,
    /// Sum of all engine utilisations (what "add everything" tools report).
    pub sum: f64,
}

impl AdapterSummary {
    /// The busiest engine (what Task Manager reports for the adapter).
    pub fn busiest(&self) -> Option<&EngineTotal> {
        self.engines.first()
    }

    /// Utilisation of the busiest engine, or 0.
    pub fn max_engine(&self) -> f64 {
        self.busiest().map_or(0.0, |e| e.utilization)
    }
}

/// GPU usage of one process: its busiest engine, as Task Manager reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessGpu {
    pub pid: u32,
    pub adapter: Luid,
    pub engine_type: EngineTypeId,
    pub utilization: f64,
}

/// GPU usage aggregated per adapter, engine and process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuSummary {
    /// Adapters sorted by LUID.
    pub adapters: Vec<AdapterSummary>,
    /// Processes with non-zero usage, busiest first.
    pub processes: Vec<ProcessGpu>,
}

impl GpuSummary {
    /// Busiest engine across all adapters.
    pub fn max_engine(&self) -> f64 {
        self.adapters
            .iter()
            .map(AdapterSummary::max_engine)
            .fold(0.0, f64::max)
    }

    /// Sum of every engine on every adapter.
    pub fn sum(&self) -> f64 {
        self.adapters.iter().map(|a| a.sum).sum()
    }
}

type EngineKey = (Luid, u32, u32);

/// Aggregates raw per-process engine usage into per-engine, per-adapter and
/// per-process figures.
pub fn summarize(usages: &[EngineUsage]) -> GpuSummary {
    let mut engines: HashMap<EngineKey, EngineTotal> = HashMap::new();
    let mut per_process_engine: HashMap<(u32, EngineKey), (EngineTypeId, f64)> = HashMap::new();

    for u in usages {
        let key = (u.adapter, u.phys, u.eng);
        engines
            .entry(key)
            .or_insert(EngineTotal {
                adapter: u.adapter,
                phys: u.phys,
                eng: u.eng,
                engine_type: u.engine_type,
                utilization: 0.0,
            })
            .utilization += u.utilization;
        per_process_engine
            .entry((u.pid, key))
            .or_insert((u.engine_type, 0.0))
            .1 += u.utilization;
    }

    let mut adapters: HashMap<Luid, AdapterSummary> = HashMap::new();
    for total in engines.into_values() {
        let summary = adapters
            .entry(total.adapter)
            .or_insert_with(|| AdapterSummary {
                adapter: total.adapter,
                engines: Vec::new(),
                sum: 0.0,
            });
        summary.sum += total.utilization;
        summary.engines.push(total);
    }
    let mut adapters: Vec<AdapterSummary> = adapters.into_values().collect();
    for a in &mut adapters {
        a.engines.sort_by(|x, y| {
            y.utilization
                .total_cmp(&x.utilization)
                .then((x.phys, x.eng).cmp(&(y.phys, y.eng)))
        });
    }
    adapters.sort_by_key(|a| a.adapter);

    let mut processes: HashMap<u32, ProcessGpu> = HashMap::new();
    for ((pid, (adapter, _, _)), (engine_type, utilization)) in per_process_engine {
        let candidate = ProcessGpu {
            pid,
            adapter,
            engine_type,
            utilization,
        };
        processes
            .entry(pid)
            .and_modify(|p| {
                if utilization > p.utilization {
                    *p = candidate;
                }
            })
            .or_insert(candidate);
    }
    let mut processes: Vec<ProcessGpu> = processes
        .into_values()
        .filter(|p| p.utilization > 0.0)
        .collect();
    processes.sort_by(|a, b| {
        b.utilization
            .total_cmp(&a.utilization)
            .then(a.pid.cmp(&b.pid))
    });

    GpuSummary {
        adapters,
        processes,
    }
}

/// A GPU sample: raw per-process engine usage plus the names of its engine types.
#[derive(Debug)]
pub struct GpuReading<'a> {
    pub usages: &'a [EngineUsage],
    pub engine_types: &'a EngineTypes,
    /// Number of counter instances returned, including ones that could not be parsed.
    pub instance_count: usize,
}

/// Samples GPU engine utilisation. Keep one alive and call [`sample`](Self::sample)
/// periodically; utilisation is measured over the time between calls.
pub struct GpuSampler {
    counters: CounterArray,
    engine_types: EngineTypes,
    usages: Vec<EngineUsage>,
}

impl GpuSampler {
    /// Opens the counters and takes the baseline reading.
    pub fn new() -> Result<Self> {
        Ok(Self {
            // Optional: machines without a WDDM GPU driver (e.g. some VMs and CI
            // runners) have no GPU Engine counters; GPU usage then reads as zero.
            counters: CounterArray::open_many(&[(GPU_ENGINE_COUNTER, false)])?,
            engine_types: EngineTypes::default(),
            usages: Vec::new(),
        })
    }

    /// Names for the engine type ids in samples.
    pub fn engine_types(&self) -> &EngineTypes {
        &self.engine_types
    }

    /// Reads utilisation since the previous call (or since [`new`](Self::new)).
    pub fn sample(&mut self) -> Result<GpuReading<'_>> {
        self.usages.clear();
        let usages = &mut self.usages;
        let engine_types = &mut self.engine_types;
        let instance_count = self.counters.collect(|name, value| {
            if let Some(i) = parse_instance_name(name) {
                usages.push(EngineUsage {
                    pid: i.pid,
                    adapter: i.adapter,
                    phys: i.phys,
                    eng: i.eng,
                    engine_type: engine_types.intern(i.engine_type),
                    utilization: value,
                });
            }
        })?;
        Ok(GpuReading {
            usages: &self.usages,
            engine_types: &self.engine_types,
            instance_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: Luid = Luid {
        high: 0,
        low: 0xC2A1,
    };
    const INTEL: Luid = Luid {
        high: 0,
        low: 0x1234,
    };

    #[test]
    fn parses_instance_name() {
        let parsed =
            parse_instance_name("pid_1234_luid_0x00000000_0x0000C2A1_phys_0_eng_3_engtype_3D");
        assert_eq!(
            parsed,
            Some(InstanceName {
                pid: 1234,
                adapter: NVIDIA,
                phys: 0,
                eng: 3,
                engine_type: "3D",
            })
        );
    }

    #[test]
    fn parses_engine_types_with_underscores_spaces_or_nothing() {
        let types = [
            (
                "pid_8_luid_0x00000001_0x0000ABCD_phys_1_eng_12_engtype_Compute_0",
                "Compute_0",
            ),
            (
                "pid_8_luid_0x00000001_0x0000ABCD_phys_1_eng_12_engtype_Video Codec",
                "Video Codec",
            ),
            (
                "pid_8_luid_0x00000001_0x0000ABCD_phys_1_eng_12_engtype_",
                "",
            ),
        ];
        for (name, expected) in types {
            let parsed = parse_instance_name(name).expect(name);
            assert_eq!(parsed.engine_type, expected);
            assert_eq!(
                parsed.adapter,
                Luid {
                    high: 1,
                    low: 0xABCD
                }
            );
            assert_eq!((parsed.pid, parsed.phys, parsed.eng), (8, 1, 12));
        }
    }

    #[test]
    fn rejects_malformed_instance_names() {
        for bad in [
            "",
            "_Total",
            "pid_x_luid_0x0_0x0_phys_0_eng_0_engtype_3D",
            "pid_1_luid_00000000_0x0_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x0_0x0_phys_0_eng_0",
        ] {
            assert_eq!(parse_instance_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn interns_engine_types() {
        let mut types = EngineTypes::default();
        let a = types.intern("3D");
        let b = types.intern("Copy");
        assert_eq!(types.intern("3D"), a);
        assert_ne!(a, b);
        assert_eq!(types.name(a), "3D");
        let empty = types.intern("");
        assert_eq!(types.name(empty), "?");
    }

    fn usage(pid: u32, adapter: Luid, eng: u32, ty: EngineTypeId, utilization: f64) -> EngineUsage {
        EngineUsage {
            pid,
            adapter,
            phys: 0,
            eng,
            engine_type: ty,
            utilization,
        }
    }

    #[test]
    fn summarizes_like_task_manager_and_keeps_sum() {
        let mut types = EngineTypes::default();
        let d3 = types.intern("3D");
        let copy = types.intern("Copy");
        let decode = types.intern("VideoDecode");

        let summary = summarize(&[
            // Two processes share the NVIDIA 3D engine.
            usage(100, NVIDIA, 0, d3, 30.0),
            usage(200, NVIDIA, 0, d3, 20.0),
            // Process 100 also uses copy and decode.
            usage(100, NVIDIA, 1, copy, 40.0),
            usage(100, NVIDIA, 2, decode, 45.0),
            // Process 300 on the Intel adapter.
            usage(300, INTEL, 0, d3, 5.0),
            // Idle process.
            usage(400, INTEL, 0, d3, 0.0),
        ]);

        assert_eq!(summary.adapters.len(), 2);
        let nvidia = summary
            .adapters
            .iter()
            .find(|a| a.adapter == NVIDIA)
            .unwrap();
        // Busiest engine is 3D at 30 + 20 = 50.
        assert_eq!(nvidia.max_engine(), 50.0);
        assert_eq!(nvidia.busiest().unwrap().engine_type, d3);
        assert_eq!(nvidia.sum, 135.0);
        assert_eq!(
            nvidia.engines.iter().map(|e| e.eng).collect::<Vec<_>>(),
            [0, 2, 1]
        );

        assert_eq!(summary.max_engine(), 50.0);
        assert_eq!(summary.sum(), 140.0);

        // Process 100's busiest engine is decode at 45, not the sum of its engines.
        let pids: Vec<_> = summary
            .processes
            .iter()
            .map(|p| (p.pid, p.utilization))
            .collect();
        assert_eq!(pids, [(100, 45.0), (200, 20.0), (300, 5.0)]);
        assert_eq!(summary.processes[0].engine_type, decode);
    }

    #[test]
    fn summarizes_empty_input() {
        let summary = summarize(&[]);
        assert_eq!(summary, GpuSummary::default());
        assert_eq!(summary.max_engine(), 0.0);
        assert_eq!(summary.sum(), 0.0);
    }

    #[test]
    fn live_sampler_tolerates_any_machine() {
        let mut sampler = GpuSampler::new().expect("open GPU counters");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let reading = sampler.sample().expect("sample GPU counters");
        assert!(reading.usages.len() <= reading.instance_count);
    }
}
