# Architecture

For contributors, and for anyone auditing muxtop before deploying it. Setup,
conventions and the PR workflow are in
[CONTRIBUTING.md](https://github.com/lucasschimmel/muxtop/blob/main/CONTRIBUTING.md).

## The workspace

Four library crates plus a binary, Rust edition 2024, MSRV 1.88.

```
muxtop-core     Collection engine. Data models, the async loops, system actions.
                Knows nothing about rendering.
muxtop-tui      ratatui interface. Knows nothing about how data was collected.
muxtop-proto    Wire protocol and binary serialisation for remote mode.
muxtop-server   TCP daemon: muxtop-core + TLS + auth, no UI.
muxtop (bin)    src/main.rs — clap CLI, tokio bootstrap, wiring.
```

Three of these — `muxtop-core`, `muxtop-proto`, `muxtop-tui` — are published to
crates.io, which makes their public API a contract. The `Semver` CI job checks
it.

The rule that keeps the boundary honest: **collection belongs in
`muxtop-core`, rendering in `muxtop-tui`.** When it is unclear where new code
goes, that is the tiebreaker. It is also what makes `muxtop-server` possible —
it reuses the whole collection engine with no UI attached.

## Collection

Four independent `tokio` loops, each at the rate its source deserves. The UI
never waits on any of them: it reads the latest published snapshot and renders.

| Loop | Rate | Source |
|---|---|---|
| System / processes | 1 Hz | `sysinfo` |
| Containers | 0.5 Hz | Docker/Podman HTTP API via `bollard` |
| Cluster | 0.2 Hz | Kubernetes API via `kube-rs` |
| GPU | 1 Hz | NVML (dynamically loaded) and `amdgpu` sysfs |

Consequence worth knowing: a hung API server freezes that tab's data and nothing
else. There is no shared lock between a slow collector and your keyboard.

### Engines are traits

Containers, clusters and GPUs each sit behind an async trait with a concrete
implementation and a detection path:

```
container_engine.rs   trait + socket detection    → docker_engine.rs   (bollard)
cluster_engine.rs     trait + kubeconfig detection → kube_engine.rs    (kube-rs)
gpu_engine.rs         trait + a composite that merges vendors
                                                  → nvml_engine.rs     (NVIDIA)
                                                  → amd_engine.rs      (AMD sysfs)
                                                  → apple/             (IOKit + IOReport)
```

The GPU composite is why an NVIDIA and an AMD card appear in one list, and it is
what made adding Apple Silicon in v0.7 plumbing rather than redesign: the data
model, the wire format and the tab already carried the `Apple` variants, so the
release shipped without a protocol break.

Every optional metric is an `Option` all the way through the model. That is what
lets the UI render `—` for "the driver cannot report this" instead of a `0` that
would be a lie.

## The TUI

```
app.rs              State machine and event handling
keymap.rs           Single source of truth for bindings
notify.rs           Typed toast stack and message log
ui/widgets/         Shared table, meters, sparklines, badges, empty states
ui/                 The six tabs, palette, inspector, theme
ui/sanitize.rs      Control-byte scrubber for externally-sourced strings
```

**`keymap.rs` is a table, not a `match`.** Every binding declares its key, its
scope (global or one tab), its group, its display string and its label. Dispatch
reads that table; so does the `?` help screen and so do the footer hints. A
binding that is not in the table does not exist, and the help screen cannot drift
from behaviour because they are the same data. Tests assert every binding is
documented and that tab-scoped bindings shadow global ones only on their own tab.

**`ui/widgets/` exists because five hand-rolled tables were five places to fix a
bug.** New tabs compose the shared table, meter and sparkline widgets.

**Rendering is event-driven.** A repaint happens on a key press, a mouse event, a
resize, or a new snapshot. An idle muxtop does approximately zero redraws.

## Remote mode

```
muxtop-server                                    muxtop --remote
  Collector (muxtop-core)
    → snapshot
      → muxtop-proto encode
        → TLS 1.3 (rustls) ────────────────────────→ decode
                                                     → the same UI
```

Two invariants:

1. **Only digested snapshots cross the wire.** The server is the only side that
   opens a kubeconfig, a container socket or a TLS key. Integration tests in
   `crates/muxtop-proto/tests/integration.rs` assert byte-for-byte that no
   `BEGIN PRIVATE KEY`, `Bearer ` or `client-key-data:` appears in an encoded
   frame.
2. **Host-mutating actions are local-only.** `Action::is_local_only()` is the
   single place that decides, and it is unit-tested.

Serialisation is `bincode` over a length-framed protocol. `deny.toml` documents
why the unmaintained-crate advisory against bincode v2 is accepted, and what the
alternatives would be.

## System actions

`muxtop-core/src/actions.rs` isolates every `unsafe libc` call in the codebase.
It is small on purpose — it is the highest-consequence file in the workspace.

- **PID validation runs before the platform split**, so its guarantees hold
  identically everywhere and stay covered by tests on every platform. A PID
  above `i32::MAX` would wrap negative, and `kill(-1, sig)` signals every
  reachable process while `kill(0, sig)` hits the caller's process group. Both
  are rejected before any syscall.
- **Signals come from an enum**, not an `i32`. Only SIGTERM and SIGKILL exist.
- **Non-POSIX platforms get stubs** with identical signatures that fail with
  `ErrorKind::Unsupported`. That is what lets the workspace compile on Windows —
  and Windows is in CI purely as a regression guard against an ungated `libc::`
  call, not as a support claim.

## Platform boundaries

The pattern to copy when adding platform-specific code:

```rust
#[cfg(unix)]
pub fn thing(…) -> Result<(), CoreError> { /* real */ }

#[cfg(not(unix))]
pub fn thing(…) -> Result<(), CoreError> { Err(unsupported("thing")) }
```

Same signature, honest failure, workspace still compiles. `actions.rs` is the
reference implementation.

## The k8s-openapi feature gate

The workspace uses kube 4.0.x with k8s-openapi 0.28 and the Kubernetes 1.32
bindings. kube 4.0 removes the deprecated YAML parser while retaining our
Rust 1.88 MSRV; kube 4.2 requires Rust 1.89. The engine uses direct polling,
so kube's runtime feature is disabled. kube's built-in request retries are
also disabled: our poll cadence owns retries, and `LIST_TIMEOUT` bounds
each resource, metrics and version request. Generated timestamps use jiff;
only digested scalar ages enter our unchanged snapshot models.

The one build wrinkle every contributor hits:

```sh
cargo check --workspace --all-targets   # ✅ dev-deps enable v1_32
cargo check --workspace                 # ❌ fails on the muxtop-core lib build
```

`k8s-openapi` refuses to compile a library crate without a `v1_*` feature, and
equally refuses to let library crates enable one in `[dependencies]`. So only the
leaf binaries and `muxtop-core`'s dev-dependencies enable `v1_32`, and
`--all-targets` is what activates them.

Where dev-dependencies are not available — `cargo publish`, `cargo doc`,
`cargo-semver-checks` — the `K8S_OPENAPI_ENABLED_VERSION=1.32` environment
variable is the documented escape hatch, and the workflows set it. `just check`
wraps the correct invocation.

## Testing

```sh
just check      # fmt + clippy + deny + tests — what CI runs
just test
```

Unit tests live in `#[cfg(test)]` modules beside the code; integration tests in
each crate's `tests/`. Two suites are `#[ignore]`d because they need
infrastructure:

```sh
cargo test -p muxtop-core --lib docker_engine -- --ignored   # needs Docker/Podman
cargo test -p muxtop-core --lib kube_engine   -- --ignored   # needs a cluster
```

The default suite touches no live cluster: the Kubernetes unit tests build typed
`Pod` / `Node` / `Deployment` objects with `serde_json::from_value`, so they run
on hosts with no kubeconfig. `kind create cluster` is the shortest path to
running the ignored ones.

CI covers Ubuntu, macOS and Windows. See [Release process](Release-process) for
what happens after a merge.
