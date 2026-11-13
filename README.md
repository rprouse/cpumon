# CPU/GPU Monitor

[![CI](https://github.com/rprouse/cpumon/actions/workflows/ci.yml/badge.svg)](https://github.com/rprouse/cpumon/actions/workflows/ci.yml)

A lightweight, Windows-only CLI that monitors CPU and GPU usage and catches the
processes responsible for high usage. Spikes are often gone by the time Task Manager is
checked.

`cpumon` samples usage every few seconds. When usage stays high it records the
offending processes to a local SQLite database: their CPU and per-engine GPU usage,
command line, image path, user, and process ancestry up to the root process. It also
logs them to the console. Report commands then show the worst offenders over time.

> **Status:** early development. Monitoring, recording, reports and running at logon
> work. See [docs/design.md](docs/design.md) for
> the design and roadmap.

## Usage

```powershell
cpumon monitor                                   # watch and record (Ctrl+C to stop)
cpumon monitor --interval 2s --cpu 80 --gpu 80 --proc-core 90
cpumon incidents --since 24h                     # list high-usage episodes
cpumon show 42 --samples                         # processes, command lines and trees
cpumon report --since 7d --by root --metric gpu  # worst offenders by application
cpumon db prune --older-than 30d                 # delete old incidents (monitor also
                                                 # does this daily: --retain 30d)
cpumon install                                   # run the monitor at every logon
cpumon status                                    # is the logon task running? + log
cpumon uninstall
cpumon top / cpumon gpu                          # live diagnostics
```

An incident starts when total CPU or GPU (busiest engine) stays at or above its
threshold, or one process uses most of a core, for `--sustain` samples in a row. It
starts at once when the machine stalls (a thread is kept waiting for `--stall`,
default 500 ms), a CPU is swamped by driver DPCs/interrupts, or a window stops
responding. Each recorded sample also notes CPU speed, temperature, free memory and
paging, and `cpumon show` names the program whose window hung.

The database is stored at `%LOCALAPPDATA%\cpumon\cpumon.db` (`cpumon db path`).
Run from an elevated terminal to capture command lines of system processes.

## Building

Prerequisites:

- [Rust](https://rustup.rs/) (stable, `x86_64-pc-windows-msvc` toolchain):
  `winget install Rustlang.Rustup`
- Visual Studio Build Tools with the **Desktop development with C++** workload
  (VC++ tools and Windows SDK).

```powershell
cargo build --release   # target\release\cpumon.exe and cpumonw.exe
```

Releases with prebuilt executables are published from `v*` tags.

## License

[MIT](LICENSE)
