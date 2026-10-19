# Containers

The Containers tab (`Alt+4`) lists Docker and Podman containers with CPU,
memory, network and block I/O, plus CPU and RX sparklines. It talks to the
engine's own API through [bollard](https://github.com/fussybeaver/bollard) — no
shelling out to `docker`.

---

## Read this before granting access

**Access to `/var/run/docker.sock` is equivalent to root on the host.** Any user
who can reach it can start a container with the host filesystem mounted and
break out. That is a property of Docker's architecture, not of muxtop, and no
monitoring tool can make it safe.

So the recommended setup is **rootless Podman**, whose per-user socket at
`$XDG_RUNTIME_DIR/podman/podman.sock` grants only that user's containers:

```sh
systemctl --user enable --now podman.socket
muxtop        # detects it automatically
```

If you must use Docker, understand what adding a user to the `docker` group
costs — see [Security model](Security-model).

---

## How the socket is found

With no flags, muxtop tries these in order and uses the first that connects:

| # | Candidate | |
|---|---|---|
| 1 | `$DOCKER_HOST` | if it parses as `unix://…` or `tcp://…` |
| 2 | `/var/run/docker.sock` | Docker, system |
| 3 | `$XDG_RUNTIME_DIR/podman/podman.sock` | **Podman, rootless** |
| 4 | `/run/podman/podman.sock` | Podman, system |

If none connect, the tab shows *"no engine configured"* — an explicit state, not
an empty table pretending there are no containers.

> **`DOCKER_HOST` pointing at a non-loopback address is flagged.** muxtop warns
> that container metadata will be sent to that host, because a `tcp://` Docker
> endpoint means your queries leave the machine.

### Overriding

```sh
muxtop --docker-socket /var/run/docker.sock                   # force a path
muxtop --docker-socket "$XDG_RUNTIME_DIR/podman/podman.sock"  # rootless Podman
muxtop --no-containers                                        # skip detection
```

`--no-containers` disables detection entirely. On a host with no containers it
saves a little startup work; on `muxtop-server` it removes the data source for
every remote client.

---

## What it reads

Monitoring reads these endpoints (exec is described separately below):

- `GET /containers/json` — the list, names, images, state
- `GET /containers/{id}/stats?stream=false` — one stats sample per cycle
- `GET /containers/{id}/json` — actual start time, including restarts

Container stats are collected at **0.5 Hz** — every two seconds — independently
of the 1 Hz process loop. Container stats are the most expensive thing muxtop
polls (the engine computes them per container), so they get a slower loop of
their own rather than dragging the whole UI to their pace.

---

## Actions

| Key | Action | Signal |
|---|---|---|
| `F9` | Stop | SIGTERM, then the engine's grace period |
| `F10` | Kill | SIGKILL, immediately |
| `F11` | Restart | stop + start |

All three ask for confirmation, naming the container, and all three are
**disabled in `--remote` mode** — a remote muxtop is a viewer.

`x` opens the same actions as a menu, and the command palette reaches them by
name: `:stop nginx`, `:restart postgres`.

## Interactive shell (Linux/macOS, local mode)

Select a **running** container and press `e`, choose **Open container shell (PTY)**
from `x`, or find the same action in `Ctrl+P`. Selection follows the displayed
sort/filter and uses the full container ID.

muxtop suspends its interface and attaches `/bin/sh` with a Docker/Podman PTY via
its existing daemon connection. No Docker/Podman CLI is required. The shell uses
the container's default user and working directory, without privileged exec.
`TERM` comes from muxtop's environment (fallback `xterm-256color`).

- Wait for the shell prompt before typing: queued input is discarded when
  entering and leaving the attachment so it cannot replay as muxtop shortcuts.
- `Ctrl+C`, `Ctrl+D`, arrows, UTF-8 and other input pass to the shell as raw bytes.
- `exit` or shell EOF returns to muxtop and reports the exit status when available.
- `Ctrl+]` closes the attachment and returns to muxtop. It consumes typeahead in
  the same input chunk. **Detaching does not guarantee termination** of the shell
  or any jobs it started; use `exit` for a normal shell shutdown.
- Resizing the terminal resizes the PTY (checked every 100 ms).
- Setup and stalled writes have ten-second deadlines. Errors restore the TUI;
  muxtop never retries shell startup automatically.

Both stdin and stdout must be terminals. Windows and `--remote` do not offer an
interactive session. Stopped/paused containers are refused. Images without
`/bin/sh` (including distroless images) report a daemon error; muxtop does not
install a shell or substitute a different command.

Shell output is a direct terminal stream, including escape sequences, as with
`docker exec -it`. It is not passed through the monitoring table's text sanitizer.

---

## Reading the table

`Enter` opens the inspector for the selected row — the full image reference,
ports, and the values the column widths had to truncate.

- **CPU%** is normalised the way the engine reports it, so a container using two
  full cores on an 8-core host can read above 100%.
- **MEM** is the container's usage against its limit where one is set.
- **NET / BLOCK** are cumulative counters since the container started; the
  sparkline shows the rate.
- A **`—`** means the engine did not report that field — a container with no
  memory limit, or a paused container with no current stats. It does not mean
  zero.

Sort with `s`, reverse with `S`, filter with `/`. The filter matches names and
images, and is independent of the process filter on the Processes tab.

---

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| *"no engine configured"* but `docker ps` works | Your shell has `DOCKER_HOST` set and muxtop was started from a different environment (a desktop launcher, systemd). Pass `--docker-socket` explicitly |
| *"no engine configured"* with rootless Podman | The user socket is not running: `systemctl --user enable --now podman.socket`. It also needs an active user session — check `loginctl enable-linger $USER` for a headless box |
| Permission denied on the socket | Your user is not in the `docker` group. Prefer rootless Podman over joining it — see [Security model](Security-model) |
| Empty list, engine connected | You are looking at a different engine than you think. Rootless Podman containers are invisible to the system socket, and vice versa |
| Stats columns all `—` | Containers are paused or just created, or the engine has not produced a first sample yet — wait one cycle (2 s) |
| Tab empty over `--remote` | The **server** has no engine, or was started with `--no-containers`. The client's flags do not apply |
| Podman containers missing under `sudo` | Root's Podman and your user's Podman are separate stores |

For detail, check `~/.local/share/muxtop/muxtop.log`, or run with
`MUXTOP_LOG=debug muxtop` to log the socket-detection sequence.

---

## Not implemented

- **Logs** (`docker logs`) are not shown — use `docker logs` for now.
- **Creating, deleting or scaling** containers is out of scope. muxtop stops,
  kills and restarts what already exists; it is a monitor, not an orchestrator.
