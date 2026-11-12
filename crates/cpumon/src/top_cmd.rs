//! `cpumon top`: live machine totals and the top CPU and GPU processes.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use cpumon_core::adapters::list_adapters;
use cpumon_core::duration::parse_duration;
use cpumon_core::sampler::Sampler;

use crate::gpu_cmd::adapter_name;

#[derive(Debug, Args)]
pub struct TopArgs {
    /// Time between samples
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    pub interval: Duration,

    /// Stop after this many samples [default: run until Ctrl+C]
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub count: Option<u32>,

    /// Number of processes to show for CPU and for GPU
    #[arg(long, default_value_t = 5)]
    pub top: usize,
}

pub fn run(args: &TopArgs) -> Result<()> {
    let adapters = list_adapters().unwrap_or_default();
    let mut sampler = Sampler::new().context("starting the sampler")?;
    let start = Instant::now();

    for n in 1.. {
        std::thread::sleep(args.interval);
        let t = Instant::now();
        let sample = sampler.sample().context("sampling")?;
        let sample_time = t.elapsed();

        println!(
            "+{:.0}s  CPU {:5.1}%   GPU {:5.1}% (busiest engine; sum {:.1}%)   [sample {:.1} ms]",
            start.elapsed().as_secs_f64(),
            sample.cpu_total,
            sample.gpu.max_engine(),
            sample.gpu.sum(),
            sample_time.as_secs_f64() * 1000.0,
        );

        println!("  CPU   machine    core     pid  process");
        for p in sample.cpu.iter().take(args.top) {
            println!(
                "       {:5.1}%  {:6.1}%  {:>6}  {}",
                p.pct_machine,
                p.pct_core,
                p.key.pid,
                sample.snapshot.name_of(p.key.pid).unwrap_or("<exited>"),
            );
        }

        println!("  GPU   engine         pid  process                          adapter");
        let busy_gpu = sample.gpu.processes.iter().filter(|p| p.utilization >= 0.1);
        for p in busy_gpu.take(args.top) {
            println!(
                "       {:5.1}%  {:<8} {:>6}  {:<32} {}",
                p.utilization,
                sample.engine_types.name(p.engine_type),
                p.pid,
                sample.snapshot.name_of(p.pid).unwrap_or("<exited>"),
                adapter_name(&adapters, p.adapter),
            );
        }
        println!();

        if args.count.is_some_and(|count| n >= count) {
            break;
        }
    }
    Ok(())
}
