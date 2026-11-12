//! `cpumon monitor`: detect high-usage incidents and log the offending processes.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use cpumon_core::adapters::{AdapterInfo, list_adapters};
use cpumon_core::duration::parse_duration;
use cpumon_core::incident::{Incident, IncidentTracker, Observation, Thresholds, Transition};
use cpumon_core::offenders::{Offender, OffenderLimits, select_offenders};
use cpumon_core::sampler::{Sample, Sampler};
use cpumon_core::time::local_timestamp;

use crate::gpu_cmd::adapter_name;

#[derive(Debug, Args)]
pub struct MonitorArgs {
    /// Time between samples
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    pub interval: Duration,

    /// Total CPU % that counts as high usage
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub cpu: u8,

    /// Total GPU % (busiest engine) that counts as high usage
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub gpu: u8,

    /// A single process using at least this % of one core counts as high usage
    /// (catches runaway threads that barely move the machine total)
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u16).range(1..))]
    pub proc_core: u16,

    /// Per-process CPU % (of the whole machine) to record during an incident
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub proc_cpu: u8,

    /// Per-process GPU % to record during an incident
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub proc_gpu: u8,

    /// Always record at least this many top CPU and GPU processes during an incident
    #[arg(long, default_value_t = 5)]
    pub top: usize,

    /// Consecutive high samples to start an incident, and normal samples to end it
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
    pub sustain: u32,
}

impl MonitorArgs {
    fn thresholds(&self) -> Thresholds {
        Thresholds {
            cpu: f64::from(self.cpu),
            gpu: f64::from(self.gpu),
            proc_core: f64::from(self.proc_core),
            sustain: self.sustain,
        }
    }

    fn limits(&self) -> OffenderLimits {
        OffenderLimits {
            proc_cpu: f64::from(self.proc_cpu),
            // Anyone who could trigger an incident on their own is an offender.
            proc_core: f64::from(self.proc_core),
            proc_gpu: f64::from(self.proc_gpu),
            top: self.top,
        }
    }
}

pub fn run(args: &MonitorArgs) -> Result<()> {
    let adapters = list_adapters().unwrap_or_default();
    let mut sampler = Sampler::new().context("starting the sampler")?;
    let mut tracker = IncidentTracker::new(args.thresholds());
    let limits = args.limits();

    println!(
        "Sampling every {:?} on {} logical CPUs. An incident starts when CPU >= {}%, GPU >= {}% \
         or one process >= {}% of a core for {} samples in a row. Ctrl+C to stop.",
        args.interval,
        sampler.logical_cpus(),
        args.cpu,
        args.gpu,
        args.proc_core,
        args.sustain,
    );

    let mut started_at = Instant::now();
    let mut next = Instant::now() + args.interval;
    loop {
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        next += args.interval;
        // If sampling fell behind (e.g. after sleep/hibernate), don't try to catch up.
        let now = Instant::now();
        if next < now {
            next = now + args.interval;
        }

        let sample = match sampler.sample() {
            Ok(sample) => sample,
            Err(e) => {
                eprintln!("{}  warning: sampling failed: {e}", local_timestamp());
                continue;
            }
        };
        let observation = Observation {
            cpu: sample.cpu_total,
            gpu: sample.gpu.max_engine(),
            max_proc_core: sample.cpu.first().map_or(0.0, |p| p.pct_core),
        };
        let (transition, triggers) = tracker.observe(&observation);
        if !transition.in_incident() {
            continue;
        }

        let ts = local_timestamp();
        if transition == Transition::Started {
            started_at = Instant::now();
            println!("{ts}  INCIDENT START ({triggers})");
        }
        println!(
            "{ts}  CPU {:5.1}%  GPU {:5.1}% (sum {:.1}%)  busiest process {:.1}% of a core  [{triggers}]",
            observation.cpu,
            observation.gpu,
            sample.gpu.sum(),
            observation.max_proc_core,
        );
        let offenders = select_offenders(sample.cpu, &sample.gpu.processes, &limits, |pid| {
            sample.snapshot.find(pid).map(|e| e.key())
        });
        for offender in &offenders {
            print_offender(&sample, &adapters, offender);
        }
        if let Transition::Ended(incident) = transition {
            print_end(&incident, started_at.elapsed());
        }
    }
}

fn print_offender(sample: &Sample<'_>, adapters: &[AdapterInfo], o: &Offender) {
    let name = sample.snapshot.name_of(o.key.pid).unwrap_or("<exited>");
    let gpu = match o.gpu {
        Some(g) => format!(
            "{:5.1}% {:<11} on {}",
            g.utilization,
            sample.engine_types.name(g.engine_type),
            adapter_name(adapters, g.adapter),
        ),
        None => "    -".to_owned(),
    };
    println!(
        "    {:>6}  {:<28} cpu {:6.1}% of a core ({:4.1}% machine)  gpu {gpu}",
        o.key.pid, name, o.cpu_core, o.cpu_machine,
    );
}

fn print_end(incident: &Incident, duration: Duration) {
    println!(
        "{}  INCIDENT END after {}s ({} samples, {}): peak CPU {:.1}%, GPU {:.1}%, process {:.1}% of a core",
        local_timestamp(),
        duration.as_secs(),
        incident.samples,
        incident.triggers,
        incident.peak_cpu,
        incident.peak_gpu,
        incident.peak_proc_core,
    );
    println!();
}
