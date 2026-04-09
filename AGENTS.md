# AGENTS.md

## Essential Commands

```sh
just              # build + test + lint + format-check (run before any commit)
just build        # cargo build --workspace
just test         # cargo test --workspace
just lint         # cargo clippy --workspace
just format-check # cargo fmt --check (CI equivalent)
just format       # cargo fmt (apply formatting)
```

Run a focused integration test suite:
```sh
cargo test --test end_to_end -p server
```

Run the server locally:
```sh
cargo run -- server
```

## Toolchain

- Rust edition **2024** (requires a recent stable toolchain)
- `rustfmt.toml`: `max_width = 100`
- Clippy lints: `clippy::all = deny`, `clippy::pedantic = warn` (workspace-wide via `Cargo.toml`). All crates inherit via `[lints] workspace = true`. Clippy failures are build errors.
- No async runtime — the event loop uses raw `poll(2)` (`crates/server/src/poll_loop.rs`).
- No CI, no pre-commit hooks. `just` is the only gate.

## Workspace Layout

```
src/main.rs           # Binary entrypoint — only `server` subcommand is wired up
crates/api/           # Wire protocol types (shared, no server deps)
crates/server/        # All server logic: config, handler, poll loop, state, IPC
crates/server/tests/  # Integration tests (spin up a real server process)
crates/server/testdata/ # Config fixtures
justfile              # Task runner
```

The `add`, `list`, and `rm` CLI subcommands exist in the wire protocol (`crates/api/`) but are **not yet wired into the CLI binary**. Only `aid server` runs.

## Architecture Notes

- **Single-threaded poll(2) event loop** — no Tokio, no threads. Adding async dependencies will clash with the design.
- **Unix socket IPC**: `$XDG_STATE_HOME/aid/server.sock` (default: `~/.local/state/aid/server.sock`). Wire format is newline-delimited JSON with a version envelope (`PROTOCOL_VERSION = 1`).
- Repo/state paths follow XDG: config in `~/.config/aid/config.toml`, state in `~/.local/state/aid/`.
