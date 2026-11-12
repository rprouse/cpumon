//! The worst-offenders report: per-process usage grouped by name, application root or
//! command line.

use std::collections::{HashMap, HashSet};

use crate::store::{ProcessInfo, ProcessUsage};

/// How report rows are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// Process image name, e.g. `msedgewebview2.exe`.
    Name,
    /// The application a process belongs to: its highest ancestor below a shell or
    /// system host (see [`app_root`]).
    Root,
    /// Exact command line.
    CommandLine,
}

/// What the report ranks by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cpu,
    Gpu,
}

/// One group of processes.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub label: String,
    /// CPU time in core-seconds.
    pub cpu_seconds: f64,
    /// GPU busy time in seconds.
    pub gpu_seconds: f64,
    pub peak_cpu_core: f64,
    pub peak_gpu: f64,
    /// Distinct incidents the group appeared in.
    pub incidents: usize,
    /// Distinct process instances in the group.
    pub processes: usize,
    pub last_seen_utc: String,
}

/// Processes that start applications rather than being one. Grouping by root stops
/// below these, so a browser's helper processes group under the browser rather than
/// under `explorer.exe` or `services.exe`.
const HOSTS: &[&str] = &[
    "system",
    "registry",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "svchost.exe",
    "userinit.exe",
    "explorer.exe",
    "sihost.exe",
    "taskhostw.exe",
];

fn is_host(name: &str) -> bool {
    HOSTS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// The application a process belongs to: walking up the recorded ancestry, the last
/// process before a host (shell or service host) or the end of the chain. A host is its
/// own root.
pub fn app_root<'a>(
    process: &'a ProcessInfo,
    processes: &'a HashMap<i64, ProcessInfo>,
) -> &'a ProcessInfo {
    let mut current = process;
    for _ in 0..64 {
        if is_host(&current.name) {
            break;
        }
        match current.parent_process_id.and_then(|id| processes.get(&id)) {
            Some(parent) if !is_host(&parent.name) => current = parent,
            _ => break,
        }
    }
    current
}

fn label(by: GroupBy, process: &ProcessInfo, processes: &HashMap<i64, ProcessInfo>) -> String {
    match by {
        GroupBy::Name => process.name.clone(),
        GroupBy::Root => app_root(process, processes).name.clone(),
        GroupBy::CommandLine => match process.command_line.as_deref() {
            Some(cmd) if !cmd.is_empty() => cmd.to_owned(),
            _ => format!("{} (command line unavailable)", process.name),
        },
    }
}

#[derive(Default)]
struct Group {
    row: Option<ReportRow>,
    incidents: HashSet<i64>,
    processes: HashSet<i64>,
}

/// Groups usage rows and returns the `top` groups by `metric`, highest first. Groups
/// with no usage of that metric are left out.
pub fn build_report(
    usage: &[ProcessUsage],
    processes: &HashMap<i64, ProcessInfo>,
    by: GroupBy,
    metric: Metric,
    top: usize,
) -> Vec<ReportRow> {
    let mut groups: HashMap<String, Group> = HashMap::new();
    for u in usage {
        let Some(process) = processes.get(&u.process_id) else {
            continue;
        };
        let label = label(by, process, processes);
        // Windows names are case-insensitive.
        let key = if by == GroupBy::CommandLine {
            label.clone()
        } else {
            label.to_lowercase()
        };
        let group = groups.entry(key).or_default();
        group.incidents.insert(u.incident_id);
        group.processes.insert(u.process_id);
        let row = group.row.get_or_insert_with(|| ReportRow {
            label,
            cpu_seconds: 0.0,
            gpu_seconds: 0.0,
            peak_cpu_core: 0.0,
            peak_gpu: 0.0,
            incidents: 0,
            processes: 0,
            last_seen_utc: String::new(),
        });
        row.cpu_seconds += u.cpu_seconds;
        row.gpu_seconds += u.gpu_seconds;
        row.peak_cpu_core = row.peak_cpu_core.max(u.peak_cpu_core);
        row.peak_gpu = row.peak_gpu.max(u.peak_gpu);
        if u.last_seen_utc > row.last_seen_utc {
            row.last_seen_utc.clone_from(&u.last_seen_utc);
        }
    }

    let value = |r: &ReportRow| match metric {
        Metric::Cpu => r.cpu_seconds,
        Metric::Gpu => r.gpu_seconds,
    };
    let mut rows: Vec<ReportRow> = groups
        .into_values()
        .filter_map(|g| {
            let mut row = g.row?;
            row.incidents = g.incidents.len();
            row.processes = g.processes.len();
            Some(row)
        })
        .filter(|r| value(r) > 0.0)
        .collect();
    rows.sort_by(|a, b| {
        value(b)
            .total_cmp(&value(a))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows.truncate(top);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(id: i64, name: &str, parent: Option<i64>, cmd: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            id,
            pid: id as u32 * 4,
            create_time_utc: String::new(),
            name: name.into(),
            image_path: None,
            command_line: cmd.map(Into::into),
            user_name: None,
            info_error: None,
            parent_process_id: parent,
        }
    }

    fn usage(process_id: i64, incident_id: i64, cpu: f64, gpu: f64, ts: &str) -> ProcessUsage {
        ProcessUsage {
            process_id,
            incident_id,
            samples: 1,
            cpu_seconds: cpu,
            gpu_seconds: gpu,
            peak_cpu_core: cpu * 10.0,
            peak_gpu: gpu * 10.0,
            last_seen_utc: ts.into(),
        }
    }

    /// services <- svchost <- Terminal <- shell <- tool; explorer <- Browser <- 2 helpers
    fn processes() -> HashMap<i64, ProcessInfo> {
        [
            proc(1, "services.exe", None, None),
            proc(2, "svchost.exe", Some(1), None),
            proc(3, "WindowsTerminal.exe", Some(2), None),
            proc(4, "pwsh.exe", Some(3), Some("pwsh")),
            proc(5, "tool.exe", Some(4), Some("tool --busy")),
            proc(10, "explorer.exe", None, None),
            proc(11, "browser.exe", Some(10), None),
            proc(12, "helper.exe", Some(11), Some("helper --type=gpu")),
            proc(13, "HELPER.EXE", Some(11), Some("helper --type=renderer")),
            proc(20, "orphan.exe", Some(999), None),
        ]
        .into_iter()
        .map(|p| (p.id, p))
        .collect()
    }

    #[test]
    fn finds_application_root_below_hosts() {
        let p = processes();
        let root = |id| app_root(&p[&id], &p).name.as_str();
        assert_eq!(root(5), "WindowsTerminal.exe");
        assert_eq!(root(12), "browser.exe");
        assert_eq!(root(11), "browser.exe");
        assert_eq!(root(2), "svchost.exe"); // hosts are their own root
        assert_eq!(root(20), "orphan.exe"); // parent not recorded
    }

    #[test]
    fn root_walk_survives_cycles() {
        let p: HashMap<_, _> = [
            proc(1, "a.exe", Some(2), None),
            proc(2, "b.exe", Some(1), None),
        ]
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
        let _ = app_root(&p[&1], &p);
    }

    #[test]
    fn groups_by_name_case_insensitively() {
        let p = processes();
        let usage = [
            usage(12, 1, 1.0, 5.0, "2026-01-01T00:00:01.0000000Z"),
            usage(13, 2, 2.0, 0.0, "2026-01-01T00:00:09.0000000Z"),
            usage(5, 1, 4.0, 0.0, "2026-01-01T00:00:02.0000000Z"),
        ];
        let rows = build_report(&usage, &p, GroupBy::Name, Metric::Cpu, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "tool.exe");
        let helper = &rows[1];
        assert_eq!(helper.cpu_seconds, 3.0);
        assert_eq!(helper.gpu_seconds, 5.0);
        assert_eq!(helper.peak_cpu_core, 20.0);
        assert_eq!((helper.incidents, helper.processes), (2, 2));
        assert_eq!(helper.last_seen_utc, "2026-01-01T00:00:09.0000000Z");
    }

    #[test]
    fn groups_by_root_and_command_line() {
        let p = processes();
        let usage = [
            usage(12, 1, 1.0, 5.0, "t"),
            usage(13, 1, 2.0, 0.0, "t"),
            usage(20, 1, 1.5, 0.0, "t"),
        ];
        let by_root = build_report(&usage, &p, GroupBy::Root, Metric::Cpu, 10);
        let labels: Vec<_> = by_root.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["browser.exe", "orphan.exe"]);
        assert_eq!(by_root[0].incidents, 1);

        let by_cmd = build_report(&usage, &p, GroupBy::CommandLine, Metric::Cpu, 10);
        let labels: Vec<_> = by_cmd.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "helper --type=renderer",
                "orphan.exe (command line unavailable)",
                "helper --type=gpu"
            ]
        );
    }

    #[test]
    fn ranks_by_metric_drops_zero_rows_and_truncates() {
        let p = processes();
        let usage = [
            usage(12, 1, 1.0, 5.0, "t"),
            usage(5, 1, 4.0, 0.0, "t"),
            usage(11, 1, 0.5, 7.0, "t"),
        ];
        let gpu = build_report(&usage, &p, GroupBy::Name, Metric::Gpu, 10);
        let labels: Vec<_> = gpu.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["browser.exe", "helper.exe"]);
        assert_eq!(
            build_report(&usage, &p, GroupBy::Name, Metric::Cpu, 1).len(),
            1
        );
        assert!(build_report(&[], &p, GroupBy::Name, Metric::Cpu, 10).is_empty());
    }
}
