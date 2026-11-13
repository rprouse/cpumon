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
use cpumon_core::time::{filetime_minus, format_utc, local_timestamp, now_filetime};
use cpumon_core::totals::Health;

use crate::gpu_cmd::adapter_name;
use crate::output::truncate;

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

    /// A thread kept waiting this long counts as a stall (the machine froze); starts an
    /// incident at once
    #[arg(long, default_value = "500ms", value_parser = parse_duration)]
    pub stall: Duration,

    /// One logical CPU spending at least this % in DPCs and interrupts counts as a
    /// driver problem; starts an incident at once
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub dpc: u8,

    /// Consecutive high samples to start an incident (for CPU, GPU and core), and
    /// normal samples to end any incident
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
    pub sustain: u32,

    /// Delete incidents older than this (checked at start and daily, between incidents)
    #[arg(long, default_value = "30d", value_parser = parse_duration)]
    pub retain: Duration,
}

/// How often the monitor prunes old incidents.
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

impl MonitorArgs {
    fn thresholds(&self) -> Thresholds {
        Thresholds {
            cpu: f64::from(self.cpu),
            gpu: f64::from(self.gpu),
            proc_core: f64::from(self.proc_core),
            stall_ms: self.stall.as_secs_f64() * 1000.0,
            dpc: f64::from(self.dpc),
            sustain: self.sustain,
        }
    }

    /// The options as command-line arguments, for running the monitor elsewhere (the
    /// scheduled task).
    pub fn to_args(&self) -> Vec<String> {
        let duration = |d: Duration| {
            if d.subsec_millis() == 0 {
                format!("{}s", d.as_secs())
            } else {
                format!("{}ms", d.as_millis())
            }
        };
        [
            ("--interval", duration(self.interval)),
            ("--stall", duration(self.stall)),
            ("--dpc", self.dpc.to_string()),
            ("--cpu", self.cpu.to_string()),
            ("--gpu", self.gpu.to_string()),
            ("--proc-core", self.proc_core.to_string()),
            ("--proc-cpu", self.proc_cpu.to_string()),
            ("--proc-gpu", self.proc_gpu.to_string()),
            ("--top", self.top.to_string()),
            ("--sustain", self.sustain.to_string()),
            ("--retain", duration(self.retain)),
        ]
        .into_iter()
        .flat_map(|(flag, value)| [flag.to_owned(), value])
        .collect()
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
    // Without a console (e.g. the logon task) there may be no Ctrl+C to handle; the
    // task is then stopped by Task Scheduler, and the next run closes any open incident.
    let (_keep_open, stop) = match stop_signal() {
        Ok(stop) => (None, stop),
        Err(e) => {
            eprintln!("warning: Ctrl+C handling unavailable ({e}); stop by ending the process");
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            (Some(tx), rx)
        }
    };

    println!(
        "Sampling every {:?} on {} logical CPUs. An incident starts when CPU >= {}%, GPU >= {}% \
         or one process >= {}% of a core for {} samples in a row, or at once when the system \
         stalls for {:?}, a CPU spends {}% in DPCs/interrupts, or a window stops responding.",
        args.interval,
        sampler.logical_cpus(),
        args.cpu,
        args.gpu,
        args.proc_core,
        args.sustain,
        args.stall,
        args.dpc,
    );
    println!("Recording to {}. Ctrl+C to stop.", db.display());
    if closed > 0 {
        println!("Closed {closed} incident(s) left open by a previous run.");
    }

    let mut started_at = Instant::now();
    let mut next = Instant::now() + args.interval;
    let mut next_prune = Instant::now();
    loop {
        if tracker.current().is_none() && Instant::now() >= next_prune {
            prune(recorder.store(), args.retain);
            next_prune = Instant::now() + PRUNE_EVERY;
        }
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
            stall_ms: sample.stall.as_secs_f64() * 1000.0,
            max_core_dpc: sample.health.max_core_dpc,
            hung_windows: sample.hung.len(),
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
            "{ts}  CPU {:5.1}%  GPU {:5.1}% (sum {:.1}%)  busiest process {:.1}% of a core  \
             stall {:.0} ms  [{triggers}]",
            observation.cpu,
            observation.gpu,
            sample.gpu.sum(),
            observation.max_proc_core,
            observation.stall_ms,
        );
        println!("{:21}{}", "", health_line(&sample.health));
        for window in sample.hung {
            println!(
                "    not responding: {} ({}) \"{}\"{}",
                sample.snapshot.name_of(window.pid).unwrap_or("?"),
                window.pid,
                truncate(&window.title, 80),
                if window.foreground {
                    " [foreground]"
                } else {
                    ""
                },
            );
        }
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

/// DPC/interrupt time, CPU speed, temperature and memory on one line.
fn health_line(h: &Health) -> String {
    let mut parts = vec![format!(
        "DPC {:.1}% + interrupts {:.1}% (worst CPU {:.1}%)",
        h.dpc, h.interrupt, h.max_core_dpc
    )];
    if let Some(perf) = h.cpu_performance {
        parts.push(format!("CPU speed {perf:.0}%"));
    }
    if let Some(t) = h.temperature_c {
        parts.push(format!("{t:.0} C"));
    }
    if let Some(mb) = h.memory_available_mb {
        parts.push(format!("{:.1} GB free", mb / 1024.0));
    }
    if let Some(c) = h.memory_committed {
        parts.push(format!("commit {c:.0}%"));
    }
    if let Some(p) = h.pages_input_per_sec {
        parts.push(format!("paging {p:.0} pages/s"));
    }
    parts.join("  ")
}

/// Deletes incidents older than `retain`, and processes no longer referenced.
fn prune(store: &Store, retain: Duration) {
    let before = format_utc(filetime_minus(now_filetime(), retain));
    match store.prune(&before) {
        Ok(stats) if stats.incidents > 0 || stats.processes > 0 => println!(
            "{}  Pruned {} incident(s) and {} process record(s) older than {:?}.",
            local_timestamp(),
            stats.incidents,
            stats.processes,
            retain,
        ),
        Ok(_) => {}
        Err(e) => eprintln!("{}  warning: pruning failed: {e}", local_timestamp()),
    }
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
        "{}  INCIDENT END{} after {}s ({} samples, {}): peak CPU {:.1}%, GPU {:.1}%, \
         process {:.1}% of a core, stall {:.0} ms, worst CPU in DPCs {:.1}%, {} hung window(s)",
        local_timestamp(),
        id.map(|id| format!(" #{id}")).unwrap_or_default(),
        duration.as_secs(),
        incident.samples,
        incident.triggers,
        incident.peak_cpu,
        incident.peak_gpu,
        incident.peak_proc_core,
        incident.peak_stall_ms,
        incident.peak_dpc,
        incident.peak_hung_windows,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: MonitorArgs,
    }

    fn parse(args: &[&str]) -> MonitorArgs {
        Wrapper::try_parse_from(std::iter::once("monitor").chain(args.iter().copied()))
            .unwrap()
            .args
    }

    #[test]
    fn to_args_round_trips() {
        for input in [
            &[][..],
            &[
                "--interval",
                "1500ms",
                "--cpu",
                "70",
                "--proc-core",
                "250",
                "--top",
                "3",
            ],
            &["--interval", "5m", "--sustain", "4", "--gpu", "95"],
        ] {
            let original = parse(input);
            let args = original.to_args();
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            assert_eq!(format!("{:?}", parse(&refs)), format!("{original:?}"));
        }
    }
}
