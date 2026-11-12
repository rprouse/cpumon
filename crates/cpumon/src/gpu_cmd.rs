//! `cpumon gpu`: live per-adapter, per-engine and per-process GPU usage.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use cpumon_core::adapters::{AdapterInfo, list_adapters};
use cpumon_core::duration::parse_duration;
use cpumon_core::gpu::{EngineTypes, GpuSampler, Luid, summarize};
use cpumon_core::snapshot::ProcessSnapshot;

/// Engines below this utilisation are not listed.
const IDLE_ENGINE_PCT: f64 = 0.1;

#[derive(Debug, Args)]
pub struct GpuArgs {
    /// Time between samples
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    pub interval: Duration,

    /// Stop after this many samples [default: run until Ctrl+C]
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub count: Option<u32>,

    /// Number of processes to show per sample
    #[arg(long, default_value_t = 10)]
    pub top: usize,
}

pub fn run(args: &GpuArgs) -> Result<()> {
    let adapters = list_adapters().unwrap_or_else(|e| {
        eprintln!("warning: could not list GPU adapters: {e}");
        Vec::new()
    });
    let mut sampler = GpuSampler::new().context("opening the GPU Engine performance counters")?;
    let mut snapshot = ProcessSnapshot::default();
    let start = Instant::now();

    for n in 1.. {
        std::thread::sleep(args.interval);

        let t = Instant::now();
        let reading = sampler.sample().context("sampling GPU counters")?;
        let pdh_time = t.elapsed();
        let summary = summarize(reading.usages);

        let t = Instant::now();
        snapshot.refresh().context("taking process snapshot")?;
        let snapshot_time = t.elapsed();

        println!(
            "+{:.1}s  GPU busiest engine {:5.1}%  sum of engines {:5.1}%   ({} instances, pdh {:.1} ms, snapshot {:.1} ms)",
            start.elapsed().as_secs_f64(),
            summary.max_engine(),
            summary.sum(),
            reading.instance_count,
            pdh_time.as_secs_f64() * 1000.0,
            snapshot_time.as_secs_f64() * 1000.0,
        );

        for adapter in &summary.adapters {
            let busiest = adapter
                .busiest()
                .map(|e| reading.engine_types.name(e.engine_type))
                .unwrap_or("-");
            println!(
                "  {} [{}]  busiest {} {:.1}%, sum {:.1}%",
                adapter_name(&adapters, adapter.adapter),
                adapter.adapter,
                busiest,
                adapter.max_engine(),
                adapter.sum,
            );
            println!("    {}", engine_line(adapter, reading.engine_types));
        }

        if summary.processes.is_empty() {
            println!("  no process is using the GPU");
        } else {
            println!("  top processes:");
            for p in summary.processes.iter().take(args.top) {
                println!(
                    "    {:5.1}%  {:<12} {:>6}  {:<32} {}",
                    p.utilization,
                    reading.engine_types.name(p.engine_type),
                    p.pid,
                    snapshot.name_of(p.pid).unwrap_or("<exited>"),
                    adapter_name(&adapters, p.adapter),
                );
            }
        }
        println!();

        if args.count.is_some_and(|count| n >= count) {
            break;
        }
    }
    Ok(())
}

pub(crate) fn adapter_name(adapters: &[AdapterInfo], luid: Luid) -> String {
    adapters
        .iter()
        .find(|a| a.luid == luid)
        .map_or_else(|| format!("adapter {luid}"), |a| a.description.clone())
}

fn engine_line(adapter: &cpumon_core::gpu::AdapterSummary, types: &EngineTypes) -> String {
    let busy: Vec<String> = adapter
        .engines
        .iter()
        .filter(|e| e.utilization >= IDLE_ENGINE_PCT)
        .map(|e| {
            format!(
                "{}#{} {:.1}%",
                types.name(e.engine_type),
                e.eng,
                e.utilization
            )
        })
        .collect();
    if busy.is_empty() {
        "all engines idle".to_owned()
    } else {
        busy.join("  ")
    }
}
