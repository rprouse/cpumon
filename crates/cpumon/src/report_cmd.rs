//! `cpumon report`: the worst offenders over a period.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, ValueEnum};
use cpumon_core::duration::parse_duration;
use cpumon_core::report::{self, build_report};
use cpumon_core::time::utc_to_local;

use crate::output::{cutoff_utc, open_existing, percent, seconds, table, truncate};

/// Longest command line shown in a report row.
const MAX_LABEL_CHARS: usize = 200;

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// How far back to look
    #[arg(long, default_value = "7d", value_parser = parse_duration)]
    pub since: Duration,

    /// How to group offenders
    #[arg(long, value_enum, default_value_t = GroupBy::Name)]
    pub by: GroupBy,

    /// Which usage to rank by
    #[arg(long, value_enum, default_value_t = Metric::Cpu)]
    pub metric: Metric,

    /// Number of rows to show
    #[arg(long, default_value_t = 20)]
    pub top: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GroupBy {
    /// Process image name
    Name,
    /// The application a process belongs to (its highest ancestor below a shell or
    /// service host such as explorer.exe or svchost.exe)
    Root,
    /// Exact command line
    Cmdline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Metric {
    /// CPU time in core-seconds
    Cpu,
    /// GPU busy time in seconds
    Gpu,
}

pub fn run(args: &ReportArgs, db: &Path) -> Result<()> {
    let Some(store) = open_existing(db)? else {
        return Ok(());
    };
    let (by, column) = match args.by {
        GroupBy::Name => (report::GroupBy::Name, "Process"),
        GroupBy::Root => (report::GroupBy::Root, "Application"),
        GroupBy::Cmdline => (report::GroupBy::CommandLine, "Command line"),
    };
    let (metric, what) = match args.metric {
        Metric::Cpu => (report::Metric::Cpu, "CPU time"),
        Metric::Gpu => (report::Metric::Gpu, "GPU time"),
    };

    let since = cutoff_utc(args.since);
    let usage = store.process_usage_since(&since)?;
    let processes = store.processes()?;
    let rows = build_report(&usage, &processes, by, metric, args.top);
    if rows.is_empty() {
        println!(
            "No {} recorded during incidents since {}.",
            what,
            utc_to_local(&since)
        );
        return Ok(());
    }

    println!(
        "Worst offenders by {what} during incidents since {}:",
        utc_to_local(&since)
    );
    let mut t = table(
        &[
            "#",
            column,
            "CPU time",
            "Peak CPU\n(% of a core)",
            "GPU time",
            "Peak GPU",
            "Incidents",
            "Processes",
            "Last seen",
        ],
        &[0, 2, 3, 4, 5, 6, 7],
    );
    for (rank, row) in rows.iter().enumerate() {
        t.add_row(vec![
            (rank + 1).to_string(),
            truncate(&row.label, MAX_LABEL_CHARS),
            seconds(row.cpu_seconds),
            percent(row.peak_cpu_core),
            seconds(row.gpu_seconds),
            percent(row.peak_gpu),
            row.incidents.to_string(),
            row.processes.to_string(),
            utc_to_local(&row.last_seen_utc),
        ]);
    }
    println!("{t}");
    println!(
        "CPU time is in core-seconds (one core fully busy for 1 s = 1 s); GPU time is \
         busy time on the process's busiest engine. Only time during incidents counts."
    );
    Ok(())
}
