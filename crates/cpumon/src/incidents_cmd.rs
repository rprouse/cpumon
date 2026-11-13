//! `cpumon incidents` and `cpumon show`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Args;
use cpumon_core::duration::parse_duration;
use cpumon_core::store::{IncidentProcess, IncidentSummary, ProcessInfo, Store};
use cpumon_core::time::utc_to_local;

use crate::output::{
    cutoff_utc, elapsed, millis, open_existing, percent, seconds, table, truncate,
};

/// Longest command line printed; the database keeps the full text.
const MAX_COMMAND_LINE_CHARS: usize = 1000;

#[derive(Debug, Args)]
pub struct IncidentsArgs {
    /// How far back to look
    #[arg(long, default_value = "24h", value_parser = parse_duration)]
    pub since: Duration,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Incident id (from `cpumon incidents`)
    pub incident_id: i64,

    /// Also list every sample (machine totals over time)
    #[arg(long)]
    pub samples: bool,
}

pub fn run_list(args: &IncidentsArgs, db: &Path) -> Result<()> {
    let Some(store) = open_existing(db)? else {
        return Ok(());
    };
    let since = cutoff_utc(args.since);
    let incidents = store.incidents_since(&since)?;
    if incidents.is_empty() {
        println!("No incidents since {}.", utc_to_local(&since));
        return Ok(());
    }

    let mut t = table(
        &[
            "#",
            "Started",
            "Duration",
            "Triggers",
            "Samples",
            "Peak CPU",
            "Peak GPU",
            "Busiest process\n(% of a core)",
            "Worst\nstall",
            "Worst CPU\nin DPCs",
            "Top process",
            "Not responding",
        ],
        &[0, 2, 4, 5, 6, 7, 8, 9],
    );
    for i in &incidents {
        t.add_row(vec![
            i.id.to_string(),
            utc_to_local(&i.started_utc),
            duration(i),
            i.triggers.clone(),
            i.samples.to_string(),
            percent(i.peak_cpu),
            percent(i.peak_gpu),
            percent(i.peak_proc_core),
            i.peak_stall_ms.map(millis).unwrap_or_default(),
            i.peak_dpc.map(percent).unwrap_or_default(),
            i.top_process.clone().unwrap_or_default(),
            i.hung_process.clone().unwrap_or_default(),
        ]);
    }
    println!("{t}");
    println!(
        "{} incident(s) since {}. `cpumon show <#>` for details.",
        incidents.len(),
        utc_to_local(&since)
    );
    Ok(())
}

fn duration(i: &IncidentSummary) -> String {
    match &i.ended_utc {
        Some(end) => elapsed(&i.started_utc, end).unwrap_or_default(),
        None => "ongoing".to_owned(),
    }
}

pub fn run_show(args: &ShowArgs, db: &Path) -> Result<()> {
    let Some(store) = open_existing(db)? else {
        return Ok(());
    };
    let Some(incident) = store.incident(args.incident_id)? else {
        bail!(
            "no incident #{} (see `cpumon incidents --since 30d`)",
            args.incident_id
        );
    };

    let ended = match &incident.ended_utc {
        Some(end) => format!("ended {} after {}", utc_to_local(end), duration(&incident)),
        None => "still in progress".to_owned(),
    };
    println!(
        "Incident #{}: started {}, {ended}.",
        incident.id,
        utc_to_local(&incident.started_utc)
    );
    println!(
        "Triggers: {}. {} samples. Peak CPU {}, GPU {} (busiest engine), busiest process {} of a core.",
        incident.triggers,
        incident.samples,
        percent(incident.peak_cpu),
        percent(incident.peak_gpu),
        percent(incident.peak_proc_core),
    );
    if let (Some(stall), Some(dpc)) = (incident.peak_stall_ms, incident.peak_dpc) {
        println!(
            "Responsiveness: worst stall {}, worst CPU in DPCs/interrupts {}.",
            millis(stall),
            percent(dpc)
        );
    }

    let hung = store.incident_hung_windows(incident.id)?;
    if !hung.is_empty() {
        println!();
        println!("Not responding:");
        for h in &hung {
            println!(
                "    {} ({}) \"{}\": {} sample(s), {} to {}{}",
                h.process.name,
                h.process.pid,
                truncate(&h.title, 100),
                h.samples,
                utc_to_local(&h.first_utc),
                utc_to_local(&h.last_utc),
                if h.foreground {
                    ", in the foreground"
                } else {
                    ""
                },
            );
        }
    }

    let processes = store.incident_processes(incident.id)?;
    if !processes.is_empty() {
        println!();
        print_process_table(&processes);
        for p in &processes {
            print_process_details(&store, &p.process)?;
        }
    }
    // Processes with a hung window that were not CPU/GPU offenders.
    let mut shown: Vec<i64> = processes.iter().map(|p| p.process.id).collect();
    for h in &hung {
        if !shown.contains(&h.process.id) {
            shown.push(h.process.id);
            print_process_details(&store, &h.process)?;
        }
    }

    if args.samples {
        println!();
        let mut t = table(
            &[
                "Time",
                "Covers",
                "CPU",
                "GPU",
                "GPU sum",
                "Busiest process\n(% of a core)",
                "Stall",
                "Worst CPU\nin DPCs",
                "CPU\nspeed",
                "Free\nmemory",
                "Paging\n(pages/s)",
                "Hung\nwindows",
                "Triggers",
            ],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        );
        let opt = |v: Option<f64>, f: fn(f64) -> String| v.map(f).unwrap_or_default();
        for s in store.incident_samples(incident.id)? {
            t.add_row(vec![
                utc_to_local(&s.ts_utc),
                seconds(s.seconds),
                percent(s.cpu_total),
                percent(s.gpu_max_engine),
                percent(s.gpu_sum),
                percent(s.max_proc_core),
                opt(s.stall_ms, millis),
                opt(s.max_core_dpc, percent),
                opt(s.cpu_performance, |p| format!("{p:.0}%")),
                opt(s.memory_available_mb, |mb| format!("{:.1} GB", mb / 1024.0)),
                opt(s.pages_input_per_sec, |p| format!("{p:.0}")),
                s.hung_windows.map(|n| n.to_string()).unwrap_or_default(),
                s.triggers,
            ]);
        }
        println!("{t}");
    }
    Ok(())
}

fn print_process_table(processes: &[IncidentProcess]) {
    let mut t = table(
        &[
            "PID",
            "Name",
            "Samples",
            "CPU time",
            "Peak CPU\n(% of a core)",
            "GPU time",
            "Peak GPU",
            "GPU engine",
        ],
        &[0, 2, 3, 4, 5, 6],
    );
    for p in processes {
        let engine = match (&p.gpu_engine, &p.gpu_adapter) {
            (Some(engine), Some(adapter)) => format!("{engine} on {adapter}"),
            (Some(engine), None) => engine.clone(),
            _ => String::new(),
        };
        t.add_row(vec![
            p.process.pid.to_string(),
            p.process.name.clone(),
            p.samples.to_string(),
            seconds(p.cpu_seconds),
            percent(p.peak_cpu_core),
            seconds(p.gpu_seconds),
            percent(p.peak_gpu),
            engine,
        ]);
    }
    println!("{t}");
}

fn print_process_details(store: &Store, process: &ProcessInfo) -> Result<()> {
    const INDENT: &str = "    ";
    println!();
    println!(
        "{} ({}), started {}",
        process.name,
        process.pid,
        utc_to_local(&process.create_time_utc)
    );
    if let Some(cmd) = process.command_line.as_deref().filter(|c| !c.is_empty()) {
        println!("{INDENT}cmd:  {}", truncate(cmd, MAX_COMMAND_LINE_CHARS));
    } else if let Some(path) = &process.image_path {
        println!("{INDENT}path: {path}");
    }
    let ancestors = store.ancestors(process.id)?;
    if !ancestors.is_empty() {
        println!("{INDENT}tree: <- {}", chain(&ancestors));
    }
    if let Some(user) = &process.user_name {
        println!("{INDENT}user: {user}");
    }
    if let Some(error) = &process.info_error {
        println!("{INDENT}note: {}", truncate(error, 200));
    }
    Ok(())
}

fn chain(ancestors: &[ProcessInfo]) -> String {
    ancestors
        .iter()
        .map(|a| format!("{} ({})", a.name, a.pid))
        .collect::<Vec<_>>()
        .join(" <- ")
}
