//! `cpumon`: monitor Windows CPU/GPU usage and record the processes responsible.

mod gpu_cmd;
mod incidents_cmd;
mod install_cmd;
mod monitor_cmd;
mod output;
mod report_cmd;
mod top_cmd;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cpumon_core::duration::parse_duration;
use cpumon_core::paths::default_db_path;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Database file [default: %LOCALAPPDATA%\cpumon\cpumon.db]
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch CPU/GPU usage and record offending processes
    Monitor(monitor_cmd::MonitorArgs),
    /// Show the worst offenders
    Report(report_cmd::ReportArgs),
    /// List high-usage incidents
    Incidents(incidents_cmd::IncidentsArgs),
    /// Show the processes, command lines and process trees of one incident
    Show(incidents_cmd::ShowArgs),
    /// Run the monitor at every logon (Task Scheduler), without a console window.
    /// Needs an elevated terminal, e.g. `sudo cpumon install`
    Install(install_cmd::InstallArgs),
    /// Stop and remove the logon task (needs an elevated terminal)
    Uninstall,
    /// Show whether the logon task is installed and running
    Status,
    /// Database maintenance
    #[command(subcommand)]
    Db(DbCommand),
    /// Show live CPU/GPU totals and the busiest processes (diagnostic)
    Top(top_cmd::TopArgs),
    /// Show live GPU usage per adapter, engine and process (diagnostic)
    Gpu(gpu_cmd::GpuArgs),
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Print the database path
    Path,
    /// Delete incidents that ended more than the given time ago, and processes no
    /// longer referenced
    Prune {
        /// Age of data to delete
        #[arg(long, default_value = "30d", value_parser = parse_duration)]
        older_than: Duration,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db = match cli.db {
        Some(path) => path,
        None => default_db_path().context("LOCALAPPDATA is not set; pass --db")?,
    };

    match cli.command {
        Command::Db(DbCommand::Path) => {
            println!("{}", db.display());
            Ok(())
        }
        Command::Top(args) => top_cmd::run(&args),
        Command::Gpu(args) => gpu_cmd::run(&args),
        Command::Monitor(args) => monitor_cmd::run(&args, &db),
        Command::Install(args) => install_cmd::install(&args, &db),
        Command::Uninstall => install_cmd::uninstall(),
        Command::Status => install_cmd::status(&db),
        Command::Report(args) => report_cmd::run(&args, &db),
        Command::Incidents(args) => incidents_cmd::run_list(&args, &db),
        Command::Show(args) => incidents_cmd::run_show(&args, &db),
        Command::Db(DbCommand::Prune { older_than }) => prune(older_than, &db),
    }
}

fn prune(older_than: Duration, db: &std::path::Path) -> Result<()> {
    let Some(store) = output::open_existing(db)? else {
        return Ok(());
    };
    let before = output::cutoff_utc(older_than);
    let stats = store.prune(&before)?;
    println!(
        "Deleted {} incident(s) that ended before {} and {} process record(s) no longer referenced.",
        stats.incidents,
        cpumon_core::time::utc_to_local(&before),
        stats.processes,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn monitor_defaults_match_design() {
        let cli = Cli::try_parse_from(["cpumon", "monitor"]).unwrap();
        let Command::Monitor(args) = cli.command else {
            panic!("expected monitor");
        };
        assert_eq!(args.interval, Duration::from_secs(2));
        assert_eq!((args.cpu, args.gpu, args.proc_core), (80, 80, 90));
        assert_eq!((args.proc_cpu, args.proc_gpu), (10, 10));
        assert_eq!((args.top, args.sustain), (5, 2));
    }

    #[test]
    fn rejects_out_of_range_threshold() {
        assert!(Cli::try_parse_from(["cpumon", "monitor", "--cpu", "101"]).is_err());
        assert!(Cli::try_parse_from(["cpumon", "monitor", "--sustain", "0"]).is_err());
        assert!(Cli::try_parse_from(["cpumon", "monitor", "--proc-core", "0"]).is_err());
    }

    #[test]
    fn proc_core_may_exceed_one_core() {
        let cli = Cli::try_parse_from(["cpumon", "monitor", "--proc-core", "400"]).unwrap();
        let Command::Monitor(args) = cli.command else {
            panic!("expected monitor");
        };
        assert_eq!(args.proc_core, 400);
    }

    #[test]
    fn report_defaults_match_design() {
        let cli = Cli::try_parse_from(["cpumon", "report"]).unwrap();
        let Command::Report(args) = cli.command else {
            panic!("expected report");
        };
        assert_eq!(args.since, Duration::from_secs(7 * 86_400));
        assert_eq!(args.by, report_cmd::GroupBy::Name);
        assert_eq!(args.metric, report_cmd::Metric::Cpu);
        assert_eq!(args.top, 20);
    }

    #[test]
    fn parses_show_and_prune() {
        let cli = Cli::try_parse_from(["cpumon", "show", "12", "--samples"]).unwrap();
        let Command::Show(args) = cli.command else {
            panic!("expected show");
        };
        assert_eq!((args.incident_id, args.samples), (12, true));
        let cli = Cli::try_parse_from(["cpumon", "db", "prune"]).unwrap();
        let Command::Db(DbCommand::Prune { older_than }) = cli.command else {
            panic!("expected prune");
        };
        assert_eq!(older_than, Duration::from_secs(30 * 86_400));
    }

    #[test]
    fn install_accepts_monitor_options() {
        let cli = Cli::try_parse_from(["cpumon", "install", "--elevated", "--cpu", "90"]).unwrap();
        let Command::Install(args) = cli.command else {
            panic!("expected install");
        };
        assert!(args.elevated && !args.no_start);
        assert_eq!(args.monitor.cpu, 90);
    }

    #[test]
    fn rejects_bad_duration() {
        assert!(Cli::try_parse_from(["cpumon", "report", "--since", "7"]).is_err());
    }
}
