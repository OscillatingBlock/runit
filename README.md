# runit-rs

**A minimal runit-style init system written in Rust — a learning project.**

A PID-1–style supervisor that:
- watches a config directory, launches every service it finds (each service is a directory containing an executable `run` file),
- supervises children via a single `poll()` loop over a signalfd + command socket,
- restarts crashed services with exponential backoff, up to `MAX_RESTART` (services that burn through the budget are abandoned),
- hot-loads / hot-unloads services when the config directory changes,
- and exposes a small CLI client that talks to it over a **unix domain socket**.

> **Authorship:** the init system, supervision and restart logic, and protocol handling were written by me as I learn systems programming in Rust. The CLI client (`src/bin/runit-client`) and the shared wire-protocol module (`src/protocol.rs`) were written by **GLM** (AI coding agent).

## Building

```bash
cargo build
```

Two binaries are produced:
- `target/debug/runit` — the init / supervisor itself
- `target/debug/runit-client` — the control CLI

## Configuration

The init system reads three environment variables. A `.env` file in the repo root is loaded automatically at startup (via `dotenvy`), so you can just `cargo run`.

Example `.env`:

```dotenv
# path of the unix socket the client connects to
LISTENER_PATH=/tmp/runit.sock
# how many times a crashing service may be restarted before it is abandoned
MAX_RESTART=5
# directory containing service subdirectories
CONFIG_DIR=./mock_config
```

Each service lives in a subdirectory of `CONFIG_DIR` and must contain an **executable** `run` file (it is `execve`'d directly, so include a shebang):

```
mock_config/
  my_service/
    run    # e.g. #!/bin/sh  +  whatever the service does
  another_service/
    run
```

## Usage

Start the init system (keep this terminal open — the logs show supervision live):

```bash
cargo run
```

Then control it from another terminal:

```bash
cargo run --bin runit-client -- <command>
```

| Command | Description |
|---|---|
| `status` | list running services (PID + run-file path) |
| `stop <pid>` | stop one service by PID |
| `reload <pid>` | stop and relaunch a service under a new PID (fresh restart budget) |
| `restart` | re-sync the config directory (pick up added/removed services) |
| `shutdown` | stop all services and exit the init system |

The client exits with `0` on success and `1` on error, so it can be used in scripts:

```bash
runit-client status && runit-client reload 4242
```

## Behaviour notes

- A service that exits with a non-zero code is scheduled for restart with exponential backoff (1s, 2s, 4s, ...). Hitting `MAX_RESTART` abandons it.
- A service that exits `0` is considered finished and is not restarted.
- Services are hot-loaded/hot-unloaded from the config directory every few seconds (the `restart` command forces a check).
- `stop`ping a service and hot-unloading its directory do not count as crashes and do not trigger a restart.
