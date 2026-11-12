//! `cpumon install`, `uninstall` and `status`: run the monitor at logon via Task
//! Scheduler.

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Args;
use cpumon_core::procinfo::current_user;
use cpumon_core::task::{self, TASK_NAME, TaskSpec, TaskState};

use crate::monitor_cmd::MonitorArgs;

/// HRESULT for "Access is denied."
const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Run with full administrator rights, so command lines of elevated and system
    /// processes are captured
    #[arg(long)]
    pub elevated: bool,

    /// Only register the task; don't start it until the next logon
    #[arg(long)]
    pub no_start: bool,

    #[command(flatten)]
    pub monitor: MonitorArgs,
}

pub fn install(args: &InstallArgs, db: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("finding cpumon.exe")?;
    let user = current_user().context("finding the current user")?;
    let db = std::path::absolute(db).context("resolving the database path")?;
    let mut arguments = format!("--db \"{}\" monitor", db.display());
    for arg in args.monitor.to_args() {
        arguments.push(' ');
        arguments.push_str(&arg);
    }
    let spec = TaskSpec {
        user,
        command: exe.display().to_string(),
        arguments,
        elevated: args.elevated,
    };

    let state = match task::install(&spec, !args.no_start) {
        Ok(state) => state,
        Err(cpumon_core::Error::Windows(e)) if e.code().0 == E_ACCESSDENIED => {
            bail!(
                "Task Scheduler refused the task (access denied). Installing needs an \
                 elevated terminal, e.g. `sudo cpumon install`."
            );
        }
        Err(e) => return Err(e).context("registering the scheduled task"),
    };

    println!(
        "Installed scheduled task \"{TASK_NAME}\" for {}{}.",
        spec.user,
        if args.elevated { " (elevated)" } else { "" }
    );
    println!("  runs:  \"{}\" {}", spec.command, spec.arguments);
    println!("  when:  at logon, without a window; restarted if it fails");
    println!("  state: {state}");
    if exe.components().any(|c| c.as_os_str() == "target") {
        println!(
            "Note: this is a build output; the task stops working if `cargo clean` removes it. \
             Copy cpumon.exe somewhere permanent and install from there."
        );
    }
    println!("Remove with `sudo cpumon uninstall`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let removed = match task::uninstall() {
        Ok(removed) => removed,
        Err(cpumon_core::Error::Windows(e)) if e.code().0 == E_ACCESSDENIED => {
            bail!("access denied. Run `sudo cpumon uninstall` from an elevated terminal.");
        }
        Err(e) => return Err(e).context("removing the scheduled task"),
    };
    if removed {
        println!("Stopped and removed scheduled task \"{TASK_NAME}\". The database is kept.");
    } else {
        println!("Scheduled task \"{TASK_NAME}\" is not installed.");
    }
    Ok(())
}

pub fn status(db: &Path) -> Result<()> {
    match task::status().context("querying Task Scheduler")? {
        Some(state) => {
            println!("Scheduled task \"{TASK_NAME}\": {state}.");
            if state != TaskState::Running {
                println!("It starts at the next logon (or run `cpumon install` again).");
            }
        }
        None => println!("Scheduled task \"{TASK_NAME}\" is not installed (`cpumon install`)."),
    }
    println!("Database: {}", db.display());
    Ok(())
}
