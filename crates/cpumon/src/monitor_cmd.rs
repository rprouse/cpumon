//! `cpumon monitor`: detect high-usage incidents, record them to the database and log
//! the offending processes.

use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use cpumon_core::adapters::{AdapterInfo, list_adapters};
use cpumon_core::console::stop_signal;
use cpumon_core::duration::parse_duration;
use cpumon_core::incident::{Incident, IncidentTracker, Observation, Thresholds, Transition};
use cpumon_core::offenders::{Offender, OffenderLimits, select_offenders};
use cpumon_core::recorder::{NewProcess, Recorder};
use cpumon_core::sampler::{Sample, Sampler};
use cpumon_core::store::Store;
use cpumon_core::time::{local_timestamp, now_filetime};

use crate::gpu_cmd::adapter_name;

/// Longest command line printed to the console (the database keeps it all).
const MAX_COMMAND_LINE_CHARS: usize = 300;

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

pub fn run(args: &MonitorArgs, db: &Path) -> Result<()> {
    let adapters = list_adapters().unwrap_or_default();
    let store = Store::open(db).with_context(|| format!("opening database {}", db.display()))?;
    store.upsert_adapters(&adapters)?;
    let closed = store.close_open_incidents()?;
    let mut recorder = Recorder::new(store);
    let mut sampler = Sampler::new().context("starting the sampler")?;
    let mut tracker = IncidentTracker::new(args.thresholds());
    let limits = args.limits();
    let stop = stop_signal().context("installing the Ctrl+C handler")?;

    println!(
        "Sampling every {:?} on {} logical CPUs. An incident starts when CPU >= {}%, GPU >= {}% \
         or one process >= {}% of a core for {} samples in a row.",
        args.interval,
        sampler.logical_cpus(),
        args.cpu,
        args.gpu,
        args.proc_core,
        args.sustain,
    );
    println!("Recording to {}. Ctrl+C to stop.", db.display());
    if closed > 0 {
        println!("Closed {closed} incident(s) left open by a previous run.");
    }

    let mut started_at = Instant::now();
    let mut next = Instant::now() + args.interval;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
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
        let incident = match transition {
            Transition::Idle => continue,
            Transition::Ended(incident) => incident,
            Transition::Started | Transition::Ongoing => {
                *tracker.current().expect("incident in progress")
            }
        };

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
        let new_processes = recorder
            .record(&incident, &observation, triggers, &sample, &offenders)
            .unwrap_or_else(|e| {
                eprintln!("{ts}  warning: could not record sample: {e}");
                Vec::new()
            });

        for offender in &offenders {
            print_offender(&sample, &adapters, offender);
            if let Some(new) = new_processes.iter().find(|n| n.key == offender.key) {
                print_details(&sample, new);
            }
        }

        if let Transition::Ended(incident) = transition {
            end_incident(&mut recorder, &incident, started_at.elapsed());
            recorder.forget_exited(sample.snapshot);
        }
    }

    if let Some(incident) = tracker.finish() {
        end_incident(&mut recorder, &incident, started_at.elapsed());
    }
    println!("Stopped.");
    Ok(())
}

fn end_incident(recorder: &mut Recorder, incident: &Incident, duration: Duration) {
    let id = recorder.incident_id();
    if let Err(e) = recorder.end(incident, now_filetime()) {
        eprintln!(
            "{}  warning: could not record incident end: {e}",
            local_timestamp()
        );
    }
    println!(
        "{}  INCIDENT END{} after {}s ({} samples, {}): peak CPU {:.1}%, GPU {:.1}%, process {:.1}% of a core",
        local_timestamp(),
        id.map(|id| format!(" #{id}")).unwrap_or_default(),
        duration.as_secs(),
        incident.samples,
        incident.triggers,
        incident.peak_cpu,
        incident.peak_gpu,
        incident.peak_proc_core,
    );
    println!();
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

/// Prints the command line, ancestry and user of a process seen for the first time.
fn print_details(sample: &Sample<'_>, new: &NewProcess) {
    const INDENT: &str = "            ";
    if let Some(cmd) = new
        .details
        .command_line
        .as_deref()
        .filter(|c| !c.is_empty())
    {
        println!("{INDENT}cmd:  {}", truncate(cmd, MAX_COMMAND_LINE_CHARS));
    }
    let tree: Vec<String> = sample
        .snapshot
        .ancestors(new.key)
        .iter()
        .map(|k| {
            let name = sample.snapshot.name_of(k.pid).unwrap_or("?");
            format!("{name} ({})", k.pid)
        })
        .collect();
    if !tree.is_empty() {
        println!("{INDENT}tree: <- {}", tree.join(" <- "));
    }
    if let Some(user) = &new.details.user {
        println!("{INDENT}user: {user}");
    }
    if let Some(error) = &new.details.error {
        println!("{INDENT}note: {error}");
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_strings_on_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("ééé", 2), "éé...");
    }
}
