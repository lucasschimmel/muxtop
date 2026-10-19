# muxtop

**A modern, multiplexed system monitor for the terminal.**

[![CI](https://github.com/lucasschimmel/muxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/lucasschimmel/muxtop/actions/workflows/ci.yml)
[![CodeQL](https://github.com/lucasschimmel/muxtop/actions/workflows/codeql.yml/badge.svg)](https://github.com/lucasschimmel/muxtop/actions/workflows/codeql.yml)
[![Advisories](https://github.com/lucasschimmel/muxtop/actions/workflows/advisories.yml/badge.svg)](https://github.com/lucasschimmel/muxtop/actions/workflows/advisories.yml)
[![Crates.io](https://img.shields.io/crates/v/muxtop.svg)](https://crates.io/crates/muxtop)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)

muxtop replaces the `htop` + `iftop` + `ctop` workflow with a single tabbed interface.
Think htop, but with multiplexer-style UX (à la tmux/zellij) and a VS Code-style command palette.

📖 **[Wiki](https://github.com/lucasschimmel/muxtop/wiki)** — remote monitoring over TLS, Kubernetes RBAC, container sockets, GPU backends, troubleshooting, the security model.

---

## Installation

### Via crates.io

```sh
cargo install muxtop
```

### Via Homebrew (macOS / Linux)

```sh
brew tap lucasschimmel/tap
brew install muxtop
```

### Via APT (Debian / Ubuntu)

```sh
# Add the repo (one time)
curl -fsSL https://lucasschimmel.github.io/apt/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/lucasschimmel.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/lucasschimmel.gpg] https://lucasschimmel.github.io/apt stable main" | sudo tee /etc/apt/sources.list.d/lucasschimmel.list

# Install
sudo apt update
sudo apt install muxtop
```

### Pre-built binary (Linux / macOS)

```sh
curl -sSfL https://raw.githubusercontent.com/lucasschimmel/muxtop/main/scripts/install.sh | sh
```

### From source

```sh
git clone https://github.com/lucasschimmel/muxtop.git
cd muxtop
cargo build --release
# Binary available at target/release/muxtop
```

> MSRV: Rust **1.88**

> **Platforms.** Linux and macOS (x86_64 and aarch64) are the supported targets — those are what the release pipeline publishes. The workspace also builds and passes its tests on Windows so the project can be developed there, but no Windows binaries are published and the process actions (`F7`/`F8` renice, `F9`/`F10` kill) are POSIX-only and return an error.

---

## Features

| Feature | Detail |
|---|---|
| **Tabs** | General, Processes, Network, Containers, Kubernetes and GPU — `Alt+1` … `Alt+6` |
| **Network tab** | Interface table with RX/s, TX/s, totals, errors + real-time sparklines |
| **Containers tab** | Docker/Podman via [bollard](https://github.com/fussybeaver/bollard) — CPU/memory/network/IO table, CPU+RX sparklines, `F9` stop / `F10` kill / `F11` restart actions, automatic socket detection |
| **Kubernetes tab** | Read-only Pods / Nodes / Deployments via [kube-rs](https://github.com/kube-rs/kube) — switch sub-views with `P` / `N` / `D`, sort with `s`, filter with `/`. Auto-detects `$KUBECONFIG` / `~/.kube/config` / in-cluster ServiceAccount; graceful fallback when `metrics-server` is absent (CPU/MEM render `—`). Lists cluster-wide by default; `--kube-namespace <NS>` scopes Pods and Deployments to one namespace so a namespace-bound Role is enough, and `A` toggles between the two at runtime (see [Kubernetes permissions](#kubernetes-permissions)) |
| **GPU tab** | Read-only NVIDIA (via NVML) and AMD (via `amdgpu` sysfs) — utilisation, VRAM, temperature, power, clocks, fan, NVENC/NVDEC. Switch sub-views with `D` / `P`, sort with `s`, filter with `/`. Multiple vendors are merged into one list, and any metric a driver cannot report renders `—` rather than a misleading `0` (see [GPU support](#gpu-support)) |
| **Command palette** | `Ctrl+P` fuzzy, `:` typed — `kill firefox`, `sort memory`, `stop nginx`, `restart postgres`, etc. |
| **Help screen** | `?` — generated from the keymap, so it always matches the bindings |
| **Inspector** | `Enter` on any row: full command line, image, pod node, GPU clocks, interface errors |
| **htop shortcuts** | `F1` help, `F5` tree, `F6` sort, `F7`/`F8` renice, `F9` kill, `F10` force kill |
| **Fuzzy search** | Powered by [nucleo](https://github.com/helix-editor/nucleo) (from the Helix editor) |
| **Tree view** | `t` / `F5` toggles the parent/child hierarchical display |
| **Renice** | `+` / `-` or `F7` / `F8` to adjust process priority |
| **Remote monitoring** | `--remote host:port` + `--token` to monitor a remote server over encrypted TLS |
| **Native TLS** | rustls encryption (TLS 1.3-only since 0.3.1), self-signed cert auto-generation (`--tls-generate`), mandatory token auth |
| **Async collection** | tokio-based — the UI never blocks, even at 3000+ processes |
| **Tokyo Night theme** | TrueColor, 256-color and 16-color renditions, plus a light and a monochrome theme (`--theme`) |
| **Degrades honestly** | ASCII glyph set on the Linux console, `NO_COLOR` honoured, mouse optional — see [Terminal compatibility](#terminal-compatibility) |
| **Static binary** | Single musl binary, no system dependencies |
| **Zero telemetry** | No client-side network calls, ever (see [Privacy](#privacy--telemetry)) |

---

## Kubernetes permissions

By default muxtop lists Pods, Nodes and Deployments across every namespace, which needs cluster-scoped `list` on all three. On a shared cluster you usually don't have that.

Pass `--kube-namespace <NS>` to scope Pods and Deployments to a single namespace — that works with a plain `Role` bound to it, no cluster-wide grant required. Press `A` in the Kube tab to switch between the scoped and cluster-wide views at runtime (local mode only; in `--remote` mode the server's `--kube-namespace` decides).

**Nodes are cluster-scoped in Kubernetes** — there is no namespaced variant of the resource, so the Nodes sub-view always needs cluster-wide access and renders empty without it. The Pods and Deployments views are unaffected. The same split applies to metrics: pod CPU/MEM follows the namespace scope, node CPU/MEM does not.

A minimal namespace-scoped Role:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: muxtop-readonly
  namespace: my-namespace
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["list"]
  - apiGroups: ["apps"]
    resources: ["deployments"]
    verbs: ["list"]
  - apiGroups: ["metrics.k8s.io"]
    resources: ["pods"]
    verbs: ["list"]
```

---

## GPU support

muxtop probes for GPUs at startup and needs **no extra privileges** — NVML is
readable by any user, the `amdgpu` sysfs nodes are world-readable, and the macOS
counters are readable unprivileged too (`powermetrics` needs root; the channels
muxtop reads do not). Nothing is installed and no vendor SDK is required at
build time: the NVIDIA library and the macOS `IOReport` library are both loaded
dynamically at runtime, so the same binary runs with or without them. Disable
the probe with `--no-gpu`.

| Vendor | Backend | Platforms | Status |
|---|---|---|---|
| **NVIDIA** | NVML (`libnvidia-ml.so` / `nvml.dll`, loaded at runtime) | Linux, Windows | Full — including per-process usage |
| **AMD** | `amdgpu` sysfs (`/sys/class/drm/card*/device`) | Linux | Devices only — no per-process usage |
| **Apple Silicon** | IOKit `IOAccelerator` + `IOReport` (loaded at runtime) | macOS | Devices only — no per-process usage, no temperature |
| **Intel** | — | — | Not implemented |

### What each backend can and cannot report

Unlike CPU or memory, no GPU metric is universally available, so every field is
optional and the UI renders an unavailable one as `—`. **A `—` means "this
driver cannot report this", not "zero"** — conflating the two would make the tab
lie about an idle GPU.

- **AMD has no per-process accounting.** The `amdgpu` sysfs interface exposes no
  equivalent of NVML's process queries, so the Procs sub-view says so explicitly
  instead of showing an empty list that reads as "nothing is using the GPU".
- **Encoder/decoder utilisation is NVML-only.** AMD renders `—` for that column.
- **On Windows, per-process GPU memory is unavailable and every process shows
  `both`.** Under the WDDM driver model the OS owns video-memory allocation, so
  NVML reports the per-process figure as unavailable; it also returns identical
  compute and graphics process lists, so the TYPE column carries no information
  there. Both behaviours are the driver's, and both are reported honestly rather
  than papered over. On Linux the figures and the distinction are real.
- **Fanless cards report no fan**, and a laptop dGPU parked in runtime-D3 may
  report nothing at all until something wakes it.
- **Apple Silicon reports no per-process usage and no GPU temperature.** There
  is no Apple equivalent of NVML's process queries, and the GPU thermal channels
  read zero for an unprivileged process, so the tab shows `—` rather than a
  confident 0 °C on a warm laptop. Power caps, fan and encoder/decoder counters
  are likewise not published: the SoC power budget is shared with the CPU and
  managed by firmware, and cooling is chassis-wide rather than per-GPU.

### Apple Silicon: unified memory is not VRAM

The GPU addresses the same physical memory as the CPU. The Devices table
therefore shows a **MEM** column rather than VRAM, reading the driver's
GPU-resident bytes against the machine's whole pool — on a 16 GB Mac, a GPU
holding 2 GB shows 12 %, and the CPU is competing for the other 88 %. The
Inspector spells it out as "Unified memory".

Two sources are read, and they degrade independently:

| Source | Gives | If it fails |
|---|---|---|
| IOKit `IOAccelerator` (public API) | device name, GPU cores, driver, utilisation, memory | no Apple GPU is reported |
| `IOReport` (private, `dlopen`ed) | power, clock | those two columns render `—` |

`IOReport` is a private framework with no stability promise, so its symbols are
resolved at runtime exactly as NVML's are. A macOS release that renames them
costs two columns, not the tab.

**Intel Macs are not covered.** Their AMD or Intel GPU answers the same IOKit
match with a differently shaped statistics dictionary, and muxtop decodes only
Apple's own driver family. The tab says so rather than showing a row labelled
Apple that reports nothing.

---

## Privileges

Access to the Docker socket (`/var/run/docker.sock`) is **equivalent to root access** on the host machine: any user in the `docker` group can launch a privileged container and break out. To run muxtop with a minimal privilege budget, use **rootless Podman** — the user-scoped socket (`$XDG_RUNTIME_DIR/podman/podman.sock`) is isolated per user and muxtop detects it automatically. Avoid running `muxtop-server` as root on an exposed host: prefer a service account with only the rootless Podman socket mounted read/write.

---

## Usage

```sh
muxtop                              # normal launch (auto-detects Docker/Podman)
muxtop --refresh 2                  # refresh every 2 seconds
muxtop --filter firefox             # start with a process filter
muxtop --sort mem                   # sort by memory at startup
muxtop --tree                       # start in tree view
muxtop --about                      # version, license, privacy pledge

# Presentation
muxtop --theme tokyo-night-light    # light terminal background
muxtop --theme mono                 # no hue at all
muxtop --no-color                   # same as NO_COLOR=1
muxtop --ascii                      # force the ASCII glyph set
muxtop --no-mouse                   # keep the terminal's own text selection

# Containers tab — by default muxtop checks $DOCKER_HOST, /var/run/docker.sock,
# then the Podman sockets. Pass a path to force, or disable entirely:
muxtop --docker-socket /var/run/docker.sock   # socket override
muxtop --no-containers                        # disable container collection

# Kubernetes tab — by default muxtop reads $KUBECONFIG, then ~/.kube/config,
# then falls back to in-cluster ServiceAccount credentials. Override or disable:
muxtop --kube-context kind-kind               # use a specific kubeconfig context
muxtop --kube-namespace kube-system           # scope Pods/Deployments to one namespace
muxtop --no-kube                              # disable cluster collection entirely

# GPU tab — NVIDIA (NVML) and AMD (amdgpu sysfs) are probed automatically.
muxtop --no-gpu                               # disable GPU detection entirely

# Run the server (TLS + auth required)
muxtop-server --token "my-secret-16chars" --tls-generate
muxtop-server --token "my-secret-16chars" --tls-cert cert.pem --tls-key key.pem
muxtop-server --token "my-secret-16chars" --tls-generate --bind 0.0.0.0:4242 --max-clients 10

# Remote monitoring (TLS)
muxtop --remote host:port --token "my-secret-16chars" --tls-skip-verify  # dev
muxtop --remote host:port --token "my-secret-16chars" --tls-ca cert.pem  # production
MUXTOP_TOKEN="my-secret-16chars" muxtop --remote host:port --tls-ca cert.pem
```

### Keyboard shortcuts

**Press `?` inside muxtop** for the full, always-current reference — it is generated from the keymap
itself, so it cannot drift from what the keys actually do. The table below is the short version.

| Key | Action |
|--------|--------|
| `?` · `F1` | Help |
| `Ctrl+P` · `Ctrl+K` | Command palette (fuzzy) |
| `:` | Command mode — `kill firefox`, `sort mem`, `filter ngin`, `theme mono`, `tab gpu` |
| `Tab` / `Shift+Tab` | Cycle to the next / previous tab |
| `Alt+1` … `Alt+6` | Switch tab (General / Processes / Network / Containers / Kubernetes / GPU) |
| `q` · `Ctrl+C` | Quit |
| `j` / `k` · `↑` / `↓` | Move the row cursor |
| `h` / `l` · `←` / `→` | Scroll columns (narrow terminals) |
| `g` / `G` · `Home` / `End` | Jump to first / last row |
| `PageUp` / `PageDown` · `Ctrl+U` / `Ctrl+D` | Scroll by a page / half a page |
| `Enter` · `i` | Inspect the selected row |
| `x` | Contextual actions menu |
| `y` | Copy the selected row's identifier (works over ssh, via OSC 52) |
| `Space` | Pause / resume the view |
| `Ctrl+L` | Message log |
| `/` · `F3` / `F4` | Filter (applies to the active tab) |
| `Esc` | One step back: close overlay → dismiss messages → leave the filter → clear it |
| `s` · `F6` | Cycle sort field (active tab) |
| `S` / `I` | Reverse sort direction (active tab) |
| `t` · `F5` | Tree view (Processes) |
| `F7` / `F8` · `-` / `+` | Renice — lower / raise priority (Processes, local mode) |
| `F9` | Kill process, SIGTERM (Processes) · Stop container (Containers) |
| `F10` | Force kill, SIGKILL (Processes) · Kill container (Containers) |
| `F11` | Restart container (Containers) |
| `P` / `N` / `D` · `[` / `]` | Switch Kube sub-view to **P**ods / **N**odes / **D**eployments |
| `A` | Toggle namespace scope — one namespace ↔ **A**ll namespaces (Kubernetes tab, local mode) |
| `D` / `P` · `[` / `]` | Switch GPU sub-view to **D**evices / **P**rocs (GPU tab only) |

Tab-scoped keys only act on their own tab: pressing `F9` on the Network tab does nothing rather than
silently killing a process you cannot see.

### Terminal compatibility

muxtop adapts to the terminal it finds, because a headless Ubuntu box reached over `ssh` and a kitty
window are both first-class targets:

| Detected | Behaviour |
|---|---|
| 24-bit colour (`$COLORTERM`, or a known terminal) | Full Tokyo Night |
| 256 colours (`xterm-256color`, Terminal.app, tmux) | 256-colour Tokyo Night |
| 16 colours (`TERM=linux`, `xterm`, serial console) | ANSI palette, the terminal's own colours |
| `NO_COLOR`, `TERM=dumb` | No colour at all — hierarchy through bold / dim / reverse |
| No UTF-8 locale, or `TERM=linux` | ASCII glyph set (the console font has no block or braille glyphs) |
| No pointer (`TERM=linux`, `dumb`) | Mouse reporting stays off |

Everything the mouse can do, the keyboard can do. Overrides: `--theme <name>` (`tokyo-night`,
`tokyo-night-light`, `mono`), `--no-color`, `--ascii`, `--no-mouse`.

---

## Benchmarks

Measured on 2026-10-19 with Rust 1.95.0 on a MacBook Air M3 (8 GB,
macOS 26.6.2), with ~500 processes, via the Thomas benchmark. Timings and
memory figures are medians of five runs with their observed ranges beside them — a single sample of peak memory varies by more than a megabyte
depending on how many processes are alive on the host.

| Metric | Target | muxtop v0.8 | was (v0.7.0) |
|----------|-------|--------|--------|
| Startup (`--about`) | < 100 ms | **5.85 ms** (5.70–7.63) | 5.67 ms |
| Binary size | < 10 MiB | **7.97 MiB** (LTO + strip) | 8.03 MiB |
| FPS (TUI) | > 30 | ~60 (event-driven, idle ≈ 0 redraws) | — |
| Peak RSS (30 s) | < 15 MiB | **12.98 MiB** (12.12–13.05) | 12.55 MiB |
| Peak footprint (30 s) | — | **6.56 MiB** (6.34–6.72) | 5.77 MiB |

**Two memory numbers, because macOS reports two.** Peak RSS is what `top`
shows, and it counts clean, shared, file-backed pages of the dyld shared cache
— the text of every system library the process touched, shared with everything
else running and never written. Physical footprint is what the kernel actually
bills the process. An earlier sample read 12.56 MiB and 5.66 MiB. RSS is the
number comparable with htop (≈ 15) and btop (≈ 40); footprint is the half
muxtop can actually reduce. Both are published. The headless macro does not
measure interactive FPS; the FPS row describes the nominal event-driven rendering cadence.

**The v0.7.0 column is a historical remeasurement** on macOS 26.5.1 with
~430 processes. The original v0.7.0 figures used a single-spawn stopwatch;
the replacement harness amortises timing overhead over 200 execs per window.
The OS, dependency graph and host load differ between columns, so these are
release observations rather than a controlled A/B comparison.

**The earlier, isolated crypto-provider change saved 1.375 MiB** before the
kube migration (8.03 to 6.66 MiB). rustls 0.23
defaults to the `aws-lc-rs` crypto provider, and `default-features = false` on
`kube` deselected the `ring` it would otherwise have used, so the binary
carried 927 KiB of C and assembly — ML-DSA and ML-KEM among it — that no
muxtop handshake can reach. Pinning `ring` explicitly drops `aws-lc-sys` from
the dependency graph entirely. It costs post-quantum key exchange, which for a
channel carrying system metrics with a lifetime of seconds is a trade this
project is willing to make. The final 7.97 MiB binary meets the public budget
but exceeds the unchanged 7 MiB contributor ceiling; that remains release
review debt.

Run the benchmark yourself:

```sh
just bench-thomas
# or
./scripts/bench-thomas.sh
```

---

## Architecture

```
muxtop/
├── src/                         # Entry point (clap CLI + tokio bootstrap)
└── crates/
    ├── muxtop-core/             # System collection, data models, actions
    │   ├── src/collector.rs     # 4 async loops: sysinfo 1 Hz, containers 0.5 Hz, cluster 0.2 Hz, gpu 1 Hz
    │   ├── src/process.rs       # Sort, filter, process tree
    │   ├── src/system.rs        # CPU / memory / load snapshots
    │   ├── src/network.rs       # Network interfaces + history
    │   ├── src/containers.rs    # Container model (ContainerSnapshot, states, engine)
    │   ├── src/container_engine.rs # Async trait + Docker/Podman socket detection
    │   ├── src/docker_engine.rs # Concrete bollard-backed implementation
    │   ├── src/kube.rs          # Pod / Node / Deployment / Cluster snapshots
    │   ├── src/cluster_engine.rs # Async trait + kubeconfig detection
    │   ├── src/kube_engine.rs   # Concrete kube-rs-backed implementation
    │   ├── src/gpu.rs           # GPU device / process snapshots (every metric optional)
    │   ├── src/gpu_engine.rs    # Async trait + composite that merges vendor backends
    │   ├── src/nvml_engine.rs   # NVIDIA backend (NVML, dynamically loaded)
    │   └── src/amd_engine.rs    # AMD backend (amdgpu sysfs)
    ├── muxtop-tui/              # ratatui interface
    │   ├── src/app.rs           # State machine, event handling
    │   ├── src/keymap.rs        # Single source of truth for bindings (drives dispatch + help)
    │   ├── src/notify.rs        # Typed toast stack and message log
    │   ├── src/ui/widgets/      # Shared table, meters, sparklines, badges, empty states
    │   └── src/ui/              # Tabs General/Processes/Network/Containers/Kube/GPU, palette, theme
    ├── muxtop-proto/            # Wire protocol and binary serialization
    └── muxtop-server/           # TCP daemon for remote monitoring
```

---

## Development

```sh
just check    # fmt + clippy + deny + tests
just bench    # criterion micro-benchmarks
just dev      # continuous check with bacon
```

---

## Roadmap

| Version | Goal |
|---------|----------|
| **v0.1** ✓ | htop replacement — tabs, command palette, tree view |
| **v0.2** ✓ | Network tab (replaces iftop) + client/server architecture (`muxtop-server`, `--remote`) |
| **v0.3** ✓ | Docker / Podman Containers tab (via [bollard](https://github.com/fussybeaver/bollard)) + Stop/Kill/Restart actions |
| **v0.3.1** ✓ | TLS 1.3 hardening, per-IP rate limit, ANSI sanitizer, event-driven render, `lto=fat` build sweep |
| **v0.4** ✓ | Kubernetes Pod tab (read-only) via [kube-rs](https://github.com/kube-rs/kube), kubeconfig auto-detection, metrics-server graceful degradation |
| **v0.5** ✓ | GPU tab — NVIDIA via NVML, AMD via `amdgpu` sysfs, per-process usage, graceful per-metric degradation |
| **v0.6** ✓ | Security and performance hardening — bounded rate limiter, shared cluster snapshots, cached Kube view, narrowed process collection |
| **v0.7** ✓ | Apple Silicon GPU support — IOKit `IOAccelerator` + `IOReport`, unprivileged, unified-memory reporting |
| **v0.8** ✓ | Reliability and performance hardening — collector freshness, TUI selection, remote recovery, bounded logs, kube 4 migration and allocation/binary optimisation |
| Future | Interactive `docker exec` (PTY) |
| v1.0 | WASM plugin system + themes + configuration file |

---

## Privacy & telemetry

muxtop collects **NO** telemetry, **NO** statistics and contacts **NO ONE**. Ever.

It makes no outbound network calls of its own. It is designed for air-gapped production servers.
If you observe outbound activity from muxtop that isn't tied to a feature you've enabled, that is a bug — please [report it](https://github.com/lucasschimmel/muxtop/issues).

### Read-only by design

When the Kubernetes or Containers tab is active, muxtop only **reads** from the corresponding API:
- Kubernetes : `LIST` on Pods / Nodes / Deployments + `GET` on `metrics.k8s.io/v1beta1`, scoped to one namespace or cluster-wide per [Kubernetes permissions](#kubernetes-permissions). No `CREATE` / `UPDATE` / `DELETE` / `PATCH` is ever issued. Write actions (Delete pod, Scale deployment, Rollout restart) are explicitly out of scope for v0.4.
- Containers : `GET /containers/json` + `/stats?stream=false`. The Stop / Kill / Restart actions are gated behind a confirmation dialog and are local-only — disabled in `--remote` mode.
- GPU : NVML query calls and reads of `/sys/class/drm/card*/device`. Both are read-only by construction — muxtop never sets a clock, a power limit or a fan curve, and the GPU tab has no actions at all. `--no-gpu` skips the probe entirely.

### Remote mode and credentials

In `--remote` mode, the **server** is the only side that opens kubeconfig or Docker socket files. Credentials never traverse the wire — only the digested snapshots do. Anti-leak guards in the test suite (`muxtop-proto/tests/integration.rs`) verify byte-for-byte that no `BEGIN PRIVATE KEY`, `Bearer ` token, or `client-key-data:` field appears in any encoded frame.

---

## Documentation

| | |
|---|---|
| **[Wiki](https://github.com/lucasschimmel/muxtop/wiki)** | Operational guides — the pages below, and more |
| [Installation](https://github.com/lucasschimmel/muxtop/wiki/Installation) | All five install methods, verifying a download, uninstalling |
| [Remote monitoring](https://github.com/lucasschimmel/muxtop/wiki/Remote-monitoring) | Certificates, tokens, a hardened systemd unit, firewalling |
| [Security model](https://github.com/lucasschimmel/muxtop/wiki/Security-model) | Trust boundaries, what is *not* defended, hardening checklist |
| [Kubernetes](https://github.com/lucasschimmel/muxtop/wiki/Kubernetes) | Minimal RBAC for the namespaced and cluster-wide cases |
| [Containers](https://github.com/lucasschimmel/muxtop/wiki/Containers) | Socket detection, rootless Podman, why the tab is empty |
| [GPU](https://github.com/lucasschimmel/muxtop/wiki/GPU) | NVML and `amdgpu` backends, and what each cannot report |
| [Troubleshooting](https://github.com/lucasschimmel/muxtop/wiki/Troubleshooting) | Symptom → cause, and where the logs are |
| [Architecture](https://github.com/lucasschimmel/muxtop/wiki/Architecture) | For contributors and auditors |

The wiki is generated from [`docs/wiki/`](docs/wiki) — send documentation fixes as pull requests against those files, not as browser edits.

---

## Security

Found a vulnerability? **[Report it privately](https://github.com/lucasschimmel/muxtop/security/advisories/new)** — never as a public issue. Scope, response targets and the threat model are in [SECURITY.md](SECURITY.md).

Release archives carry a build-provenance attestation, so you can verify a download came from this repository's release workflow rather than only that it downloaded intact:

```sh
gh attestation verify muxtop-x86_64-unknown-linux-musl.tar.gz --repo lucasschimmel/muxtop
```

---

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for prerequisites, code conventions, the branch workflow and PR submission instructions. Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

---

## License

Available under either of the following licenses, at your option:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))
