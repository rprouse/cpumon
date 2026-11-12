//! `cpumon`: monitor Windows CPU/GPU usage and record the processes responsible.

mod gpu_cmd;
mod monitor_cmd;
mod top_cmd;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
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
    Report(ReportArgs),
    /// List high-usage incidents
    Incidents(IncidentsArgs),
    /// Show samples and process trees for one incident
    Show {
        /// Incident id (from `cpumon incidents`)
        incident_id: i64,
    },
    /// Database maintenance
    #[command(subcommand)]
    Db(DbCommand),
    /// Show live CPU/GPU totals and the busiest processes (diagnostic)
    Top(top_cmd::TopArgs),
    /// Show live GPU usage per adapter, engine and process (diagnostic)
    Gpu(gpu_cmd::GpuArgs),
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// How far back to look
    #[arg(long, default_value = "7d", value_parser = parse_duration)]
    since: Duration,

    /// How to group offenders
    #[arg(long, value_enum, default_value_t = GroupBy::Name)]
    by: GroupBy,

    /// Which usage to rank by
    #[arg(long, value_enum, default_value_t = Metric::Cpu)]
    metric: Metric,

    /// Number of rows to show
    #[arg(long, default_value_t = 20)]
    top: usize,
}

#[derive(Debug, Args)]
struct IncidentsArgs {
    /// How far back to look
    #[arg(long, default_value = "24h", value_parser = parse_duration)]
    since: Duration,
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Print the database path
    Path,
    /// Delete data older than the given age
    Prune {
        /// Age of data to delete
        #[arg(long, default_value = "30d", value_parser = parse_duration)]
        older_than: Duration,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GroupBy {
    /// Process image name
    Name,
    /// Root ancestor process
    Root,
    /// Exact command line
    Cmdline,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Metric {
    Cpu,
    Gpu,
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
        Command::Report(_) => not_implemented("report"),
        Command::Incidents(_) => not_implemented("incidents"),
        Command::Show { .. } => not_implemented("show"),
        Command::Db(DbCommand::Prune { .. }) => not_implemented("db prune"),
    }
}

fn not_implemented(command: &str) -> Result<()> {
    bail!("`cpumon {command}` is not implemented yet")
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
    fn rejects_bad_duration() {
        assert!(Cli::try_parse_from(["cpumon", "report", "--since", "7"]).is_err());
    }
}
