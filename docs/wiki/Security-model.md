# Security model

What muxtop defends, what it does not, and why. If you are about to run
`muxtop-server` on a host that matters, this is the page to read.

To **report** a vulnerability, use [private security
reporting](https://github.com/lucasschimmel/muxtop/security/advisories/new) —
never a public issue. Scope and response targets are in
[SECURITY.md](https://github.com/lucasschimmel/muxtop/blob/main/SECURITY.md).

---

## The uncomfortable part first

A system monitor is a **privilege amplifier**. It exists to show you things a
normal process cannot see, and the interesting features want progressively more
access:

| Feature | What it needs | What that grants |
|---|---|---|
| Processes, CPU, memory, network | `/proc` — world-readable by default | Every process's command line on the host |
| Kill / renice | Ownership of the target, or root | Ability to end other users' processes |
| Containers | The Docker socket | **Root-equivalent on the host** |
| Containers, rootless | A user Podman socket | That user's containers |
| Kubernetes | A kubeconfig or ServiceAccount | Everything that credential can read |
| GPU | NVML, `/sys/class/drm` | Nothing privileged |
| Remote | A listening TCP port | See the whole chain below |

muxtop's design goal is to make the **minimum** of that list useful, and to
make each step up explicit rather than a default. It cannot make Docker socket
access safe — nothing can — so it documents the cost and makes rootless Podman
the easy path.

---

## Trust boundaries

Four boundaries, from the outside in. Each is a place where muxtop assumes the
data is hostile.

### 1. The network → `muxtop-server`

The attacker is unauthenticated and can reach the port.

| Control | Detail |
|---|---|
| **TLS 1.3 only** | rustls. No 1.2, no downgrade path, no cipher negotiation to get wrong |
| **Mandatory token** | ≥ 16 characters, enforced at startup. There is no anonymous mode |
| **No plaintext mode** | The server refuses to start without a certificate |
| **Per-IP rate limit** | Token bucket, default 10 connections/s with a burst of 10 (`--rate-limit-per-ip`) |
| **Connection cap** | `--max-clients`, default 8 |
| **Localhost default** | `--bind` defaults to `127.0.0.1:4242`. Exposing it is a decision you make |

The goal at this boundary: an unauthenticated peer can achieve nothing except
its own disconnection. Anything more — a panic in the frame decoder, an
allocation driven by an attacker-chosen length, an authentication bypass — is
in scope for a report.

### 2. `muxtop-server` → the client

The attacker is an authenticated client, or someone reading the wire.

**Credentials never traverse the wire.** The server is the only side that opens
a kubeconfig, a container socket or a TLS key. The client receives digested
snapshots — numbers and names — and nothing it could replay elsewhere.

That is asserted, not just intended: the test suite in
`crates/muxtop-proto/tests/integration.rs` encodes snapshots and checks
byte-for-byte that no `BEGIN PRIVATE KEY`, no `Bearer `, and no
`client-key-data:` appears in any frame.

**The auth token cannot be logged by accident.** Both binaries wrap it in a
newtype whose `Debug` implementation prints `Token("[REDACTED]")`, so a
`tracing::debug!`, a panic message or a `format!("{:?}", cli)` cannot spill it.

**Host-mutating actions are refused remotely.** Kill, force-kill, renice and the
container stop/kill/restart/shell actions are local-only. `muxtop --remote` is a
viewer, so a compromised client cannot use the server as a remote-execution
proxy.

### 3. Collected data → your terminal

The attacker controls a string that muxtop will render.

This boundary is the one people forget. A container label, a process `comm`
field, a pod name and a network interface name are all **attacker-controlled**,
and a terminal is an interpreter. A process named
`$'\e]0;owned\a'` would rewrite your window title; worse sequences exist.

So every externally-sourced monitoring string passes through `scrub_ctrl`
(`crates/muxtop-tui/src/ui/sanitize.rs`) before it reaches a cell:

- Replaced with `?`: every ASCII control byte — `0x00`–`0x08`, `0x0a`–`0x1f`
  (which includes `ESC`, the CSI and OSC introducer), and `0x7f` (DEL).
- Preserved: `0x09` (TAB), and every byte ≥ `0x80`, so UTF-8 process names
  render correctly rather than being mangled into `?????`.

A `?` where you expected a character is muxtop telling you the source string
contained a control byte.

An explicitly opened **interactive container shell** is a separate terminal
attachment. Its output intentionally includes raw escape sequences, as with
`docker exec -it`; it does not have the table sanitizer's protection. It uses the
same daemon connection, the container's default user and working directory, and
never requests privileged exec. Detach closes the transport and does not promise
to terminate the remote process. Exec never traverses muxtop's monitoring wire.

### 4. You → the host

The attacker is a mistake.

- **Signals come from a fixed set.** Only SIGTERM and SIGKILL, chosen through an
  enum. There is no path that passes a raw signal number through to `kill(2)`.
- **PIDs are validated before any syscall.** A PID above `i32::MAX` would wrap
  to negative, and `kill(-1, sig)` signals every process the caller can reach;
  `kill(0, sig)` hits the caller's own process group. Both are rejected up
  front, on every platform, and both are covered by tests.
- **Actions are confirmed.** Kill, force-kill and container stop/kill/restart show a
  dialog naming the target first.
- **Tab-scoped keys don't leak across tabs.** `F9` on the Network tab does
  nothing rather than killing the process selected on a tab you cannot see.

### Files on disk

| Path | Mode | |
|---|---|---|
| data directory | `0700` | `~/.local/share/muxtop/` (Linux) |
| `server.key` | `0600` | generated private key |
| `server.crt`, `server.fingerprint` | `0644` | not secret |

Secret files are opened with `O_NOFOLLOW` on Unix, so a pre-planted symlink
cannot redirect the write somewhere else. The certificate gets the same
treatment even though it is public — the write itself is the thing being
protected.

---

## What muxtop does not defend against

Stated plainly, because a security page that claims everything claims nothing:

- **Docker socket access.** Anyone who can reach `/var/run/docker.sock` can
  start a privileged container and own the host. If muxtop can read it, so can
  anything else running as that user. Use rootless Podman.
- **Running as root.** Then muxtop has root's view and root's reach. It does not
  drop privileges.
- **`--tls-skip-verify`.** It disables certificate verification. That is what it
  is for; the name is the warning.
- **A leaked token.** It is a bearer credential. Whoever has it can read the
  metrics.
- **`--token` on a shared host.** Command-line arguments are world-readable via
  `/proc/<pid>/cmdline` — muxtop's own Processes tab will show it to other users.
  Use `--token-file`.
- **A compromised kubeconfig.** muxtop uses the credential it is given, with
  whatever permissions it has. Scope it with RBAC — see [Kubernetes](Kubernetes).
- **The host's own resource limits.** A 1 Hz refresh on a 5000-process machine
  costs CPU. See [Performance](Performance).

---

## Hardening checklist

For a `muxtop-server` deployment, in descending order of value:

- [ ] **Don't expose the port.** Bind to `127.0.0.1` and use an ssh tunnel —
      see [Remote monitoring](Remote-monitoring)
- [ ] Run as a **dedicated unprivileged user**, never root
- [ ] Token from `--token-file`, mode `0400`, generated with
      `openssl rand -base64 32`
- [ ] A real certificate with a `subjectAltName`; verify with `--tls-ca` on the
      client, never `--tls-skip-verify` outside a tunnel
- [ ] Disable what you don't watch: `--no-containers`, `--no-kube`, `--no-gpu`
- [ ] Scope what you keep: `--kube-namespace`, a rootless
      `--docker-socket`
- [ ] Lower `--max-clients` and `--rate-limit-per-ip` to what you actually need
- [ ] Firewall the port to known source addresses
- [ ] Use the hardened systemd unit, and read the note about which three
      directives break which features
- [ ] Never add the service user to the `docker` group

---

## Privacy

muxtop makes **no outbound network calls of its own** — no telemetry, no update
check, no crash reporting, no analytics. The only sockets it opens belong to a
feature you enabled: a container socket, a Kubernetes API server, or the
`--remote` connection you asked for. It is intended to be usable on an
air-gapped production host.

If you observe outbound traffic that is not one of those, that is a bug worth
[reporting](https://github.com/lucasschimmel/muxtop/issues).

Its logs are local, at `~/.local/share/muxtop/muxtop.log`, and default to
warnings and errors only. They can contain process command lines, container
names and hostnames — worth a glance before pasting one into an issue.

---

## Supply chain

The binary you run is part of the attack surface, so the pipeline that builds it
is hardened too:

- Release archives carry a **build-provenance attestation** — verify with
  `gh attestation verify <archive> --repo lucasschimmel/muxtop`. This answers a
  question a published checksum cannot: an attacker who can replace the archive
  can replace the `.sha256` beside it, but not the signature.
- Every third-party GitHub Action is **pinned to a commit SHA**, so a
  re-pointed tag cannot inject code into a release. Dependabot advances those
  SHAs under review.
- Workflow tokens are **read-only by default**; write scopes are granted job by
  job.
- `cargo deny` runs in CI **and daily**, so an advisory published against an
  already-shipped dependency opens an issue instead of waiting for the next
  commit.
- CodeQL analyses the workspace on every push and weekly.
- A release is gated on the tag matching the workspace version and on a
  CHANGELOG entry existing.

Details in the [release process](Release-process).
