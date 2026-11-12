# cpumon design

## Problem

The machine regularly shows high CPU and/or GPU usage (for example 100% GPU in the
PowerToys dock), but by the time Task Manager is checked nothing obvious is using it.
`cpumon` is a lightweight, Windows-only CLI that samples usage every few seconds, and
when usage is high it records the offending processes, with their command lines and
process ancestry, to a local SQLite database. A report command then shows the worst
offenders over time.

## Goals

- Very low overhead: the monitor must never become a CPU/GPU offender itself.
- Capture short-lived and intermittent spikes that are missed by watching Task Manager.
- Record enough context to identify the real culprit: command line, image path, user,
  and the parent chain up to the root process, even after those processes exit.
- Break GPU usage down per engine and per adapter so discrepancies between tools can
  be explained.
- Single self-contained executable, no runtime or installer required.

## Non-goals (v1)

- Cross-platform support.
- ETW-based tracing (possible phase 2 if polling misses spikes).
- A GUI. Reporting starts as CLI commands over the same database.
- Auto-start / service install (possible later `cpumon install` via Task Scheduler).

## Background: why the numbers disagree

- GPU utilisation is exposed per **engine** (3D, Copy, VideoDecode, VideoEncode,
  Compute, ...) per **adapter** per **process**, via the PDH counter set
  `GPU Engine(*)\Utilization Percentage`. Instance names look like
  `pid_1234_luid_0x00000000_0x0000C2A1_phys_0_eng_3_engtype_3D`.
- Task Manager's per-process GPU column is the **max engine** for that process, and
  the overall GPU figure is the **max engine** across the adapter.
- A tool that **sums** engines, watches a different adapter (iGPU vs dGPU), or
  includes work attributed to `dwm.exe`, `System`, or processes in other sessions can
  report 100% while Task Manager shows nothing large. How the PowerToys dock computes
  its figure is not yet verified; recording both max-engine and sum lets us compare.
- CPU in Task Manager is a percentage of the whole machine. A single thread pegging one
  core on a 16-core machine shows as about 6%, so `cpumon` records both
  "% of machine" and "% of one core".

## Technology

| Concern | Choice |
|---|---|
| Language | Rust (stable, `x86_64-pc-windows-msvc`) |
| Windows APIs | [`windows`](https://crates.io/crates/windows) crate (`Wdk` + `Win32` features) |
| Database | [`rusqlite`](https://crates.io/crates/rusqlite) with the `bundled` feature |
| CLI parsing | [`clap`](https://crates.io/crates/clap) (derive) |
| Console tables | [`comfy-table`](https://crates.io/crates/comfy-table) |

Rust was chosen for C-level overhead (no GC, no runtime threads, a few MB of memory),
first-party Windows API bindings, and safe handling of variable-length native buffers.
Build prerequisites: Rust via rustup and the Visual Studio Build Tools (VC++ tools +
Windows SDK).

## Workspace layout

```
Cargo.toml                 workspace
crates/
  cpumon-core/             sampling, process info, storage, queries (library)
    src/
      snapshot.rs          NtQuerySystemInformation process snapshot
      cpu.rs               CPU deltas between snapshots
      gpu.rs               PDH GPU Engine counters + instance-name parsing
      totals.rs            machine-wide CPU/GPU totals
      procinfo.rs          command line, image path, user, session (enrichment)
      tree.rs              ancestry resolution with PID-reuse protection
      incident.rs          threshold / sustain state machine
      store/               SQLite schema, migrations, inserts, report queries
  cpumon/                  the `cpumon` binary (CLI, console output)
docs/
```

## Sampling loop

Runs every `--interval` (default 2s). No WMI and no per-process handle opening on the
hot path.

1. **Process snapshot**: a single `NtQuerySystemInformation(SystemProcessInformation)`
   call returns every process with PID, parent PID, create time, kernel/user CPU time,
   image name, working set, and thread count.
2. **Per-process CPU**: delta of kernel+user time between consecutive snapshots,
   divided by elapsed wall time (and by logical core count for "% of machine").
   Processes are keyed by `(pid, create_time)` so PID reuse is never confused with the
   original process.
3. **Per-process GPU**: a persistent PDH query with the wildcard counter
   `\GPU Engine(*)\Utilization Percentage`, read with `PdhGetFormattedCounterArrayW`.
   Instance names are parsed into PID, adapter LUID, and engine type.
4. **Machine totals**: CPU from `\Processor Information(_Total)\% Processor Utility`;
   GPU as max engine per adapter (Task Manager style) plus the sum across engines.
5. **Thresholds**: if total CPU or GPU exceeds its threshold for `--sustain`
   consecutive samples, open (or continue) an **incident**. Record every process over
   the per-process threshold, or at least the top `--top` processes.
6. **Enrichment** (once per `(pid, create_time)`, cached): open the process with
   `PROCESS_QUERY_LIMITED_INFORMATION` and read the command line via
   `NtQueryInformationProcess(ProcessCommandLineInformation)`, the full image path via
   `QueryFullProcessImageNameW`, plus the user and session. Failures (for example
   access denied for elevated or protected processes when not running as admin) are
   recorded rather than treated as errors.
7. **Ancestry**: walk parent PIDs through the current snapshot. A candidate parent is
   only accepted if its create time is earlier than the child's; otherwise the PID was
   reused and the chain stops. Every ancestor is persisted so the tree survives after
   processes exit.
8. The incident closes when totals drop below their thresholds.

The previous snapshot is kept in memory every interval, so a per-process CPU delta is
always available the moment a spike is detected.

## Storage

Location: `%LOCALAPPDATA%\cpumon\cpumon.db` (overridable with `--db`). WAL mode,
schema versioned through `PRAGMA user_version`. Retention pruning (default 30 days).
All timestamps are stored in UTC.

```sql
CREATE TABLE incident (
    id            INTEGER PRIMARY KEY,
    started_utc   TEXT NOT NULL,
    ended_utc     TEXT,
    trigger       TEXT NOT NULL,          -- 'cpu' | 'gpu' | 'both'
    peak_cpu      REAL,
    peak_gpu      REAL
);

CREATE TABLE sample (
    id              INTEGER PRIMARY KEY,
    incident_id     INTEGER NOT NULL REFERENCES incident(id),
    ts_utc          TEXT NOT NULL,
    cpu_total       REAL NOT NULL,
    gpu_max_engine  REAL NOT NULL,
    gpu_sum         REAL NOT NULL,
    gpu_detail_json TEXT                  -- per adapter/engine breakdown
);

CREATE TABLE process (
    id                INTEGER PRIMARY KEY,
    pid               INTEGER NOT NULL,
    create_time_utc   TEXT NOT NULL,
    name              TEXT NOT NULL,
    image_path        TEXT,
    command_line      TEXT,
    info_error        TEXT,               -- e.g. 'access denied'
    user              TEXT,
    session_id        INTEGER,
    parent_process_id INTEGER REFERENCES process(id),
    first_seen_utc    TEXT NOT NULL,
    last_seen_utc     TEXT NOT NULL,
    UNIQUE (pid, create_time_utc)
);

CREATE TABLE process_sample (
    sample_id         INTEGER NOT NULL REFERENCES sample(id),
    process_id        INTEGER NOT NULL REFERENCES process(id),
    cpu_pct_machine   REAL NOT NULL,
    cpu_pct_core      REAL NOT NULL,
    gpu_pct           REAL NOT NULL,
    gpu_engine        TEXT,
    gpu_adapter       TEXT,
    working_set_bytes INTEGER,
    PRIMARY KEY (sample_id, process_id)
);
```

The root process of any offender can be found with a recursive CTE over
`process.parent_process_id`.

## CLI

```
cpumon monitor   [--interval 2s] [--cpu 80] [--gpu 80]
                 [--proc-cpu 10] [--proc-gpu 10] [--top 5] [--sustain 2] [--db PATH]
cpumon report    [--since 7d] [--by name|root|cmdline] [--metric cpu|gpu] [--top 20]
cpumon incidents [--since 24h]
cpumon show <incident-id>        # samples and process trees for one incident
cpumon db path | prune [--older-than 30d]
```

Example monitor output:

```
14:02:11  GPU 100% (3D, NVIDIA GeForce ...)  CPU 23%
  gpu 87%  3D   msedgewebview2.exe (18244)  <- Teams.exe <- explorer.exe
           "...msedgewebview2.exe --type=gpu-process ..."
```

## Reporting

`cpumon report` ranks offenders by accumulated CPU-seconds or GPU-seconds while above
threshold, number of incidents, and peak usage, grouped by process name, root ancestor,
or exact command line. A GUI or HTML dashboard can be layered on the same
`cpumon-core` queries later.

## Privileges

Runs unelevated by default. Some command lines (elevated or protected processes) will
be unavailable and are recorded as such. Running from an elevated terminal, or later as
a scheduled task at logon with highest privileges, captures everything.

## Roadmap

1. ~~Workspace scaffold, build verified.~~ Done: CLI surface defined, `db path` works.
2. GPU engine sampler (PDH + instance-name parsing) with a debug print command, to
   investigate the GPU discrepancy first.
3. Process snapshot, CPU deltas, machine totals.
4. Incident state machine and console logging.
5. SQLite storage, enrichment (command line, path, user), ancestry.
6. `report`, `incidents`, `show`, `db` commands.
7. Later: `install` scheduled task, ETW option, GUI/dashboard.
