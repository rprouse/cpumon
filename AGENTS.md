# AGENTS.md

Guidance for AI coding agents working in this repository.

## Project

`cpumon` is a Windows-only Rust CLI that monitors CPU/GPU usage, records offending
processes (with command line and process ancestry) to SQLite, and reports the worst
offenders. **Read [docs/design.md](docs/design.md) before making changes.** It is the
source of truth for architecture, schema, CLI, and roadmap. Update it when a design
decision changes.

## Principles

- **Low overhead first.** The monitor runs continuously and must never become an
  offender itself. No WMI, no shelling out, no per-process handle opening on the hot
  path. Avoid allocations in the per-sample loop where practical, and reuse buffers and
  the PDH query between samples.
- **Windows only.** Target `x86_64-pc-windows-msvc`. No cross-platform abstractions.
- **Identify processes by `(pid, create_time)`**, never by PID alone (PIDs are reused).
- **Degrade gracefully when unelevated.** Access-denied on enrichment is recorded as
  data (`info_error`), not an error.
- **Store timestamps in UTC**; convert to local time only for display.

## Layout

- `crates/cpumon-core`: library with sampling, Windows API calls, storage, and queries.
  Keep `unsafe` code here, in small functions with a `// SAFETY:` comment on each block.
- `crates/cpumon`: the binary with CLI parsing (`clap`) and console output only.
- `docs/`: design and other documentation.

## Dependencies

- `windows` for Win32/WDK APIs. Enable only the features actually used.
- `rusqlite` with `bundled`.
- `clap` (derive), `comfy-table`.
- Discuss before adding other dependencies; prefer small, well-maintained crates.

## Commands

```powershell
cargo build                   # debug build
cargo build --release         # release build
cargo test                    # all tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

Run `cargo fmt`, `cargo clippy`, and `cargo test` before considering a change done.

## Testing

- Unit-test pure logic thoroughly: GPU instance-name parsing, CPU delta math, the
  incident/threshold state machine, ancestry resolution with PID reuse, and SQL
  queries (against an in-memory SQLite database).
- Keep Windows API calls behind thin functions so the logic around them can be tested
  with fabricated snapshots.
- Tests that hit live system APIs should tolerate any machine state (no assertions on
  specific usage values).

## Conventions

- Rust 2024 edition, `rustfmt` defaults, clippy clean.
- Errors: `thiserror` in `cpumon-core` if needed, `anyhow` in the binary.
- Database schema changes go through the versioned migration in `store/` and bump
  `PRAGMA user_version`. Never edit an already-shipped migration.
- Commit or push only when asked.
