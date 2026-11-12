# CPU/GPU Monitor

A lightweight, Windows-only CLI that monitors CPU and GPU usage and catches the
processes responsible for high usage. Spikes are often gone by the time Task Manager is
checked.

`cpumon` samples usage every few seconds. When usage stays high it records the
offending processes to a local SQLite database: their CPU and per-engine GPU usage,
command line, image path, user, and process ancestry up to the root process. It also
logs them to the console. Report commands then show the worst offenders over time.

> **Status:** early development. The CLI is scaffolded, but the commands are not yet
> implemented. See [docs/design.md](docs/design.md) for the full plan and roadmap.

## Planned usage

```powershell
cpumon monitor --interval 2s --cpu 80 --gpu 80   # watch and record
cpumon incidents --since 24h                     # list high-usage episodes
cpumon show 42                                   # details and process trees for one incident
cpumon report --since 7d --by root --metric gpu  # worst offenders
```

The database is stored at `%LOCALAPPDATA%\cpumon\cpumon.db`.

## Building

Prerequisites:

- [Rust](https://rustup.rs/) (stable, `x86_64-pc-windows-msvc` toolchain):
  `winget install Rustlang.Rustup`
- Visual Studio Build Tools with the **Desktop development with C++** workload
  (VC++ tools and Windows SDK).

```powershell
cargo build --release
```

## License

[MIT](LICENSE)
