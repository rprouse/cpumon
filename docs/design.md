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
      error.rs             Error / Result types
      snapshot.rs          NtQuerySystemInformation process snapshot
      cpu.rs               CPU deltas between snapshots
      pdh.rs               reusable wildcard PDH counter query
      gpu.rs               GPU Engine instance-name parsing, sampler, aggregation
      adapters.rs          DXGI adapter names for LUIDs
      totals.rs            machine-wide CPU total (% Processor Utility)
      sampler.rs           one call per interval: totals + per-process CPU/GPU
      procinfo.rs          command line, image path, user, session (enrichment)
      tree.rs              ancestry resolution with PID-reuse protection
      incident.rs          triggers and the sustain state machine
      offenders.rs         which processes to record during an incident
      report.rs            worst-offender grouping (name, application root, command line)
      time.rs              FILETIME <-> UTC strings, local display time
      recorder.rs          writes incidents, samples, offenders and ancestry
      console.rs           Ctrl+C / close handling
      stall.rs             probe thread measuring wake-up lateness
      hung.rs              windows that stopped responding
      task.rs              logon task (Task Scheduler COM API)
      store/               SQLite schema, migrations, inserts, report queries, pruning
  cpumon/                  the CLI (library) and two executables
    src/
      lib.rs               argument parsing and dispatch
      main.rs              cpumon.exe (console)
      bin/cpumonw.rs       cpumonw.exe (no console window, for the logon task)
      monitor_cmd.rs       monitor
      report_cmd.rs        report
      incidents_cmd.rs     incidents, show
      install_cmd.rs       install, uninstall, status
      output.rs            shared table and number formatting (comfy-table)
      top_cmd.rs, gpu_cmd.rs  live diagnostics
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
   Instance names are parsed into PID, adapter LUID, and engine type. Machines without
   a WDDM GPU driver (some VMs, CI runners) have no such counters; GPU then reads 0.
4. **Machine totals**: CPU from `\Processor Information(_Total)\% Processor Utility`;
   GPU as max engine per adapter (Task Manager style) plus the sum across engines.
5. **Responsiveness** (see below): stall time from a probe thread, plus DPC/interrupt
   time, CPU speed, temperature and memory from the same PDH query as the CPU total,
   and a scan for windows that stopped responding.
6. **Triggers**: a sample is high when any of these fire:
   - **cpu**: total CPU >= `--cpu` (default 80%);
   - **gpu**: busiest GPU engine >= `--gpu` (default 80%);
   - **core**: any single process >= `--proc-core` percent of one core (default 90%).
     One pegged core on a 16-thread machine is only ~6% of the total, so a runaway
     thread would never reach the total CPU threshold.
   - **stall**: a normal-priority thread was kept waiting >= `--stall` (default 500 ms);
   - **dpc**: one logical CPU spent >= `--dpc` percent (default 30%) in DPCs and
     interrupts;
   - **hung**: a visible window is not responding.

   The load triggers (cpu, gpu, core) open an **incident** after `--sustain`
   consecutive high samples. The responsiveness triggers (stall, dpc, hung) open one on
   a single sample, because the freezes they catch are often shorter than two
   intervals. While an incident is open,
   every sample records the offenders: each process over `--proc-cpu` (% of machine),
   `--proc-core` or `--proc-gpu`, plus the top `--top` CPU and GPU users. Offenders
   are ranked by their share of one core (capped at 100) or busiest GPU engine,
   whichever is higher, then by raw CPU.
7. **Enrichment** (once per `(pid, create_time)`, cached): open the process with
   `PROCESS_QUERY_LIMITED_INFORMATION` and read the command line via
   `NtQueryInformationProcess(ProcessCommandLineInformation)`, the full image path via
   `QueryFullProcessImageNameW`, plus the user and session. Failures (for example
   access denied for elevated or protected processes when not running as admin) are
   recorded rather than treated as errors.
8. **Ancestry**: walk parent PIDs through the current snapshot. A candidate parent is
   only accepted if its create time is earlier than the child's; otherwise the PID was
   reused and the chain stops. Every ancestor is persisted so the tree survives after
   processes exit.
9. The incident closes after `--sustain` consecutive normal samples, so brief dips
   neither end nor split it.

The previous snapshot is kept in memory every interval, so a per-process CPU delta is
always available the moment a spike is detected.

### Responsiveness signals

The machine can freeze without high CPU or GPU percentages, for example when a driver
holds the CPUs, the system is paging, or the desktop GPU is saturated. These signals
catch that:

- **Stall** (`stall.rs`): a thread wakes every 250 ms and records how late it woke;
  each sample takes the worst lateness since the previous one. Normal-priority threads
  are all delayed when the machine is starved, so this measures what the user feels,
  and its short period catches freezes far shorter than the sampling interval. Idle
  baseline is 12-16 ms (timer granularity). Lateness over 30 s is taken as
  sleep/hibernate and ignored.
- **DPC and interrupt time** (`totals.rs`): `\Processor Information(*)\% DPC Time` and
  `% Interrupt Time`, both for the machine and for the worst single logical CPU, which
  is where a misbehaving driver (audio, USB, Wi-Fi, webcam, GPU) shows. Idle baseline
  on the development laptop: worst CPU 3-13%.
- **CPU speed** (`% Processor Performance`, percent of base frequency; well under 100
  while busy suggests thermal or power throttling) and **temperature** (hottest ACPI
  thermal zone; missing on many laptops, including the development one). Recorded, not
  triggers.
- **Memory**: available MB, committed % and hard-fault reads (`Pages Input/sec`).
  Recorded, not triggers.
- **Hung windows** (`hung.rs`): `EnumWindows` + `IsHungAppWindow` each sample (about
  0.5 ms). Only visible windows count, since hidden windows of background apps can sit
  unpumped harmlessly. Windows hides a hung window behind a "ghost" (class `Ghost`,
  owned by `dwm.exe`, which itself reports as hung); ghosts are excluded but matched by
  title to the hidden original. The owning process is recorded with its command line
  and ancestry like any offender. This only works in the user's session, which is why
  the logon task runs interactively (below).

## Storage

Location: `%LOCALAPPDATA%\cpumon\cpumon.db` (overridable with `--db`). WAL mode,
`synchronous=NORMAL`, foreign keys on. The schema is versioned through
`PRAGMA user_version`; migrations live in `crates/cpumon-core/src/store/schema.rs`.
`cpumon db prune` (default `--older-than 30d`) deletes incidents that ended before the
cutoff, with their samples, and then any process rows no longer referenced by a sample
(as an offender, a hung window's owner, or an ancestor of either). Incidents in progress
are never pruned. `monitor` does the same itself with `--retain` (default 30d) at
startup and then once a day, between incidents, so an unattended logon task keeps the
database bounded.

Timestamps are fixed-width UTC strings with 100 ns precision, such as
`2026-11-12T14:05:52.1234567Z`. They sort chronologically and work with SQLite's date
functions. Process identity is `(pid, create_time_utc)`.

| Table | Contents |
|---|---|
| `adapter` | LUID to adapter description, refreshed at startup |
| `incident` | start/end, triggers fired (e.g. `cpu+stall`), sample count, peak CPU/GPU/per-process core, peak stall and DPC, most hung windows at once (v3) |
| `sample` | one row per sample during an incident: totals, busiest process, triggers, seconds covered (`interval_s`, v2), stall, DPC/interrupt, CPU speed, temperature, memory, hung window count (v3) |
| `sample_gpu_engine` | busy engines (>= 0.1%) per sample, per adapter |
| `sample_hung_window` | windows not responding per sample: process, title, whether in the foreground (schema v3) |
| `process` | one row per process instance: name, image path, command line, user, session, parent PID, link to parent row, `info_error`, first/last seen |
| `process_sample` | each offender in each sample: CPU (% machine and % core), GPU %, engine, adapter, working set |

Each sample is written in one transaction. The incident row is updated every sample,
so a crash leaves a usable record; incidents left open are closed at their last
sample on the next start. `Ctrl+C`, `Ctrl+Break` and closing the console end the
incident cleanly.

Details (command line, path, user) are read once per process, when it is first
recorded, along with any of its ancestors not yet stored. Each ancestor gets its own
`process` row linked through `parent_process_id`, so the chain survives after the
processes exit. The root of any offender can be found with a recursive CTE:

```sql
WITH RECURSIVE chain(id, name, parent, depth) AS (
    SELECT id, name, parent_process_id, 0 FROM process WHERE id = ?
    UNION ALL
    SELECT p.id, p.name, p.parent_process_id, depth + 1
    FROM process p JOIN chain c ON p.id = c.parent)
SELECT name FROM chain ORDER BY depth DESC LIMIT 1;
```

## CLI

```
cpumon monitor   [--interval 2s] [--cpu 80] [--gpu 80] [--proc-core 90]
                 [--proc-cpu 10] [--proc-gpu 10] [--top 5] [--stall 500ms] [--dpc 30]
                 [--sustain 2] [--retain 30d] [--db PATH]
cpumon report    [--since 7d] [--by name|root|cmdline] [--metric cpu|gpu] [--top 20]
cpumon incidents [--since 24h]
cpumon show <incident-id> [--samples]   # processes, command lines, trees; --samples adds the timeline
cpumon db path | prune [--older-than 30d]
cpumon install   [--elevated] [--no-start] [monitor options]   # run monitor at logon
cpumon uninstall | status
cpumon top       [--interval 2s] [--count N] [--top 5]    # live CPU/GPU diagnostic
cpumon gpu       [--interval 1s] [--count N] [--top 10]   # live GPU engine diagnostic
```

Example monitor output:

```
14:02:11  GPU 100% (3D, NVIDIA GeForce ...)  CPU 23%
  gpu 87%  3D   msedgewebview2.exe (18244)  <- Teams.exe <- explorer.exe
           "...msedgewebview2.exe --type=gpu-process ..."
```

## Measured overhead

Release build, development laptop (16 logical CPUs, Intel UHD + NVIDIA RTX 3050 Ti,
~1,280 GPU Engine counter instances), `cpumon gpu`:

| | Cost |
|---|---|
| Startup (mostly DXGI adapter enumeration loading GPU user-mode drivers) | ~230 ms CPU, once |
| Per sample (PDH ~2 ms + process snapshot ~7 ms wall) | ~12 ms CPU |
| At the default 2 s interval | ~0.6% of one core, ~0.04% of the machine |
| Peak working set | ~16 MB (mostly the GPU driver DLLs loaded by DXGI) |

Possible later optimisations: name adapters via `D3DKMTEnumAdapters2` instead of DXGI
(avoids loading user-mode drivers), and only take the process snapshot every sample
while needed for CPU deltas.

## Reporting

Only time during incidents is recorded, so all figures are "while the machine was
busy". Each sample stores the seconds it covers, so usage is accumulated as:

- **CPU time** in core-seconds: `cpu_pct_core / 100 x seconds` (one core fully busy
  for 1 s = 1 s).
- **GPU time** in seconds: the process's busiest engine `gpu_pct / 100 x seconds`.

Samples recorded before schema v2 have no `interval_s`; the gap since the previous
sample of the same incident is used instead.

`cpumon report` ranks groups by CPU or GPU time (`--metric`), and also shows peak
usage, the number of incidents and process instances, and when the group was last
seen. Groups (`--by`):

- `name`: image name, case-insensitive.
- `cmdline`: exact command line.
- `root`: the application a process belongs to. Walking up the recorded ancestry,
  the root is the last process before a shell or system host (`explorer.exe`,
  `svchost.exe`, `services.exe`, `sihost.exe`, `taskhostw.exe`, `wininit.exe`, ...).
  So Teams' `msedgewebview2.exe` renderers group under `ms-teams.exe` instead of
  `sihost.exe`. A host that is itself an offender is its own root.

`cpumon incidents` lists incidents with duration, triggers, peaks and the top process
(most CPU, capped at one core, plus GPU over the incident). `cpumon show` lists each
process's usage in one incident, then its command line, stored ancestry, user and any
`info_error`. The aggregation and grouping live in `cpumon-core` (`store/queries.rs`,
`report.rs`) so a GUI or HTML dashboard can reuse them.

## Privileges

Runs unelevated by default. Some command lines (elevated or protected processes) will
be unavailable and are recorded in `info_error`. Running from an elevated terminal, or
installing the logon task with `--elevated`, captures everything.

## Running at logon

`cpumon install` registers a Task Scheduler task named `cpumon` through the Task
Scheduler COM API (no `schtasks.exe`):

- Trigger: logon of the installing user. Action: `cpumonw.exe` (next to `cpumon.exe`)
  with `--db <absolute path> monitor <the given monitor options>`.
- `cpumonw.exe` is the same program built for the Windows GUI subsystem, so it runs
  without a console window. Its normal output is discarded; standard error (a start
  line, warnings, and the error if it exits) is appended to `cpumon.log` next to the
  database, rotated to `cpumon.log.old` past 1 MB. `cpumon status` shows the last
  lines.
- Principal: that user with the interactive logon type ("run only when the user is
  logged on"), so it runs in the user's session and can see which windows stop
  responding. An earlier version used S4U ("run whether logged on or not"), which
  needed admin rights to register and ran in session 0, where the user's windows are
  invisible. `--elevated` sets run level `HighestAvailable` and needs an elevated
  terminal (e.g. Windows' built-in `sudo`); tasks registered that way, or by the S4U
  version, also need one to remove.
- Settings: no time limit, runs on battery, one instance, normal priority, restarted up
  to 3 times a minute apart if it fails. Started immediately unless `--no-start`.
- Stopping the task ends the process without Ctrl+C; the incident left open is closed
  at its last sample on the next start.

`cpumon uninstall` stops and deletes the task (the database is kept); `cpumon status`
reports whether it is installed and running. Reinstall after moving the executables.

## Continuous integration

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy -D warnings` and
`cargo test` on `windows-latest` for pushes to `main` and pull requests. Pushing a tag
`v*` also builds a release and attaches `cpumon-<tag>-x86_64-windows.zip` (both
executables, README, LICENSE) to a GitHub release. Live tests tolerate machines without
a GPU. Text files are stored and checked out with LF (`.gitattributes`).

## Roadmap

1. ~~Workspace scaffold, build verified.~~ Done: CLI surface defined, `db path` works.
2. ~~GPU engine sampler (PDH + instance-name parsing) with a debug print command, to
   investigate the GPU discrepancy first.~~ Done: `cpumon gpu`. The process snapshot
   (names, parent PID, create time, CPU times) was pulled forward to label processes.
3. ~~CPU deltas and machine totals.~~ Done: `Sampler` combines everything; `cpumon top`.
   Verified against a single-thread burner: 99.4% of a core, 6.2% of the machine.
4. ~~Incident state machine and console logging.~~ Done: `cpumon monitor` logs to the
   console, including the single-process `core` trigger. Not yet persisted.
5. ~~SQLite storage, enrichment (command line, path, user), ancestry.~~ Done: `monitor`
   records to the database and prints command line, tree and user for new offenders.
6. ~~`report`, `incidents`, `show`, `db` commands.~~ Done: grouping by name, command
   line or application root; `db prune` removes old incidents and orphaned processes.
7. ~~`install` logon task.~~ Done: `install`, `uninstall`, `status`.
8. ~~Responsiveness signals.~~ Done: stall probe, DPC/interrupt time, CPU speed,
   temperature, memory and hung windows; `stall`, `dpc` and `hung` triggers; the logon
   task runs windowless in the user's session.
9. Later: ETW option, GUI/dashboard.
