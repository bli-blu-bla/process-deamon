# procd

**English** | [Deutsch](README.de.md)

`procd` is a lightweight process supervisor for Docker containers that need
to run more than one long-running process, e.g. a web server and a worker.
procd runs as the container's entrypoint, starts the processes defined in
`process.toml` and looks after them:

- **Lifecycle:** start on boot, orderly stop on `docker stop`
  (SIGTERM to each process group, SIGKILL after a timeout)
- **Restart on crash** with a delay and a burst limit, similar to systemd's `Restart=on-failure`
- **Logs:** the output of all processes shows up in `docker logs` with a
  timestamp and the process key, optionally also in a file per process
- **Schedules:** start processes automatically via cron expressions (cron
  jobs) or restart them periodically
- **Crash hook:** an arbitrary command per crash, e.g. for notifications
- **Status:** state, PID, uptime, memory and CPU per process
- **Control:** via the CLI (`docker exec … procd ls`) or a JSON API on a Unix
  socket that other processes or containers can use as well

A single static binary (~2 MB), no runtime dependencies.

## Installation

`install.sh` downloads the static binary for the matching architecture
(`x86_64` or `aarch64`) from the GitHub releases, verifies its SHA256 checksum
and installs it to `/usr/local/bin/procd`. It only needs `sh` and `curl` or
`wget`, so it also works with busybox on Alpine.

```bash
curl -fsSL https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh | sh

# specific version, different directory
curl -fsSL https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh \
    | sh -s -- --version v0.2.0 --dir ~/.local/bin
```

| Option | Env | Default |
|---|---|---|
| `-v`, `--version <tag>` | `PROCD_VERSION` | `latest` |
| `-d`, `--dir <path>` | `PROCD_INSTALL_DIR` | `/usr/local/bin` |
| `-r`, `--repo <owner/repo>` | `PROCD_REPO` | `bli-blu-bla/process-deamon` |
| | `PROCD_BASE_URL` | `https://github.com/<repo>/releases` (for mirrors) |

If the target directory isn't writable, `sudo` is used. In a Dockerfile this
replaces the build stage:

```dockerfile
FROM alpine:3
RUN wget -qO- https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh | sh -s -- --version v0.2.0
```

Releases are created automatically when a `v*` tag is pushed (see
`.github/workflows/build.yml`).

## Docker

### Adding it to an image

procd is built in its own build stage and copied into the application image
as a binary:

```dockerfile
FROM rust:1-alpine AS procd
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM alpine:3
COPY --from=procd /src/target/release/procd /usr/local/bin/procd
COPY process.toml /etc/procd/process.toml
ENV PROCD_CONFIG=/etc/procd/process.toml

HEALTHCHECK --interval=30s --timeout=3s CMD ["procd", "ls"]
ENTRYPOINT ["procd"]
```

The musl binary is statically linked, so it also runs in images that aren't
based on Alpine (Debian, distroless, `scratch` with `/bin/sh`).

### Running

```bash
docker run -d --init --name app my-image
```

```yaml
# compose.yaml
services:
  app:
    image: my-image
    init: true
    stop_grace_period: 30s
```

### Operating

```bash
docker logs -f app                     # output of all processes
docker exec app procd ls               # overview
docker exec app procd status worker
docker exec app procd stop worker
docker exec app procd start -a
```

```
KEY       STATE    PID  UPTIME  MEM      CPU   LAST EXIT
web (ro)  running  7    3h12m   48.2 MiB 1.3%  -
worker    crashed  -    -       -        -     code 1
```

### Notes for running in containers

- **Stop timeout:** `docker stop` waits 10s by default and then sends SIGKILL
  to the whole container. In the worst case procd needs `shutdown_timeout`
  (default 10s) plus the remaining runtime of hooks that are still running
  (`failed_hook_timeout`). So set `stop_grace_period` or `docker stop -t`
  higher than `shutdown_timeout`, or lower `shutdown_timeout`.
- **PID 1 / `--init`:** procd reaps its own children, but not unrelated
  orphaned processes that the kernel hands to PID 1 (e.g. grandchildren of a
  crashed process). With `--init` (compose: `init: true`) a minimal init
  process (tini) takes care of that. It forwards signals to procd.
- **Healthcheck:** `procd ls` exits with a non-zero code if procd doesn't respond.
- **Sharing the socket:** if `core.sock` lives in a volume, other containers
  can use the API. `socket_perm` and the socket file's group must match the
  accessing user.
- **Read-only mode:** with `procd --readonly` the API can only list
  processes, not start or stop them. Useful when the socket is shared with
  other containers.

## CLI

Without a subcommand, procd runs as the supervisor (same as `procd run`). The
other subcommands are a client that talks to a running instance over the
socket.

```bash
procd                               # supervisor with ./process.toml
procd -c /etc/procd/process.toml    # different config
procd run --config process.toml -r  # supervisor, read-only API

procd ls                            # table of all processes (alias: list)
procd ls --json                     # the same as JSON
procd status worker                 # details of one process
procd start worker                  # start individual processes …
procd stop worker other             # … or stop them
procd start -a                      # start all that aren't running
procd stop --all                    # stop all running ones (except readonly processes)
procd check -c process.toml         # validate the config and exit, e.g. during the image build
```

### Supervisor options

| Short | Long | Env | Default | Description |
|---|---|---|---|---|
| `-c` | `--config <PATH>` | `PROCD_CONFIG` | `process.toml` | Path to the config file |
| `-r` | `--readonly` | | off | Rejects `process.start`/`process.stop` over the socket, regardless of the config. Listing is still allowed. |
| `-h` | `--help` | | | Help (`--help` for details) |
| `-V` | `--version` | | | Version |

### Client options

| Short | Long | Env | Description |
|---|---|---|---|
| `-s` | `--socket <PATH>` | `PROCD_SOCKET` | Socket of the running instance. Default: `core.sock` from the config (`-c`), otherwise `/run/procd.sock` |
| `-c` | `--config <PATH>` | `PROCD_CONFIG` | Config to read `core.sock` from (only that field; if the file is missing, the default applies) |
| `-a` | `--all` | | `start`/`stop`: all processes instead of individual keys |
| `-j` | `--json` | | `list`/`status`: JSON instead of a table |

`start`/`stop` exit with code `1` if at least one process fails. A process
that is already in the target state doesn't count as an error and is reported
as `skip`. Colors are only used on a terminal; `NO_COLOR=1` turns them off.

## Configuration: `process.toml`

Unknown fields are an error at startup (protection against typos). Durations
are given as strings: `"500ms"`, `"10s"`, `"1m30s"`, `"1h"`. Relative paths are
resolved against the directory of `process.toml`, not the current working
directory. A commented example is in [`process.toml`](process.toml).

### `[core]`

| Field | Type | Default | Description |
|---|---|---|---|
| `sock` | string | `/run/procd.sock` | Path of the API's Unix socket. A stale socket file left by a crashed run is removed at startup. If an active instance is still listening there, startup aborts before any process is started. |
| `socket_perm` | octal literal | `0o660` | File permissions of the socket |
| `shutdown_timeout` | duration | `10s` | How long to wait after sending SIGTERM to a process before SIGKILL follows. Applies to `stop` and to procd shutting down. |
| `metrics_interval` | duration | `2s` | Sampling interval for memory/CPU |
| `envfile` | string | – | `.env` file (`KEY=VALUE` per line, `#` comments, optional quotes) whose variables are passed to **all** processes and hooks. If the file is missing or a line is invalid, startup aborts. |
| `logfile` | string | – | Shared crash log: one line per crash across all processes, with timestamp, key, exit code, signal and `cmd` |
| `failed_hook` | string | – | Command run on every crash (see [Crash hook](#crash-hook)) |
| `failed_hook_timeout` | duration | `30s` | Maximum runtime of a hook; after that its process group is killed with SIGKILL |

### `[[process]]`

One block per process, as many as needed.

| Field | Type | Default | Description |
|---|---|---|---|
| `key` | string | *required* | Unique name, used by the CLI, the API and as log prefix |
| `cmd` | string | *required* | Run as `sh -c "<cmd>"` in its own process group. Signals go to the whole group. With `exec` (`cmd = "exec my-server --flag"`) the program replaces the shell; PID and metrics then refer to the program itself. |
| `workdir` | string | procd's cwd | Working directory |
| `env` | string array | – | Additional variables (`"KEY=VALUE"`). They override the inherited environment and `core.envfile`. |
| `readonly` | bool | `false` | Prevents **only stopping** via CLI/API. Starting is still allowed; when procd shuts down, the process is stopped anyway. |
| `autostart` | bool | `true` | Start on boot |
| `stdout` | bool | `true` | Mirror stdout/stderr with a prefix to the console (→ `docker logs`) |
| `logfile` | string | – | Also write stdout/stderr raw (without prefix) to this file. Independent of `stdout`. |
| `restart` | bool | `false` | Restart on **crash** (non-zero exit code or signal). Not on exit code 0 and not after a stop. |
| `restart_delay` | duration | `1s` | Delay before each restart |
| `restart_window` | duration | `60s` | Time window for the burst limit |
| `restart_max_retries` | int | `5` | Maximum number of restarts within `restart_window`; after that the state is `failed` and no further attempt is made. `0` = unlimited. A manual start resets the counter. |
| `failed_hook` | string | inherits `core.failed_hook` | Own hook for this process; `""` disables it |
| `schedule` | string | – | Schedule as a cron expression (see [Schedules](#schedules)) |
| `schedule_action` | string | `"start"` | What happens when the schedule fires: `"start"` or `"restart"`. Only allowed together with `schedule`. |

### States

| State | Meaning |
|---|---|
| `stopped` | not started, or ended by a stop |
| `running` | running |
| `stopping` | SIGTERM sent, waiting for the exit |
| `exited` | exited on its own with exit code 0 |
| `crashed` | exited on its own with a non-zero exit code or by a signal; a restart may be scheduled |
| `failed` | restart limit reached, no further automatic attempt |

(`starting` exists in the protocol but is practically never visible.)

### Console output

```
[2026-08-17 12:00:12][core] procd 0.1.0 starting (config=/etc/procd/process.toml, readonly=false)
[2026-08-17 12:00:12][core] listening on /run/procd.sock (readonly=false)
[2026-08-17 12:00:13][web] server started on :8080
[2026-08-17 12:00:13][worker] waiting for jobs
[2026-08-17 12:03:47][core] worker crashed, restarting in 2s (attempt 1/5 within 1m0s)
```

## Schedules

With `schedule`, procd starts or restarts a process at fixed times.
`schedule_action` defines what happens:

- **`start`** (default): the process is started if it isn't running. If it is
  still running from the previous run, the slot is skipped (no overlap). This
  is a classic cron job, usually with `autostart = false`.
- **`restart`**: a running process is stopped (SIGTERM, SIGKILL after
  `shutdown_timeout`) and then started again. If it isn't running, it is
  simply started. Meant e.g. for a nightly restart of a service that leaks
  memory.

```toml
[[process]]
key = "cleanup"
cmd = "exec /usr/local/bin/cleanup --older-than 7d"
autostart = false
schedule = "30 2 * * *"         # daily at 02:30

[[process]]
key = "web"
cmd = "exec /usr/local/bin/web-server"
schedule = "0 4 * * sun"        # restart on Sundays at 04:00
schedule_action = "restart"
```

### Syntax

Five fields as in classic cron: `minute hour day month weekday`.

| Field | Values | Names |
|---|---|---|
| minute | 0-59 | |
| hour | 0-23 | |
| day | 1-31 | |
| month | 1-12 | `jan`-`dec` |
| weekday | 0-7 (0 and 7 = Sunday) | `sun`-`sat` |

Each field accepts `*`, single values (`5`), ranges (`1-5`, `mon-fri`), steps
(`*/15`, `10-50/10`) and lists (`1,15,30`). If both day *and* weekday are
restricted, it's enough for one of them to match (as in cron). In addition:

| Expression | Meaning |
|---|---|
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly` | `0 0 1 * *` |
| `@weekly` | `0 0 * * 0` |
| `@daily`, `@midnight` | `0 0 * * *` |
| `@hourly` | `0 * * * *` |
| `@every <duration>` | fixed interval from procd's start, e.g. `@every 30m` (at least `1s`) |

### Behavior

- Times are in the container's local time zone (`TZ` or `/etc/localtime`,
  UTC if neither is set). When daylight saving time starts, times that don't
  exist are skipped; when it ends, a repeated time runs only once.
- Slots missed while a restart was running or the clock jumped are not made
  up for.
- A schedule runs independently of `autostart`, `restart` and manual stops:
  a process stopped via the CLI is started again at the next slot. It also
  runs for `readonly` processes, because it comes from the config and not
  from the API.
- A scheduled start counts as a manual start: a process in `failed` gets a
  new chance, and the restart counter is reset.
- `procd status <key>` shows the schedule and the next slot. In the API these
  are `schedule`, `schedule_action` and `next_run`.

## Crash hook

When a process crashes (non-zero exit code or signal, without a stop having
been requested), procd runs `failed_hook`. Exit code 0 and a regular stop
don't trigger it.

- **One call per crash,** *after* the restart decision:
  `PROCD_EVENT=crashed` (restart scheduled or `restart` off) or
  `PROCD_EVENT=failed` (restart limit reached).
- Runs via `sh -c` in the process's `workdir`, in its own process group,
  with the process's environment (including `envfile` and `env`) plus the
  variables below. So tokens for notifications can come from the `envfile`.
- Runs asynchronously and blocks neither the restart nor other processes.
  Its output appears with a `[hook]` prefix under the process's key.
- Once procd is shutting down, no new hooks are started. Running ones may
  finish, up to `failed_hook_timeout`.

### Environment variables

All variables are always set (possibly empty), so scripts can read them
under `set -u` too.

| Variable | Example | Description |
|---|---|---|
| `PROCD_EVENT` | `crashed` | `crashed` or `failed` |
| `PROCD_KEY` | `worker` | the process's key |
| `PROCD_CMD` | `exec /usr/local/bin/worker` | its `cmd` |
| `PROCD_WORKDIR` | `/app` | its `workdir` (empty = procd's cwd) |
| `PROCD_LOGFILE` | `/var/log/procd/worker.log` | its `logfile` (empty if none) |
| `PROCD_STATE` | `crashed` | `crashed` or `failed` |
| `PROCD_PID` | `4711` | PID of the crashed run |
| `PROCD_EXIT_CODE` | `3` | exit code; `-1` for a signal |
| `PROCD_EXIT_SIGNAL` | `killed` | signal name (`killed`, `terminated`, `segmentation fault`, …), empty on a normal exit |
| `PROCD_STARTED_AT` | `2026-08-17T12:00:12+02:00` | start time of the run (RFC 3339) |
| `PROCD_EXITED_AT` | `2026-08-17T12:03:47+02:00` | time of the crash (RFC 3339) |
| `PROCD_UPTIME_SECONDS` | `215` | runtime until the crash |
| `PROCD_RESTART_ENABLED` | `true` | whether `restart` is enabled |
| `PROCD_RESTART_SCHEDULED` | `true` | whether a restart was scheduled |
| `PROCD_RESTART_DELAY_SECONDS` | `2` | delay until then (`0` if none is scheduled) |
| `PROCD_RESTART_COUNT` | `3` | crashes in the current `restart_window`, including this one |
| `PROCD_RESTART_MAX_RETRIES` | `5` | `restart_max_retries` (`0` = unlimited) |

### Example

```sh
#!/bin/sh
# /usr/local/bin/notify-crash.sh
set -eu

if [ "$PROCD_EVENT" = "failed" ]; then
    title="$PROCD_KEY: automatic restarts given up"
else
    title="$PROCD_KEY crashed"
fi

curl -fsS -X POST "$NOTIFY_WEBHOOK" \
    --data-urlencode "title=$title" \
    --data-urlencode "body=exit=$PROCD_EXIT_CODE signal=$PROCD_EXIT_SIGNAL uptime=${PROCD_UPTIME_SECONDS}s attempt=$PROCD_RESTART_COUNT/$PROCD_RESTART_MAX_RETRIES"
```

## API

The CLI is a client of this API. Other programs can talk to the socket
directly. The protocol is not HTTP but modeled after JSON-RPC 2.0: **one JSON
line per request** (terminated by `\n`), one JSON line per response. Multiple
requests can be sent one after another over a single connection. It is closed
after 60s without a request.

```json
→ {"id": 1, "method": "process.start", "params": {"key": "worker"}}
← {"id": 1, "result": {"process": {"key": "worker", "state": "running", "pid": 4711, ...}}}
← {"id": 1, "error": {"code": -32004, "message": "process \"worker\" is already running"}}
```

```bash
printf '{"id":1,"method":"process.list"}\n' | nc -U /run/procd.sock -w 1
```

### Methods

| Method | Params | Result | Blocked by `--readonly` |
|---|---|---|---|
| `process.list` | – | `{"processes": [ProcessInfo, ...]}` | no |
| `process.get` | `{"key": string}` | `{"process": ProcessInfo}` | no |
| `process.start` | `{"key": string}` | `{"process": ProcessInfo}` | yes |
| `process.stop` | `{"key": string}` | `{"process": ProcessInfo}` | yes (and per-process `readonly`) |

### `ProcessInfo`

| Field | Description |
|---|---|
| `key`, `cmd` | from the config |
| `state` | see [States](#states) |
| `pid`, `started_at`, `uptime_seconds` | only while the process is running |
| `rss_bytes`, `cpu_percent` | sampled from `/proc`, only while the process is running; `cpu_percent` is not normalized per core (as in `top`) |
| `readonly`, `autostart`, `stdout`, `logfile` | from the config |
| `exit_code`, `exit_signal` | set once the process has exited at least once |
| `schedule`, `schedule_action` | from the config, only for processes with a schedule |
| `next_run` | next scheduled slot (RFC 3339) |

Fields without a value are omitted.

### Error codes

| Code | Meaning |
|---|---|
| `-32700` | line is not valid JSON |
| `-32600` | `method` is missing |
| `-32601` | unknown method |
| `-32602` | `params.key` is missing or empty |
| `-32603` | internal error (e.g. start failed) |
| `-32001` | unknown `key` |
| `-32002` | procd runs with `--readonly`, modifying methods are forbidden |
| `-32003` | process has `readonly = true` and can't be stopped |
| `-32004` | already in the target state (start on a running, stop on a stopped process) |

## Build

```bash
cargo build --release                                   # → target/release/procd
cargo build --release --target x86_64-unknown-linux-musl  # static, for containers
cargo test
```

## Limitations

- Linux only: metrics come from `/proc`, signals go to process groups.
- Restart only on crash, not after exit code 0.
- `shutdown_timeout` is global, not per process.
- Processes start in parallel, without ordering or dependencies.
- Schedules have minute resolution; seconds fields (6 fields) are not supported.

## Migrating from process-ctl

procd is the Rust rewrite of `process-ctl`. Existing `process.toml` files work
unchanged. The API's protocol, methods, fields and error codes are the same.
What changed:

- **Invocation:** `-config`/`-readonly` are now `-c`/`--config` and
  `-r`/`--readonly`. The old invocation aborts with an error message.
- **Hook variables:** `PROCESS_CTL_*` are now `PROCD_*`.
- **Default socket:** `/run/procd.sock` instead of `/run/observer.sock`. This
  only affects configs without `core.sock`.
- **Crash detection:** a crash is now also detected when a child process
  keeps the output pipe open. process-ctl stayed in `running` forever in that
  case.
- **Hook environment:** the hook always inherits the full environment.
  process-ctl passed only the hook variables without `envfile`/`env`, so not
  even `PATH`.
- A per-process `logfile` is resolved relative to `process.toml`, like all
  other paths.
