# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-10-19

Reliability, dependency maintenance and performance release. Docker/Kubernetes
collection, TUI selection and remote reconnect/shutdown behavior are corrected;
kube 4.0 removes the deprecated YAML dependency while retaining Rust 1.88.

**Source API:** the hidden benchmark helper now uses k8s-openapi 0.28 Pods,
which requires this minor bump. **Wire compatibility:** snapshot schemas and
encoding are unchanged from v0.7.0; mixed v0.7/v0.8 client/server pairs retain
the same protocol. Interactive `docker exec` uses a local daemon attachment,
separate from the monitoring protocol.

The optimization measurements below were taken before the kube migration;
final release validation and measurements are recorded separately.

### Added

- **Interactive container shell with a PTY** (`core`, `tui`). Press `e` on a
  running container, or choose the shell action from `x` / `Ctrl+P`, to attach
  `/bin/sh` through the existing Docker/Podman daemon connection. Linux/macOS
  local mode only, without a Docker CLI dependency or privileged exec. Raw input
  (including Ctrl+C), terminal resize, bounded writes and shell exit status are
  supported. `exit` returns to muxtop; `Ctrl+]` closes the attachment without
  promising to terminate the remote process. Errors restore the TUI. Remote
  monitoring and snapshot/wire layouts are unchanged.

### Performance

- **The Apple GPU backend subscribes to 3 IOReport channels instead of 150**
  (`core`). `IoReportSampler` copied the whole `Energy Model` group to read one
  counter. The v0.7.0 notes guessed that group at "roughly eighty CPU
  channels"; measured on an M3 it is **149**, one per power rail on the SoC.
  The group cannot be narrowed with `IOReportCopyChannelsInGroup`'s subgroup
  filter the way `GPU Stats` is — every `Energy Model` channel reports an empty
  subgroup, so there is nothing to filter on — so the channel set is now
  assembled directly from the two channels `decode` actually reads plus the
  DVFS residency channel.

  **The expected saving did not materialise, and is not claimed.** This was
  picked as the most likely first megabyte of the Apple backend's footprint.
  Measured A/B over five 30 s runs each, peak RSS is 12.41 MiB against 12.28
  MiB and peak footprint 5.70 MiB against 5.56 MiB — both within run-to-run
  spread, in the wrong direction if anything. Whatever the backend's ~1.5 MiB
  of private footprint is, it is not these channels, and that remains open.

  **What it does buy is CPU.** `decode` walks every channel of every delta and
  converts each one's group name into an owned `String`, so the wide
  subscription spent ~150 allocations and string comparisons per tick to find
  two channels. `AppleEngine::snapshot` goes from **3.63 ms to 3.06 ms**
  (median of 60 calls, −16 %) on an M3. It also drops a private symbol:
  `IOReportMergeChannels` is no longer resolved, so one fewer undocumented
  entry point has to survive a macOS release for the backend to start.

  Behaviour is unchanged and verified against the running backend: power and
  clock still read `—` on the first tick and real values afterwards
  (0.42 W, 338 MHz), utilisation and memory come from the IORegistry and are
  untouched, and temperature stays `—` as documented.

- **Building a process's COMMAND column no longer allocates once per argument**
  (`core`). `SystemSnapshot::collect` rendered argv with
  `.map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")`,
  which allocates one `String` per argument, plus the `Vec` holding them, plus
  the joined result — then frees all but the last. That is `argc + 2`
  allocations per process, for every process on the host, at the collector's
  1 Hz. `to_string_lossy` already borrows whenever the bytes are valid UTF-8,
  which every ordinary command line is, so the arguments are now pushed into
  one pre-sized buffer and the cost is a single allocation regardless of
  `argc`. Measured on a MacBook Air M3 with 400 processes averaging 5.5
  arguments: **2 999 allocations per tick down to 400** — ~2 600 fewer per
  second — and `SystemSnapshot::collect` improves by **2.0 %** (6.77 ms to
  6.63 ms, p < 0.05). The rest of that time is sysinfo's own refresh, which
  dominates the function.

  The other three per-process strings — `name`, `user` and `status` — are
  deliberately left alone. Each is a single allocation that `ProcessInfo` has
  to own, and `status` in particular copies a `&'static str` onto the heap
  every tick for want of anywhere else to put it. Removing those means changing
  `ProcessInfo`'s field types, and those fields are on the wire: bincode encodes
  a `String` as a length followed by its bytes, so swapping one for an enum or
  an interned handle changes the byte sequence and breaks client/server
  interop. Three allocations per process is not worth a wire break.

- **The binary is 1.375 MiB smaller — 8.03 MiB to 6.66 MiB** (`deps`). rustls
  0.23 selects `aws-lc-rs` as its crypto provider by default, and
  `default-features = false` on `kube` — added in v0.4 to trim the feature set —
  silently deselected the `ring` that kube 0.99 would otherwise have chosen.
  Cargo then unified `tokio-rustls`'s own `aws_lc_rs` default across the graph,
  so the `rustls` shared by kube-client and hyper-rustls resolved to aws-lc.
  Nothing in this workspace ever asked for it. The result was **927 KiB of C
  and assembly** in the binary, 242 KiB of which is ML-DSA and ML-KEM —
  post-quantum code that no muxtop handshake can reach, since no certificate
  authority issues ML-DSA certificates. Pinning `ring` on both `kube` and
  `tokio-rustls` removes `aws-lc-sys` from the dependency graph entirely.

  **What it costs: post-quantum key exchange.** ring does not implement ML-KEM,
  so a muxtop-server session no longer negotiates a hybrid key exchange. The
  channel carries system metrics with a useful lifetime measured in seconds,
  which makes harvest-now-decrypt-later a poor threat model to optimise for.
  TLS 1.3 itself is unaffected. The secondary benefit is that `aws-lc-sys`
  needs a C toolchain and cmake at build time, which the static musl build was
  paying for.

  **What it does not change: memory or startup.** Peak RSS is 12.56 MiB against
  12.55 MiB before, and startup is unchanged within run-to-run spread. The code
  removed was never paged in — it only executes during a TLS handshake, which
  the benchmark does not perform. Binary size and resident memory are close to
  independent here, and reporting the size win as a memory win would be wrong.

### Fixed

- **Container CPU and uptime stay accurate across samples and restarts**
  (`core`). CPU deltas retain the daemon's full container ID. Uptime uses
  the actual start time, and remains unknown when inspect is unavailable.
- **Kubernetes failures no longer leave cached resources looking fresh**
  (`core`). Failed resource categories are cleared, expired collections are
  reported stale, and replies from an earlier namespace scope are discarded.
  Multi-file `KUBECONFIG` values are split using the platform separator and
  merged with kube's configuration semantics.
- **Inspectors and copy actions follow the displayed row** (`tui`). Network,
  GPU and Kubernetes selection respects sorting and filtering; shrinking
  snapshots clamp selections and scroll offsets. Deferred process filters
  also finish while paused, and textual sorting targets the active subview.
- **Remote availability remains visible through disconnect and recovery**
  (`proto`, `tui`). Ordered connection events keep old samples stale until
  a fresh sample arrives after reconnect. Silent sessions expire after three
  heartbeat intervals. Server writes and handshakes are cancellable and
  bounded; shutdown joins client tasks, and closing the collector receiver
  stops its secondary loops.
- **Diagnostic logs are bounded** (`cli`, `server`). A nonblocking worker
  writes a single log segment capped at 2 MiB, and admission rejection counts
  are aggregated instead of logging each rejected connection.

- **The benchmark was measuring itself** (`ci`). Two of the three figures
  `scripts/bench-thomas.sh` published described the harness rather than the
  binary.

  Startup was timed by wrapping a *single* exec in two `date` calls, charging
  the measurement for two process spawns. On a MacBook Air M3 two back-to-back
  `date` calls with nothing between them cost 6–19 ms — and the script reported
  ~12 ms for a binary that starts in 5.7 ms. About half of every published
  startup figure was the stopwatch. That made the metric useless rather than
  flattering: any change worth less than the noise floor was invisible, and
  that is every change an optimisation release makes. Startup is now timed over
  200 execs inside one window, and the harness's own per-exec share — 43 µs —
  is printed beside the result instead of being left to trust.

  Peak RSS on macOS is mach's `resident_size`, which counts clean, file-backed,
  **shared** pages of the dyld shared cache: the text of every system library
  the process touched, shared with every other process and never written by us.
  On the same run it reads 12.56 MiB where `phys_footprint` — what the kernel
  actually bills — reads 5.66 MiB. Both are now reported. RSS was deliberately
  **not** replaced: swapping the metric would have dropped the headline number
  by 7 MiB without saving a byte, and it is the only figure comparable with the
  htop and btop numbers the README cites.

  Every figure is now a median of five runs printed with its observed range.
  Single samples of peak RSS vary by ±1.5 MiB depending on how many processes
  are alive on the host — wider than most of the savings v0.8 is chasing.

- **The published performance figures were wrong in both directions**
  (`docs`). `docs/wiki/Performance.md` still carried v0.3.1's 5.3 MiB binary
  and 11.3 MiB RSS, four releases after that binary stopped existing — the
  README was corrected for v0.7.0 and the wiki was not. Both now carry
  re-measured v0.8 figures, the machine they came from, and the ranges. The
  contributor ceilings and the published budgets are also now shown in one
  table with the relationship between them stated, since the two lived in
  separate documents that each looked like the only one.

### Security

- Update the lockfile to rustls 0.23.46 and rustls-webpki 0.103.15 while
  retaining the explicit ring provider.
- Migrate kube 0.99 to 4.0 and k8s-openapi 0.24 to 0.28, removing
  `serde_yaml` and `unsafe-libyaml` from the dependency graph. `cargo deny`
  passes without a new advisory exception. Disable kube's automatic retries
  so the existing poll cadence and per-request deadlines continue to own
  failure recovery.

### Changed

- Keep current Clippy builds compatible with Rust 1.88: use fixed-size slice
  chunks for Apple DVFS decoding and require async-trait 0.1.92, which removes
  a redundant generated `must_use` attribute flagged by newer Clippy.
- Kubernetes bindings target **1.32**; test, docs and publishing feature
  gates are updated together. Kubernetes timestamps now use jiff while
  snapshot ages keep their existing semantics. Rust 1.88 remains the MSRV;
  kube is constrained to 4.0.x because 4.2 requires Rust 1.89. The unused
  kube runtime feature is removed.
- The hidden `build_kube_snapshot_for_bench` helper now accepts the Pod type
  from k8s-openapi 0.28. This changes the Rust type identity for consumers
  of that helper and is accounted for by this minor release. Public snapshot
  models and the remote wire format are unchanged.

## [0.7.0] - 2026-08-08

Feature release: the GPU tab on **Apple Silicon**. Utilisation and memory come
from the IORegistry through public IOKit calls, power and clock from `IOReport`
loaded at runtime — all of it unprivileged, which is precisely the fact the v0.5
and v0.6 deferrals got wrong. macOS becomes the third platform with a working
GPU tab.

The roadmap's other v0.7 item, interactive `docker exec` (PTY), moves to v0.8 —
the same treatment v0.5 gave it. This release is Apple Silicon only.

**Minor bump, not patch.** `gpu_engine::APPLE_DEFERRED_DETAIL` is removed and
`muxtop_core::apple` is new public surface, which breaks the source API of
`muxtop-core` — permitted under Cargo's 0.x rules only on a minor bump.

**The wire format is untouched, and a mixed v0.6/v0.7 pair therefore does
interoperate** — the first minor release since v0.3 where that holds. Not one
type carrying `Encode`/`Decode` changed: `GpuVendor::Apple` and
`GpuBackend::AppleIoReport` were reserved in the model in v0.5 for exactly this
release, so the backend had somewhere to land without appending a field. The
version-matching rules documented in v0.4.0 and v0.5.0 gain no new entry.

### Added

#### Apple Silicon GPU support (`muxtop-core`, `muxtop-tui`)

The GPU tab works on an Apple Silicon Mac. It was scheduled for v0.5, deferred
to v0.6, and shipped in neither — the deferral rested on a premise that does not
hold.

**The v0.5 reasoning was wrong, and it is worth saying how.** The release notes
argued that macOS exposes GPU counters only through the private `IOReport`
framework, and that `IOReport` needs root because `powermetrics` does. Both
halves are false. The GPU driver publishes utilisation and memory in the
IORegistry through documented IOKit calls any user can make — it is where
Activity Monitor's own GPU graph comes from — and the `IOReport` energy and
performance-state channels are readable unprivileged. `powermetrics` needs root
because it reads channels muxtop never subscribes to. Two releases of an empty
tab came out of not checking.

**No wire-protocol break** — see the release header. The reserved `Apple`
variants are why: a backend arriving without a home in the model would have had
to append a field, and bincode is order-sensitive.

- **Layered, not all-or-nothing.** `IOAccelerator` (public IOKit) gives the
  device name, GPU core count, driver build, utilisation and memory;
  `IOReport` (private, `dlopen`ed at runtime like NVML) adds power and clock. A
  macOS release that renames the `IOReport` symbols costs the POWER and CLOCK
  columns and nothing else — the tab keeps working on the public half alone.
- **Power** is derived from the `GPU Energy` counter over the real interval
  between two samples, not an assumed second. The unit travels with the value
  because it is per-channel metadata rather than a constant: the same M3 reports
  `GPU Energy` in nanojoules and `GPU` in millijoules, and hard-coding either
  would be off by a factor of a million on the other. The two agree to within
  rounding, which is how the conversion was checked.
- **Clock** is the residency-weighted average over the states the GPU actually
  ran in, from the hardware DVFS residency channel against the `voltage-states9`
  table. The parked state is excluded from the average: a GPU that spent 90 % of
  the second clock-gated and 10 % at 1 338 MHz was running at 1 338 MHz when it
  ran, and folding the zero in would report a clock the hardware never used.
- **The DVFS table is treated as untrusted input.** It is an undocumented
  device-tree blob, and that index 9 is the GPU's is an observation about M1
  through M4, not a contract. A blob that is not a whole number of `(Hz, mV)`
  pairs, or that decodes to an impossible clock, is rejected outright — as is a
  performance state the table cannot name *and* that accumulated time. Losing
  the CLK column on a future chip is recoverable; printing an invented clock
  next to real numbers is not.
- **Unified memory is not VRAM, and the tab no longer says it is.** The GPU
  addresses the same physical pool as the CPU, so the memory column header reads
  **MEM** on an Apple Silicon host and the Inspector says "Unified memory". The
  figures are the driver's GPU-resident bytes against the machine's whole
  memory: 2 GB of 16 GB reads 12 %, with the CPU competing for the rest.
  Discrete cards keep the VRAM wording, which on them is accurate.
- **Apple Silicon only.** `IOAccelerator` is the generic accelerator class, so
  an Intel Mac's AMD or Intel GPU answers the same match with a differently
  shaped statistics dictionary. Detection requires Apple's own `AGX` driver
  family and the empty tab explains itself, rather than showing a row labelled
  Apple that reports nothing.
- The `apple/metrics.rs` derivations — DVFS decode, residency weighting, energy
  units — are deliberately **not** `cfg`-gated, so their tests run on the Linux
  and Windows CI legs too. Only the FFI is gated. This is the same rule
  `amd_engine` follows, and for the same reason: the v0.4.2 Windows break
  survived two releases because one leg never compiled that code.

### Honest limitations

Held to the same contract as every other backend — **`—` means "cannot report",
never "zero"**.

- **No per-process usage.** There is no Apple equivalent of NVML's process
  queries, public or private. The Procs sub-view says so explicitly, as it does
  for AMD, instead of rendering an empty list that reads as an idle GPU.
- **No GPU temperature.** The `GPU Stats / Temperature` channels exist in the
  `IOReport` legend and read zero for an unprivileged process on every machine
  tested. A confident 0 °C on a warm laptop would be worse than a dash.
- **No power limit, no fan, no encoder/decoder.** The SoC power budget is shared
  with the CPU and managed by firmware, cooling is chassis-wide rather than
  per-GPU (a MacBook Air has none at all), and the media engine publishes no
  utilisation counter.
- **Power and clock are `—` for the first tick.** Both are counters, not gauges:
  they are only defined over an interval. The baseline is taken on the first
  collector tick rather than at connect time — the gap between connecting and
  the first tick is unbounded and can be a millisecond, and dividing an energy
  counter by a millisecond produces a number with no relationship to the GPU's
  power draw. One tick of `—` buys a real one-second window.

### Measured cost

The Thomas macro-benchmark was re-run for this release, and it turned up two
things worth stating rather than burying.

- **The Apple backend costs ~2.4 MiB of resident memory.** Peak RSS over a 30 s
  headless run goes from 10.1 MiB to 12.4 MiB on a MacBook Air M3. The figure is
  attributed, not assumed: v0.7.0 launched with `--no-gpu` lands at 9.9 MiB,
  which matches v0.6.0 on the same machine to within run-to-run noise, so none
  of the growth belongs to the dependency bumps that also landed here — sysinfo
  0.34 → 0.38 included, despite sitting on the process-collection hot path. That
  leaves 0.6 MiB of headroom against the project's 13 MiB budget, which is
  thinner than it should be. The most likely first cut is the `IOReport`
  subscription: it copies the whole `Energy Model` group, roughly eighty CPU
  channels, to read one GPU counter.
- **Binary size did not meaningfully move** — 7.99 MiB at v0.6.0, 8.03 MiB here,
  so this release added 49 KiB. The three new macOS dependencies are thin FFI
  layers and behave like it.

The README's benchmark table was showing figures measured at **v0.3.1** and
never re-run across four releases — 5.3 MiB binary, 11.3 MiB RSS — which
described a binary nobody had downloaded since v0.4 brought in a Kubernetes
client. It now carries measured v0.7.0 numbers, the machine they came from, and
the attribution above. **v0.8 is an optimisation release**: restating a budget
you have drifted past is not the same as meeting it.

### Changed

- **Sub-watt power no longer rounds away to `0W`** (`muxtop-tui`). Whole watts
  are right for a discrete card that idles in the tens and peaks in the
  hundreds; they are wrong for an Apple GPU idling under a tenth of a watt,
  where `0W` claims a powered-off block. Figures below 10 W now carry one
  decimal. Discrete cards are unaffected.
- **An empty bus id renders as `—`** in the Inspector (`muxtop-tui`). Apple
  Silicon's GPU is on the SoC die and has no PCI bus to report; the blank cell
  read as a rendering bug rather than as an absent value.
- The macOS "no GPU" message names what is *supported* rather than what is
  *scheduled*. Its predecessor promised the backend "in v0.6" and was still
  saying so after v0.6 shipped without it; a test now fails if the string names
  a release at all.

### API

Source-breaking for consumers of `muxtop-core` outside this workspace, which
under Cargo's 0.x rules means this release takes a **minor** bump rather than a
patch. The wire format is untouched.

- `muxtop_core::gpu_engine::APPLE_DEFERRED_DETAIL` is removed. Its replacement
  is `MACOS_UNSUPPORTED_GPU_DETAIL`, which describes an unsupported *host* (an
  Intel Mac) rather than an unshipped release.
- New `muxtop_core::apple` module. `AppleEngine` is re-exported at the crate
  root on macOS targets; `apple::metrics` is public on every target so the
  derivations can be tested and reused.
- New macOS-only dependencies: `core-foundation`, `core-foundation-sys` and
  `libloading`. All three are target-gated, so no other platform's dependency
  graph or binary size changes.

### Fixed

- **Processes no longer disappear from the tree view** (`muxtop-core`). `build_process_tree` reached every node from a root, and a process only qualified as a root if its `parent_pid` was absent, `0`, or missing from the snapshot. A group of processes listed as each other's ancestors satisfies none of those, so the whole group — and every subtree hanging off it — was unreachable and silently dropped. `parent_pid` is only what the OS reported at sample time, and PIDs get recycled: a dead parent's PID reappearing on one of its own descendants is enough to close such a loop, which is why this surfaced on Windows first (a CI run flattened 13 of 143 processes). The walk now marks what it has placed and re-roots whatever it could not reach, so `flatten_tree(&build_process_tree(p))` holds every process exactly once. The same pass fixes the `MAX_DEPTH` cut-off, which had been dropping the tail of any chain deeper than 256 rather than re-rooting it.

## [0.6.0] - 2026-08-07

Security and performance release over the Kubernetes surface, plus one
structural defect in the v0.3.1 rate limiter.

**Minor bump, not patch.** `SystemSnapshot::{containers, kube}` change type, which
breaks the source API of `muxtop-core` — permitted under Cargo's 0.x rules only on
a minor bump. The wire format is unchanged and covered by a round-trip test, so a
v0.6.0 client and server interoperate byte-for-byte with each other; a mixed
v0.5/v0.6 pair does not, for the reasons already documented in v0.4.0 and v0.5.0.

### Security

- **Rate-limiter memory is now bounded** (`muxtop-server`). The per-IP token-bucket map created an entry for every source address ever seen — including the ones it *rejected* — and never removed any, so an attacker with a routed IPv6 prefix could grow it until the process was OOM killed. The component meant to stop a flood was itself the target, and it is on by default. Idle buckets are now evicted on an amortised sweep: a bucket refilled back to `burst` admits exactly what an absent entry would, so dropping it is behaviour-preserving, and buckets reach that state after `burst / refill_per_sec` seconds (1 s at the defaults). Sweeps run at most once per second so the O(n) scan cannot be triggered per connection; `MAX_TRACKED_IPS` (65 536) is a last-resort ceiling past which unknown sources are rejected while already-tracked ones keep their budget.
- **Flood refusals no longer amplify into the log.** Both refusal paths wrote one line per rejected connection, and refusals arrive at flood rate by definition — a bounded accept path feeding an unbounded log. The ceiling warning now fires on the transition rather than per attempt, and the per-peer accept message drops from `warn` to `debug`.
- **The remote hostname is scrubbed** (`muxtop-tui`). The v0.4.1 follow-up closed the sanitizer bypass on the confirm prompt and the status path, but the connection indicator still interpolated the hostname straight from the server's `Welcome` frame — and unlike a toast, the chrome is painted every frame for the whole session.
- **Per-process collection narrowed to what the TUI renders** (`muxtop-core`). `ProcessRefreshKind::everything()` also collected `environ`, `cwd`, `root` and `exe`. `environ` is read once and then held in sysinfo's process table for the whole run, so running muxtop as root parked a copy of every process's environment — API keys, tokens, database URLs — in our address space, for data we never display.
- **metrics-server responses are capped at 16 MiB** (`muxtop-core`). `metrics.k8s.io` is served by an *aggregated* APIService, so unlike the typed resource lists its body is chosen by a component outside the trust boundary the rest of the kube path assumes. Oversized bodies are rejected rather than truncated, since a half-read body parses as invalid JSON and would be misreported as "metrics-server unavailable".
- **No panic on a pre-epoch system clock** (`muxtop-core`). `SystemSnapshot::collect` ran `.expect()` on `duration_since(UNIX_EPOCH)` every tick. An embedded host with no RTC reads 1970 until NTP lands — and is exactly the kind of box someone points a system monitor at.
- Dependabot alerts and automated security fixes enabled; private vulnerability reporting enabled; `develop` protected with required status checks.

### Fixed

- **Nodes sub-view PODS column** (`muxtop-core`). `pod_count` was hardcoded to `0` behind a "populated in S2.6" comment that never landed, so the column reported 0 pods on every node of every cluster since v0.4.0. The count now joins the pod list in `ClusterEngine::snapshot` and follows `kubectl describe node`'s "Non-terminated Pods" rule — Succeeded and Failed pods have released their node resources.
- **Selected Kubernetes object under an active sort** (`muxtop-tui`). `selected_kube_name` re-derived its own *unsorted* filtered list and took `nth(kube_selected)`, but the selection indexes the sorted order — so with any sort active it named a different object than the one highlighted on screen. It now reads the same projection the table renders from.

### Performance

- **Container and cluster snapshots are shared, not deep-cloned** (`muxtop-core`). Both are produced by loops slower than the system tick (0.5 Hz and 0.2 Hz against at most 1 Hz), so four of every five kube clones rebuilt three `String`s per pod for nothing. Both fields are now `Arc`. Measured on a 1 000-pod snapshot: **149 µs → 10 ns** per tick, removing ~3 000 redundant allocations per second and scaling with cluster size. `gpu` is deliberately left owned — `GPU_INTERVAL` is 1 s and `--refresh` has a 1 s floor, so its clone is never redundant. **The wire format is unchanged** — bincode encodes `Arc<T>` through `T::encode`, and a new proto test proves the byte sequences are identical rather than asserting it in a comment.
- **Kube tab view is cached** (`muxtop-tui`). The tab never received v0.3.1's PERF-M2/PERF-M5 fixes: it re-filtered and re-sorted every frame, lowercasing each row's name and namespace, and `kube_count()` ran a second full filter on every navigation keypress. `compute_view_indices` now runs once per state change into `AppState::kube_view_cache`, matching via `contains_ignore_case` (ASCII fast path, no allocation). The cache stores indices, so it costs one `usize` per row regardless of row size.
- **Narrowed process refresh** (`muxtop-core`). Beyond the `environ` exposure above, `disk_usage` was refreshed unconditionally on every tick — a `/proc/<pid>/io` open+parse per process per second on Linux. Measured on macOS with 444 processes: 5.87 ms → 4.75 ms per refresh (**−19 %**); the Linux saving is not measured here.
- **Kube polls hold their cadence** (`muxtop-core`). Resource and metrics fetches awaited one after another under 3 s timeouts each, so a slow API server could spend 9 s and 6 s respectively against a 5 s `POLL_INTERVAL`. Both loops also slept *after* their work, making the real period `tick + 5 s` and drifting out of phase with the collector's fixed sampling. They now fan out with `tokio::join!` and are driven by `tokio::time::interval` with `MissedTickBehavior::Skip`.

### Not done

- Caching the converted `KubeSnapshot` inside `KubeEngine` was considered and rejected. With the cadence drift fixed above, the redundant conversion is ~60 µs/s at 1 000 pods; a second cache keyed on `last_update_ms` would trade that for a way to render stale cluster state.

### API

- `SystemSnapshot::{containers, kube}` are now `Option<Arc<..>>`, and `SystemSnapshot::collect` takes them as such. Wire-compatible; source-breaking for any consumer of `muxtop-core` outside this workspace.
- `muxtop_core::process::contains_ignore_case` is now public.

### Repository and pipeline

No change to the binary. muxtop is a public project with a network-facing
daemon, and the parts of it that a user has to trust — the release pipeline,
the disclosure policy, the operational documentation — had not received the
same attention as the code.

### Added

- **`SECURITY.md`** — disclosure policy with private reporting, response
  targets, an explicit in-scope / out-of-scope list, and a one-page summary of
  the trust boundaries. Private vulnerability reporting is enabled on the
  repository, so a report never has to start as a public issue.
- **A wiki, generated from `docs/wiki/`** — thirteen pages covering what the
  README cannot hold: TLS and token setup for `muxtop-server` with a hardened
  systemd unit, Kubernetes RBAC for both the namespaced and cluster-wide cases,
  container socket choices, GPU backend limits, a symptom → cause
  troubleshooting table, performance tuning, the architecture, and the release
  runbook. `wiki-sync.yml` mirrors it on release, so the wiki is a published
  artefact rather than a second source that drifts.
- **Scheduled advisory audit (`advisories.yml`)** — `cargo deny check
  advisories` daily. CI only runs when somebody pushes, so an advisory
  published against an already-shipped dependency previously went unnoticed
  until the next unrelated commit. Failures open one rolling issue rather than
  one per day, and `deny.toml`'s documented exceptions are honoured so the
  audit does not re-report accepted risk every morning.
- **CodeQL analysis** — on push and weekly, feeding the Security tab.
- **Build-provenance attestations on release artefacts** — verifiable with
  `gh attestation verify <archive> --repo lucasschimmel/muxtop`. A published
  checksum cannot prove provenance: whoever can replace the archive can replace
  the `.sha256` beside it.
- **Release gate (`verify` job)** — the tag must match the workspace version and
  `CHANGELOG.md` must document it. Everything downstream is irreversible: a
  crates.io publish is permanent and two package managers take the version
  before a mistake is noticeable.
- **New CI jobs** — MSRV check against the declared 1.88, `cargo doc` with
  `-Dwarnings` (broken intra-doc links are the most common rot in a published
  crate), coverage via `cargo-llvm-cov` reported to the run summary with no
  third-party service, and `cargo-semver-checks` on the three published crates.
- **`dependabot.yml`** — weekly Cargo and GitHub Actions updates against
  `develop`, grouped so the majors that need reading are not skimmed alongside
  forty patch bumps.
- **Issue and pull-request templates, `CODEOWNERS`, `CODE_OF_CONDUCT.md`.** The
  bug template asks for terminal, `$TERM`, platform and local-vs-remote up
  front — the answers that decide whether a report is reproducible.

### Changed

- **Every third-party GitHub Action is pinned to a commit SHA**, so a
  re-pointed tag cannot inject code into a release. Dependabot advances them
  under review. Note that pinning `dtolnay/rust-toolchain` by SHA means the
  toolchain can no longer be inferred from the `@stable` ref, so every use now
  names it explicitly.
- **Workflow tokens are read-only by default**, with write scopes granted per
  job: `release` gets `contents: write`, `build` gets attestation signing, and
  the Homebrew and APT jobs get nothing on this repository. A compromised build
  step cannot publish a release.
- **`concurrency` groups on every workflow.** Pull-request runs supersede
  themselves; `develop`, `main` and release runs never cancel, since a cancelled
  required check reads as a failure.
- **`cross` and `cargo-deb` install as prebuilt binaries** instead of being
  compiled from source on every release build.
- **`fail-fast: false`** on the test and release matrices — hiding whether a
  break is platform-specific is the opposite of useful.

## [0.5.1] - 2026-08-05

Ergonomics and UI/UX release. No new data source: everything muxtop already
collected, made navigable, discoverable and readable. The audit that produced
it, with the per-screen specifications, is in [`docs/UX-v0.5.1.md`](docs/UX-v0.5.1.md).

The root cause of most of what follows was structural: the five table views each
hand-rolled their own column header, scroll arithmetic, striping, filter bar and
empty state. Five copies meant five behaviours and five places to fix every bug.
They now share `ui/widgets/`, and the keymap is a single table that drives
dispatch, the help screen and the footer hints at once.

### Changed — breaking

- **`←` / `→` no longer switch tabs.** They scroll columns. Horizontal arrows
  meaning "change screen" while vertical arrows meant "change row" was the single
  most disorienting thing in the 0.4/0.5 UI. Use `Tab` / `Shift+Tab`, or
  `Alt+1`…`Alt+6`.
- **`F1`–`F5` no longer sort.** muxtop advertises htop shortcuts, so it now
  honours the htop map: `F1` Help, `F3`/`F4` Search/Filter, `F5` Tree, `F6` Sort,
  `F7`/`F8` Nice, `F9` Kill, `F10` Force kill. A user coming from htop pressing
  `F1` expecting documentation used to silently re-sort the table instead.
- **Tab-scoped keys act only on their own tab.** `t` and the function keys used
  to fire from anywhere: pressing `F9` on the Network tab killed the selected
  *process*, and `t` re-shaped a process table the user was not looking at.
- **`Esc` is progressive.** One step back per press: close overlay → dismiss
  messages → leave the filter input → clear the filter. It never quits.

### Fixed

- **The mouse wheel works.** It moved `scroll_offset` without the selection, so
  the next frame snapped the view back and the wheel appeared dead. It also
  always moved the *process* offset, whatever tab was on screen.
- **"Clear filter" clears the filter you can see.** The palette command had no
  `Tab::Kube` arm, so on the Kube tab it cleared the process filter instead.
  Every filter operation now routes through one implementation.
- **Message severity is declared, not guessed.** The footer decided whether a
  message was an error by testing it for the substring `"failed"`, so rewording
  an error painted it on the success colour.
- **256-colour terminals get a 256-colour theme.** `ColorSupport::Colors256` was
  detected and then discarded — `Theme::new` only branched on TrueColor, so
  Terminal.app and a default `ssh user@host` session both fell through to 16
  colours.
- **`NO_COLOR` is honoured**, and `TERM=dumb` really gets no colour: the "no
  colour" branch used to emit `Cyan` and `Green` like every other fallback.
- **`+` / `-` renice exists.** It had been in the README since 0.1 with no
  handler behind it.
- **Palette commands take arguments** — `kill firefox`, `stop nginx`,
  `restart postgres` — as the README has advertised since 0.3 against a command
  enum that could not carry one.
- **GPU columns sort unreported metrics last in both directions.** Folding an
  absent value in as a very low one made a card whose driver reports no
  temperature look like the coldest one, which is exactly the confusion the
  dashes exist to prevent.

### Added

- **Help overlay (`?` / `F1`)**, generated from the keymap table, so it cannot
  drift from the bindings. Leads with the active tab's own keys and annotates
  what remote mode disables.
- **Inspector (`Enter` / `i`)** — the second layer the tables truncate: full
  command line, image, memory against its cgroup limit, pod node and QoS, GPU
  clocks and power, interface error counters. A side panel on wide terminals, a
  full overlay on narrow ones.
- **Actions menu (`x`)** listing exactly the actions available on this tab, with
  their shortcuts.
- **Command mode (`:`)** for the argument forms, alongside the fuzzy palette
  (`Ctrl+P` / `Ctrl+K`). The palette now ranks the active tab's commands first,
  highlights matched characters, remembers the session's recent commands, and
  finally exposes the Kube and GPU sub-views and the namespace toggle.
- **Typed notifications** with a severity, a toast stack, and a session log
  (`Ctrl+L`) so an action that failed while you were on another tab is still
  recoverable.
- **`Space` pauses the view** so a fast-moving table can be read; `r` resumes.
- **`y` copies the selected row's identifier** over OSC 52 — works through `ssh`
  and `tmux`.
- **Live per-tab counts in the tab bar**, and a header line carrying host,
  connection, uptime and global CPU/memory meters.
- **A status bar that shows state**: sort column and direction, active filter
  with its match count, cursor position, paused indicator — then as many
  contextual hints as fit, instead of a fixed list that silently overflowed an
  80-column terminal.
- **The General tab is a dashboard**: CPU, load against core count, memory, a
  traffic graph, the top processes, and a cross-tab Workloads card that
  summarises containers, Kubernetes and the busiest GPU. 0.5 absorbed the
  remaining height with an empty `Constraint::Min(0)` — on a 50-row terminal,
  half the tab was blank.
- **Responsive tables.** Columns declare a priority; the least useful are dropped
  first and the identity column always survives. Plus scrollbars, position
  readouts, and empty states that say why a view is empty and what to do about it.
- **`--theme <name>`** (`tokyo-night`, `tokyo-night-light`, `mono`),
  **`--no-color`**, **`--ascii`**, **`--no-mouse`**.
- **Honest terminal detection.** `TERM=linux` (the kernel console, a KVM console,
  serial-over-LAN) gets the ASCII glyph set, because its bitmap font has no
  block, braille or rounded-box glyphs and would otherwise paint tofu. A
  non-UTF-8 locale does the same on Unix. Mouse capture is skipped where there is
  no pointer. Every mouse gesture has a keyboard equivalent.
- **`muxtop_core::system::host_name()`**, cached, for the header.

### Tests

TUI coverage roughly doubles (486 tests). Every tab and every overlay is rendered
at sizes from 1×1 to 400×100 and under all four colour depths crossed with
Unicode and ASCII, with an assertion that ASCII mode emits no multi-byte
character anywhere. Each fixed bug above has a regression test that names it.

## [0.5.0] - 2026-08-05

Feature release: the **GPU** tab. NVIDIA via [NVML](https://developer.nvidia.com/nvidia-management-library-nvml) and AMD via the `amdgpu` sysfs interface, with per-process usage on NVIDIA. Auto-detected at startup, so `muxtop` gains a sixth tab on any host with a supported GPU.

**Apple Silicon is deferred to v0.6, not dropped** — see [Why Apple Silicon is not in this release](README.md#why-apple-silicon-is-not-in-this-release). The roadmap's other v0.5 item, interactive `docker exec` (PTY), also moves to v0.6; this release is GPU only.

### Wire protocol break

- **`SystemSnapshot` gains a `gpu` field**, appended after `kube` and before `timestamp_ms`. bincode is order-sensitive, so a 0.4.x client decoding a 0.5 frame reads the GPU bytes as its `timestamp_ms` and fails. **Client and server must match on the minor version**, exactly as for the v0.4 `kube` break.

### Added

#### GPU (`muxtop-core`, `muxtop-tui`, `muxtop`, `muxtop-server`)

- New `Tab::Gpu` (keybind `Alt+6`, palette entry "Switch to GPU tab") with two sub-views switched by `D` / `P`:
  - **Devices** (default) — 11 columns `#` / NAME / VENDOR / UTIL / MEM / MEM% / TEMP / POWER / CLK / FAN / ENC-DEC, with the utilisation and memory columns on the shared gauge ramp and a GPU-specific temperature ramp (green < 80 °C, amber ≥ 80 °C, red ≥ 90 °C — 80 °C is unremarkable for a GPU under load where the same number on a CPU would be alarming).
  - **Procs** — 5 columns PID / NAME / GPU / TYPE / GPU MEM, one row per (pid, device) pair as `nvidia-smi` reports it.
- **Sort cycling** via `s` (Devices: Index→Util→Mem→Temp→Power→Name; Procs: Mem→Pid→Name→Device) and `S` / `I` for direction, with the active column marked `↓` / `↑`. Devices default to **index** order rather than utilisation: users refer to GPUs by number, and a list that reorders itself under load breaks that mental map.
- **Filter** via `/`, matching device name + vendor on Devices and process name **+ PID** on Procs — a user chasing a runaway job usually has the PID from the Processes tab, not the name.
- **NVIDIA backend (`nvml_engine.rs`)** — utilisation, memory-controller utilisation, VRAM used/total, temperature, power draw and enforced limit, graphics and memory clocks, fan duty cycle, NVENC/NVDEC utilisation, plus compute and graphics processes merged per PID. `libnvidia-ml.so` / `nvml.dll` is resolved **at runtime** via `libloading`, so muxtop builds and runs identically on hosts with no NVIDIA driver — no feature flag and no separate build. NVML calls run in `spawn_blocking` so the synchronous C library never stalls the runtime worker driving the container and cluster loops.
- **AMD backend (`amd_engine.rs`)** — `/sys/class/drm/card*/device` plus its `hwmon` node: utilisation, VRAM, temperature, power (average with an instantaneous fallback) and cap, clocks (live `hwmon` frequencies preferred over the `pp_dpm_*` state tables), and fan duty from `pwm1`. Zero dependencies and no privileges beyond world-readable sysfs. Cards are filtered by PCI vendor id so an Intel iGPU sitting at `card0` is not claimed, connector directories (`card0-DP-1`) are rejected, and discovery is ordered numerically so `card10` cannot sort ahead of `card2` and shuffle device indices under the user's cursor between ticks.
- **`CompositeGpuEngine`** merges several vendor backends into one flat snapshot — an AMD iGPU plus an NVIDIA dGPU is the standard gaming-laptop layout and the Containers/Kube one-daemon model does not fit. Device indices are reassigned across the merged list and `GpuProcessSnapshot::device_index` is remapped through the same shift, so a process keeps pointing at its own card. A backend that fails is skipped rather than fatal: a broken NVIDIA driver must not hide a working AMD card.
- **`--no-gpu`** on both `muxtop` and `muxtop-server` disables detection entirely. Useful on laptops where probing a discrete GPU can pull it out of runtime-D3 and cost battery.
- GPU process names are resolved in `SystemSnapshot::collect` from the process table the collector already refreshed for the Processes tab, rather than inside each backend — one hash lookup per GPU process instead of a second full process enumeration per tick.
- `gpu_bench.rs` criterion benchmark covering the composite merge (up to 8 devices × 8 processes) and name resolution — the only muxtop-authored code on the GPU hot path.

### Changed

- **The collector runs a fourth loop** (`spawn_gpu_loop`) at **1 Hz** — faster than the container (0.5 Hz) and cluster (0.2 Hz) loops. NVML computes `utilization_rates` over an internal window of about one second, so polling slower aliases the signal and renders a GPU pinned at 100 % as an erratic sawtooth. An NVML device query is sub-millisecond, so matching the driver's cadence is the cheap correct choice rather than a trade-off.
- `Collector::with_all_engines` is the new full constructor; `with_engines` is retained and delegates to it, so existing callers keep compiling.
- **The tab bar no longer shows `GPU [soon]`** — the placeholder became a real tab. `FUTURE_TABS` is kept but empty; a regression test now asserts no implemented tab is still marked `[soon]`.
- The palette's "Clear filter" command now clears the Kube and GPU filters too. It previously fell through to the process filter on those tabs, silently clearing the wrong one.
- Tab-cycling tests derive from `Tab::ALL` instead of hard-coding pairs, so adding a tab no longer means editing half a dozen tests that only care that the order is a closed cycle.

### Honest limitations

Every GPU metric is `Option`, and the UI renders an unavailable one as `—`. **`—` means "this driver cannot report this", never "zero"** — an idle GPU shows `0 %` and conflating the two would make the tab lie.

- **AMD reports no per-process usage.** The `amdgpu` sysfs interface has no equivalent of NVML's process queries, so the Procs sub-view states that explicitly instead of showing an empty list that reads as "nothing is using the GPU". The summary bar carries a `per-process: unsupported` badge, mirroring the v0.4 `metrics-server: off` badge.
- **Encoder/decoder utilisation is NVML-only**; AMD renders `—`.
- **On Windows, per-process GPU memory is unavailable and every process reports `both`.** Under WDDM the OS owns video-memory allocation so NVML returns the per-process figure as unavailable, and it returns identical compute and graphics process lists — verified against an RTX 3080 on Windows 11 (27 PIDs each, full overlap). Both are the driver's answers and are surfaced as-is. On Linux the figures and the compute/graphics distinction are real.

### Security

- Device names, process names and the engine's `detail` string all pass through `scrub_ctrl` before rendering. Device names come from the driver, process names from whatever a user chose to call their binary, and `detail` crosses the wire from the server in `--remote` mode — all three are foreign strings, and the v0.4.1 lesson was that a render site missed by the sanitizer sweep is a terminal-escape injection point.
- The GPU tab is **read-only by construction**: it has no actions, and no code path sets a clock, a power limit or a fan curve. The `nvml-wrapper` dependency is target-gated to Linux and Windows, so it is absent from macOS builds entirely.

## [0.4.2] - 2026-08-05

Follow-up to 0.4.1: `--kube-namespace` becomes a real scoping filter instead of a display label, and the workspace compiles on Windows again. No wire-format change — 0.4.1 and 0.4.2 remain compatible on the wire.

### Fixed

- **The workspace compiles on Windows again (`muxtop-core`)** — `actions.rs` called `libc::kill`, `libc::setpriority`, `libc::getpriority`, `libc::SIGKILL`, `libc::PRIO_PROCESS` and `libc::id_t` with no `cfg(unix)` gate. None of those exist in `libc` on Windows, so the whole of `muxtop-core` failed to build there and took every dependent crate with it — no `cargo check`, no `cargo test`, no way to work on any part of muxtop from a Windows machine. The POSIX implementations are now gated behind `cfg(unix)`, with `cfg(not(unix))` stubs of identical signature that fail with `ErrorKind::Unsupported`. PID validation was extracted into a shared `validate_pid` that runs *before* the platform split, so the safety guarantees (no `kill(-1, …)`, no `kill(0, …)`) hold and stay tested on every platform.

  This is a build fix, not Windows support: `release.yml` still publishes musl and darwin binaries only, and F7/F8/F9/F10 return an error there rather than acting.

### Changed

- **CI runs the test suite on `windows-latest`** as a regression guard. The break above survived two releases precisely because nothing in the pipeline ever compiled the workspace on Windows.

### Added

- **Real namespace scoping for the Kubernetes tab (`muxtop-core`, `muxtop-tui`, `muxtop`, `muxtop-server`)** — `--kube-namespace <NS>` now scopes the actual API calls: Pods and Deployments are listed through `Api::namespaced` instead of `Api::all`, and pod metrics move to `/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods`. A `Role` bound to one namespace is now sufficient to use the tab — previously muxtop required cluster-scoped `list` on all three resources, which locked out anyone without cluster-wide RBAC. Without the flag the behaviour is unchanged (cluster-wide), so existing setups are unaffected.
- **`A` toggles the namespace scope at runtime** (Kube tab, local mode) — flips between the configured namespace and all-namespaces. The engine owns the flip because only it knows which namespace to scope to (`--kube-namespace`, else the kubeconfig context's default). Rescoping clears the pod and deployment caches immediately so no out-of-scope row survives until the next 5 s poll. In `--remote` mode the server's `--kube-namespace` decides and the key reports that it is local-only, mirroring how container actions are gated. Closes the "namespace toggle `A`" item deferred from v0.4.0.
- **`ClusterEngine::scope` / `ClusterEngine::toggle_scope`** with default implementations reporting `KubeScope::AllNamespaces`, so out-of-tree implementations keep compiling.
- **README gains a "Kubernetes permissions" section** with a minimal namespace-scoped `Role` manifest and an explanation of why Nodes are the exception.

### Security

- **Namespace input is validated as a DNS-1123 label** before it reaches the metrics-server URI builder (`is_valid_namespace`, shared by both binaries through `parse_namespace` so they reject identical input with identical wording). The namespace comes from `--kube-namespace` and is interpolated into a request path; the accepted character set excludes `/`, `.`, `?` and `%`, so neither a path segment nor a query string can be injected. Rejected at the CLI boundary by clap and again inside `KubeEngine::connect`, which must not depend on its callers validating.
- **`current_namespace` is now scrubbed before rendering (`muxtop-tui`)** — the Kube summary bar interpolated it raw. In `--remote` mode this string arrives from the server, making it the last unsanitised render site in the Kube tab; the v0.3.1 sanitizer sweep and the v0.4.1 follow-up both missed it.

### Changed

- **`KubeSnapshot::current_namespace` now means the effective scope**, with the empty string encoding "all namespaces". It previously carried the kubeconfig's default namespace while listing cluster-wide regardless — the field described an intent the engine did not honour. **No wire-format change**: the distinction is encoded in the existing field rather than a new one, so 0.4.1 and this version remain compatible on the wire.
- The Kube summary bar renders `ns: all` when cluster-wide instead of a bare `ns:`, and the sub-tab bar shows an `[A]` hint naming the scope it would switch to.
- The Nodes sub-view distinguishes "no nodes" from "no access to nodes" when scoped to a namespace, rather than implying a reachable cluster has no nodes.
- `muxtop_tui::run` takes a fourth argument, the optional cluster engine. Internal to the workspace — the only caller is `src/main.rs`.

## [0.4.1] - 2026-08-05

First published release carrying the Kubernetes tab. **0.4.0 was tagged in the changelog and merged to `develop` on 2026-04-26 but never released** — no git tag, no GitHub release, no crates.io publication. Users upgrading from 0.3.1 receive the entire 0.4.0 feature set (see below) plus the security and correctness fixes in this section. There is no 0.4.0 artifact on any distribution channel; the 0.4.0 changelog entry is retained as the record of when that work landed.

### Security

- **Sanitizer bypass on two render paths (`muxtop-tui`)** — the `scrub_ctrl` guard added in v0.3.1 (MED-S5) was applied by the table renderers but not by the confirmation dialog or the footer status bar, both of which interpolate attacker-controlled process `comm` / container names. A process named `bash\x1b]0;…\x07` fired its escape sequence as soon as the user pressed `F9`. `ConfirmAction::prompt()` now scrubs each name, and `AppState::set_status()` scrubs centrally so every current and future caller is covered.
- **`event-listener` bumped to 5.4.2** — closes RUSTSEC-2026-0221 (unsound `Send`/`Sync` on `StackSlot`, pulled in transitively via `kube-runtime` → `async-broadcast`). `cargo deny check` is green again.

### Fixed

- **`Alt+5` now switches to the Kubernetes tab (`muxtop-tui`)** — the shortcut was documented in the v0.4.0 release notes and the README but never bound; only `Alt+1`–`Alt+4` existed, leaving the Kube tab reachable solely through `Tab` / arrow cycling. A regression test now asserts one `Alt+N` binding per entry in `Tab::ALL`.
- **Command palette gains `Switch to Kubernetes tab`** — every other tab had a palette entry; the Kube tab did not.
- **README keyboard reference corrected** — the shortcut table advertised bindings that do not exist (`F1` help, `F3` search, `F4` filter, `F5` tree, `F6` sort menu, `F10` quit, `+` / `-` renice). The real bindings are `F1`–`F5` for process sort columns, `F7` / `F8` for renice, `F10` for force kill, `/` for filter and `t` for tree view. Navigation keys that were implemented but undocumented (`g` / `G`, `Home` / `End`, `PageUp` / `PageDown`, `Esc`, `Tab` / `Shift+Tab`, arrow keys) are now listed.

### Changed

- **`--kube-namespace` documentation corrected (`muxtop`, `muxtop-server`)** — the flag sets the namespace displayed in the Kube header, but Pods / Nodes / Deployments are still listed cluster-wide via `Api::all`. The README and both `--help` texts said "override the default namespace", which read as a scoping filter. Actual namespace scoping remains to be implemented; the cluster-scoped RBAC requirement is now stated in the README feature table.

## [0.4.0] - 2026-04-26

Major feature release: the **Kubernetes** tab (replaces `k9s`-light) with read-only Pod / Node / Deployment monitoring via [kube-rs](https://github.com/kube-rs/kube). Auto-detection at startup means `muxtop` gains a fifth tab on any host with a reachable kubeconfig.

### Added

#### Kubernetes (`muxtop-core`, `muxtop-tui`, `muxtop`)
- New `Tab::Kube` (keybind `Alt+5`) with three sub-views switched by `P` / `N` / `D`:
  - **Pods** (default) — 9 columns NAMESPACE / NAME / READY / STATUS / RESTARTS / AGE / CPU / MEM / NODE, color-coded by phase (Running=success, Pending=warning, Succeeded=accent, Failed/CrashLoop=danger, Terminating/Unknown=dim).
  - **Nodes** — 8 columns NAME / STATUS / ROLES / AGE / VERSION / CPU% / MEM% / PODS, color-coded by status (Ready=success, NotReady=danger, SchedDisabled=warning).
  - **Deployments** — 6 columns NAMESPACE / NAME / READY / UP-TO-DATE / AVAILABLE / AGE, READY column color-coded (green when ready==desired, red when available==0 && desired>0, yellow otherwise).
- Visual sub-tab bar above the table shows `[P]ods [N]odes [D]eployments` with the active sub-view bold + underlined.
- 1-line cluster summary header (cluster_kind / namespace / counts / metrics-server badge).
- **Sort cycling** via `s` (per sub-view: Pods cycle Cpu→Mem→Name→Restarts→Age→Phase ; Nodes cycle CpuPct→MemPct→Name→PodCount→Age ; Deployments cycle Name→ReadyRatio→Namespace→Age). `S` / `I` toggles direction. Active column header shows `↓` / `↑` indicator. Switching sub-view resets sort, filter, and selection.
- **Filter** via `/` opens an inline capture bar with cursor block; `Esc` clears the filter (when not in input mode); `Enter` commits and exits input mode. Filter applies on the active sub-view (name + namespace for Pods/Deployments, name for Nodes).
- **Selection + scroll** via `j`/`k` or arrow keys, with bounds tracked through `kube_count()` (filter-aware). The selected row is bolded and highlighted via `theme.selection_bg`.
- Four render fallbacks: `kube = None` → "Waiting for cluster data…", `reachable = false` → "No cluster data" with a kubectl hint, empty pod list → "No pods in this cluster.", filter shrinks list to zero → "No pods/nodes/deployments match the filter.".
- `metrics-server` graceful degradation: when `/apis/metrics.k8s.io/v1beta1` is unavailable, the CPU/MEM columns (Pods + Nodes) render `—` and the summary shows "metrics-server: off" in yellow. The badge logic is bool-driven by `KubeSnapshot::metrics_available`.
- ANSI / control-char sanitizer (`scrub_ctrl` from v0.3.1) applied to every attacker-controlled string in all three sub-views: pod namespace/name/node, node name/kubelet_version, deployment namespace/name, and the user's filter input echoed in the filter bar — closes the terminal-escape spoofing surface for these new render sites.

#### Cluster engine (`muxtop-core`)
- `ClusterEngine` async trait (see ADR-04 in `forge/32-v04-kubernetes-epics`) with methods `snapshot`, `metrics_available`, `kind`, `server_version`. `#[async_trait]` for dyn-safety, mirroring v0.3 `ContainerEngine`.
- `KubeconfigSource` enum (`Env`/`Home`/`InCluster`/`None`) plus `detect_kubeconfig_with(env, home_kubeconfig, in_cluster_token)` and the production wrapper `detect_kubeconfig()`. Reuses the existing `EnvLookup` trait from `container_engine.rs` (ADR-03 v0.3) — no duplicated env-injection layer.
- `ClusterError` enum: `KubeconfigNotFound`, `Unreachable(String)`, `Forbidden { resource: &'static str, namespace: Option<String> }`, `MetricsUnavailable`, `Stale { since_secs }`, `Other(String)`. Bridges to `CoreError` via `#[from]`.
- `KubeEngine` concrete impl on top of `kube 0.99` + `k8s-openapi 0.24` (features `client`, `rustls-tls`, `runtime`; `default-features = false`). Two background tokio tasks on a 5 s tick race the engine's `CancellationToken`:
  - **Resource poll** — `Api::<Pod>::all().list(limit=5_000)` + `Api::<Node>::all` + `Api::<Deployment>::all` via `kube::api::ListParams`. Per-resource timeout 3 s; per-resource RBAC degradation (`Forbidden` on one resource preserves the rest of the cache).
  - **Metrics poll** — `Client::request_text("/apis/metrics.k8s.io/v1beta1/{pods,nodes}")`. Both 404 → `available = false` and caches cleared. Otherwise sums per-pod CPU + MEM across containers and parses Quantity strings (nanocores / millicores / cores).
- See ADR-05 in `forge/32-v04-kubernetes-epics` for the full poll-vs-reflectors trade-off (poll-based MVP; reflector switch is internal-only if perf measurements warrant).
- ClusterKind heuristic from `serverVersion.gitVersion` substring: `kind` / `k3s` / `k3d` / `eks` / `gke` / `aks` / `openshift`. Fallback `Generic`.
- Conversion logic (`pod_to_snapshot`, `node_to_snapshot`, `deployment_to_snapshot`):
  - Pod synthetic phases — `CrashLoop` (any container in `CrashLoopBackOff`), `Terminating` (`metadata.deletionTimestamp` set).
  - Node status — `status.conditions[type=Ready]`, plus `spec.unschedulable == true` → `SchedulingDisabled`. Roles from `node-role.kubernetes.io/*` labels.
  - Deployment strategy — `RollingUpdate` (default) vs `Recreate`.
  - Quantity parsing — `parse_quantity_to_millis` ("4" / "2000m" / "100m" / "0.5" / "1.5") + `parse_quantity_to_bytes` (Ki/Mi/Gi/Ti binary + K/M/G/T decimal).
  - Metrics injection — when `MetricsCache.{pods,nodes}` carries a `(cpu_millis, mem_bytes)` for the row, populate `cpu_millis` / `mem_bytes`; otherwise leave `None` (UI renders `—`).

#### CLI (`muxtop`, `muxtop-server`)
- `--kube-context <NAME>` flag on both binaries to override the kubeconfig context (default = current-context).
- `--kube-namespace <NS>` flag to override the default namespace from the kubeconfig context.
- `--no-kube` flag to disable the cluster engine entirely (mutually exclusive with `--kube-context`).
- Local mode: `muxtop` runs `detect_kubeconfig()` + `KubeEngine::connect`; failure is non-fatal — the engine becomes `None` and the Kube tab renders the unreachable state.
- Remote mode: the **server** is the only side that opens the kubeconfig. The kubeconfig content (paths, bearer tokens, client certs, exec-auth blocks) **never crosses the wire** — only the digested `KubeSnapshot` does. Anti-leak guards in `muxtop-core/src/kube.rs` and `muxtop-proto/tests/integration.rs` regex-scan every encoded frame for `BEGIN PRIVATE KEY` / `Bearer ` / `client-key-data:` / etc., failing the test if any match.

#### Wire protocol (`muxtop-proto`)
- `PodSnapshot`, `NodeSnapshot`, `DeploymentSnapshot`, `KubeSnapshot`, `PodPhase` (7 variants incl. synthetic `CrashLoop`/`Terminating`), `NodeStatus`, `QosClass`, `DeploymentStrategy`, `ClusterKind` (8 variants) all derive `Serialize`, `Deserialize`, `Encode`, `Decode`, `PartialEq`, `Clone`, `Debug` so they cross the wire via `WireMessage::Snapshot(SystemSnapshot)` unchanged.
- Integration tests: round-trip on populated `KubeSnapshot` (50 pods + 5 nodes + 10 deployments), `unavailable()` sentinel round-trip, frame-size sanity check (1000 pods + 50 nodes + 100 deployments encoded < 1 MiB, well under `MAX_FRAME_SIZE` 4 MiB), anti-leak guard.

### Wire protocol break

- `SystemSnapshot` gains `kube: Option<KubeSnapshot>` between `containers` and `timestamp_ms`. **bincode is order-sensitive — pre-v0.4 clients cannot decode v0.4 snapshots and vice versa.** The new field is `Option`, so the schema mirrors how `containers` was added in v0.3.0.

### Binary size

| | v0.3.1 baseline | v0.4.0 | Delta |
|---|---|---|---|
| `muxtop` (release stripped) | 5,542,560 B (5.29 MiB) | **5,988,560 B (5.71 MiB)** | **+0.43 MiB** |
| `muxtop-server` | 5,226,144 B (4.98 MiB) | **5,672,176 B (5.41 MiB)** | **+0.43 MiB** |
| `cargo build --release` from-scratch | 2:01.99 (224s user) | **2:22.79 (324s user)** | **+21 s wall (+17 %)** |

Net binary delta is much smaller than the original v0.4 plan budgeted (≤ +5 MiB) thanks to `lto=fat` + `strip=symbols` aggressively dead-code-eliminating the `k8s-openapi` types we don't reference from the typed-API path. ADR-04 preserved the engagement to revisit if the delta crossed +5 MiB; that threshold is unmet, no remediation required.

### Dependencies (workspace)
- `kube = "0.99"` (default-features = false; features `client` + `rustls-tls` + `runtime` only) — Kubernetes client + watchers.
- `k8s-openapi = "0.24"` (workspace declares no version feature; binary leaves `muxtop` and `muxtop-server` plus muxtop-core dev-deps each enable `v1_31`, per the k8s-openapi library guideline).
- `http = "1"` — needed by `kube::Client::request_text` for the metrics-server raw HTTP path.
- `serde_json = "1"` — metrics-server response parsing without a typed metrics crate.
- `dirs = "6"` — moved from binary-only to a `muxtop-core` library dep so `detect_kubeconfig` can resolve `~/.kube/config`.

CI implication: `cargo check --workspace` no longer suffices because `k8s-openapi` forbids enabling `v1_*` features in non-binary crates' `[dependencies]`. Use `cargo check --workspace --all-targets` (which activates dev-deps) or build leaf binaries directly. **Publishing muxtop-core to crates.io** runs a verification build of the lib alone — without dev-deps active — so the publish workflow now sets `K8S_OPENAPI_ENABLED_VERSION=1.31` (the documented k8s-openapi escape hatch) at job-level env in `.github/workflows/publish-manual.yml`.

### Performance

A new criterion benchmark in `crates/muxtop-core/benches/kube_bench.rs` measures the hot-path Pod-to-snapshot conversion. Three groups, all on the same synthesized 1000-pod fixture (varied phases / restart counts / namespaces):

| Bench | Median |
|---|---|
| `kube_snapshot/100_pods/with_metrics`  | ~67 µs |
| `kube_snapshot/1000_pods/with_metrics` | ~728 µs |
| `kube_snapshot/1000_pods/no_metrics`   | ~720 µs |

The original v0.4 plan budgeted < 50 ms at 1000 pods (T-816). Measured cost is **~68× under budget**, leaving substantial headroom for cache-and-reuse strategies later if event-driven render starts to bottleneck on the conversion. Metrics injection is a `HashMap::get` per pod — its cost is below noise vs no-metrics.

Incremental compile time (release profile, after the kube-rs deps land):

| Scenario | Wall |
|---|---|
| from-scratch (clean target) | ~2:04–2:22 (CPU-thermal variance) |
| no-op rebuild               | **0.69 s** |
| touch `kube_engine.rs` (cascade through LTO) | ~2:04 |

The no-op number drives the daily-development feedback loop and is unchanged from v0.3.1. The touch-and-rebuild is essentially a full rebuild because `lto=fat` + `codegen-units=1` invalidates downstream LLVM artefacts on any input change in muxtop-core — accepted trade-off for the binary size win documented above.

### Changed
- `Collector::with_engines(interval, container, cluster)` — superset constructor; `Collector::new` and `Collector::with_container_engine` preserved as wrappers for backward compatibility within the library.
- `SystemSnapshot::collect` signature gained a fourth argument `kube: Option<KubeSnapshot>`. All internal call sites (collector + 4 sysinfo tests + 2 benches + alloc_profile example + the wire-module stub) updated; consumers outside the workspace do not need to change because the only production caller is the Collector.
- `Tab::ALL` now has 5 variants; `Tab::next()` / `Tab::prev()` cycle General → Processes → Network → Containers → **Kube** → General. Arrow / Tab / BackTab navigation updated accordingly.
- `WireMessage` and `Event` enums get `#[allow(clippy::large_enum_variant)]` with rationale comments — boxing the `Snapshot` variant would impose a heap allocation on every collector tick, which v0.3.1's perf sweep specifically eliminated. The variant size difference is an accepted trade-off.

### Tests
- Workspace test count: 560 (v0.3.1) → **612** (v0.4.0). New tests by area:
  - `muxtop-core` cluster_engine (15: kubeconfig detection priority, ClusterError variants, trait dyn-safety + stub, …) + kube data model (15: clone/eq, exhaustive enum matches, round-trip per-type, anti-leak guard) + kube_engine (21: connect rejection paths, Pod/Node/Deployment conversion green + edge cases, metrics injection, quantity parsing, cluster-kind heuristic, end-to-end populated snapshot).
  - `muxtop-tui` ui::kube (8: format buckets, exhaustive label paths, smoke-render unreachable + populated paths through `ratatui::backend::TestBackend`).
  - `muxtop-proto` integration (4: KubeSnapshot round-trip, unavailable sentinel, frame-size guard, anti-leak guard).
  - `muxtop-tui` Tab navigation tests updated to cover the new 5-entry cycle.
- All `cargo check --workspace --all-targets` / `clippy --workspace --all-targets -- -D warnings` / `fmt --check` / `test --workspace` green on macOS Darwin 25 / Rust 2024 stable.

### Out of scope (deferred to v0.4.x)
- Namespace toggle `A` (current-namespace ↔ all-namespaces) on Pods / Deployments — minor UX, defer to v0.4.x.
- Per-row sparklines (CPU + MEM) with a 60-entry `VecDeque` per `(namespace, name)` for the selected pod / node / deployment — substantial sub-state that warrants its own pass.
- Write actions (Delete pod, Scale deployment, Rollout restart) — read-only by design in v0.4.0.
- `kubectl exec` interactive PTY — non-goal for the Kube tab; same call as the deferred `docker exec` PTY (also v0.5+).
- Log streaming — non-goal (`stern` / `k9s` territory).
- `#[ignore]` E2E test against a `kind` cluster (T-818) — needs a CI runner with kind preinstalled.
- ADR-04 follow-up: re-evaluate kube-rs vs `k8s-openapi` direct if a future delta exceeds +5 MiB binary or > +90 s compile wall.

## [0.3.1] - 2026-04-25

Hardening + performance follow-up to the v0.3.0 Containers release. Closes every finding from the 2026-04-25 security & performance audit, plus a build-profile sweep that almost halves the binary size.

### Security

#### Server / wire protocol (`muxtop-server`, `muxtop-proto`)
- **TLS 1.3 only.** `ServerConfig` and `ClientConfig` are now pinned via `builder_with_protocol_versions(&[&TLS13])`; a regression test asserts a TLS-1.2 client handshake fails.
- **Hardened self-signed certificates.** Rebuilt around explicit `CertificateParams` with `iPAddress` + `DnsName` SAN (was DNS-only), `PKCS_ECDSA_P256_SHA256`, 90-day validity. The generated key file is opened with `O_NOFOLLOW` + mode `0600`; the parent data dir is `chmod 0700` (Unix). A `<data_dir>/server.fingerprint` is persisted (mode `0644`) so operators can recover the SHA-256 even if the startup banner is swallowed by systemd / CI.
- **Per-IP token-bucket rate limiter** (default 10/s, configurable via `--rate-limit-per-ip`; `0` disables). No new dependency.
- **`max_clients` semaphore acquired in the accept loop *before* the TLS handshake.** Over-quota TCP streams are dropped silently — no TLS handshake, no Error frame.
- **Pre-auth Hello frame capped at 4 KiB** via `FrameReader::read_frame_with_max_payload(usize)`. Post-handshake reads keep the 4 MiB cap.
- **Allocation-bounded bincode decode** (`config::standard().with_limit::<MAX_DECODE_BYTES>()`); a payload claiming a 100 MiB string is rejected without allocation.
- **`--token-file <path>`** flag on both binaries (mutually exclusive with `--token`). 16-char minimum after trim. The in-memory token is wrapped in a private `Token(String)` newtype that redacts on `Debug`. `--token` help now warns about `/proc/<pid>/cmdline` leakage.
- **Insecure-mode visibility.** `--tls-skip-verify` fires `tracing::warn!(target: "muxtop::insecure")` on every handshake; the CLI prints a bordered banner immediately after parsing.
- **Hostname-aware SNI.** New `muxtop_proto::parse_remote_target(s) -> (SocketAddr, ServerName)`: IP literals → `ServerName::IpAddress`, DNS names → `ServerName::DnsName(host)`. Drops the previous SocketAddr-only parse that forced IP-bound certs.

#### Containers + TUI (`muxtop-core`, `muxtop-tui`)
- **`DOCKER_HOST` exfiltration warning.** `container_engine::detect_with` emits `tracing::warn!` whenever `$DOCKER_HOST` resolves to a non-loopback `http://` / `tcp://` target. New `http_host_is_loopback` helper handles IP literals and bracketed IPv6.
- **Symlinked-socket rejection.** `DockerEngine::connect_explicit(allow_symlink: bool)` is the new primary entry point. Auto-detection refuses to follow a symlinked Unix socket; explicit user paths log a warning but proceed.
- **Per-container stats failure isolation.** `list_and_stats` no longer aborts the whole tab when one container returns `PermissionDenied` / `Timeout` / `Other` on stats — the bad row is dropped with a warn log, the rest render normally.
- **ANSI / control-char sanitizer.** New `tui::ui::sanitize::scrub_ctrl(&str) -> Cow<str>` strips bytes in `0x00..=0x1f` (except `\t`) and `0x7f`, applied at every row-render site that displays attacker-controlled strings (process name/command/user, container name/image/status, network interface name). Closes the terminal-escape spoofing surface.

### Performance

#### Event-driven render (TUI keystone)
- `terminal.draw` is now called only on `Snapshot | Resize | Key | Mouse | needs_redraw_flag | status_message_just_expired`. Tick events no longer trigger an unconditional 60 Hz redraw against 1 Hz data. New `AppState::needs_redraw` flag armed by `apply_snapshot`, `pump_action_results`, `set_status`, and any state-mutating key handler. **Idle CPU drops ~5–10×; render-loop allocations from ~24k/s to near-zero.**

#### Hot-path allocation cuts
- `recompute_visible` no longer calls `filter_processes` twice in tree mode (was both at 866 and 877; now reuses the first result).
- 50 ms debounce on burst typing in the filter (`FILTER_DEBOUNCE` + `last_filter_change`); `Enter` / `Esc` commit immediately.
- `AppState::sorted_filtered_containers_cache` populated in `apply_snapshot` and refreshed on every container sort/filter mutation. `draw_body`, `draw_sparklines`, and `selected_container` read from the cache (was three independent `Vec` clones + sorts per render).
- Sparkline data built single-pass with `iter().skip(len.saturating_sub(N))` (was double-reverse + double collect).
- New `process::contains_ignore_case` helper — ASCII fast path with no per-row `to_lowercase` allocation, falls back to a Unicode-correct path.
- `PaletteState::matcher` caches the nucleo `Matcher` across keystrokes; `Command::search_texts()` interns haystacks via `OnceLock<Vec<String>>`. Result: `palette_refilter/short_query` **5 allocs / 257 B** vs 52 allocs / 134 KB before; `palette_refilter/no_match` **1 alloc / 10 B** vs 49 allocs / 134 KB; `long_query` and `no_match` time **−83 to −85 %**.
- `network::draw_network_tab` pre-computes a `BandwidthMap` once per render and threads it into the summary bar, body, and sort comparators (was O(N² log N) string-compare lookups).
- Server-side `Collector` now uses targeted `refresh_memory_specifics` + `refresh_cpu_usage` + `refresh_processes_specifics(...)` instead of `refresh_all` (was walking `/proc` per-process every tick). Per-core CPU labels interned via `OnceLock<RwLock<Vec<String>>>`.

#### Tree mode + recompute_visible
- `apply_snapshot/tree` allocations: 37 376 / 2 088 KB → **29 374 / 1 744 KB** per tick (**−21 % allocs, −16 % bytes**).
- `recompute_visible/tree/500`: **−37 %** time (statistically significant, p = 0.00).

#### Build profile sweep
- New workspace `[profile.release]`: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "abort"`. **Binary size 9.2 MiB → 5.3 MiB (−42 %)**, with a small win on cold startup (`--about` 14 ms → 12 ms). `mimalloc` was evaluated but degraded RSS on macOS by ~0.6 MiB (Apple `libmalloc` already returns pages aggressively); not adopted.
- Peak RSS: 10.3 MiB → **11.3 MiB** — net cost of v0.3.0 Containers + bollard, not a regression of this release.

#### Container-action hygiene
- Container Stop/Kill/Restart spawns now race their engine call against a `CancellationToken` cancelled in `quit()` — avoids 10 s of detached tasks surviving past TUI shutdown.
- Engine actions now dispatch with `c.id_full.clone()` instead of the truncated 12-char id (closes the Docker prefix-match risk).

### Wire protocol break

- `ContainerSnapshot` gains `id_full: String` (the 64-char ID). bincode is order-sensitive, so this is a wire-format break — pre-v0.3.1 clients cannot decode v0.3.1 snapshots and vice versa.

### Server / CLI follow-up (carrying v0.3.0 functionality across to remote)
- `maybe_connect_default_engine()` extracted from `src/main.rs` and hoisted into `muxtop-core/src/docker_engine.rs` as the single source of truth for both binaries.
- `muxtop-server` gains `--docker-socket <PATH>` and `--no-containers` flags mirroring the client. The server now calls `Collector::with_container_engine`, so remote clients see actual containers in their `Alt+4` tab.

### Tests

- Workspace test count: 488 (v0.3.0) → **560** (v0.3.1) + 1 `#[ignore]` integration test requiring a live Docker daemon. Breakdown of new tests: rate_limit, frame cap, bincode limit, cert generation (parsed via `x509-parser`), TLS 1.3 enforcement, key file permissions, fingerprint persistence, `--token-file` path, hostname SNI parsing, `scrub_ctrl` clean/dirty/tab/OSC/null/multi-byte UTF-8, `connect_explicit` symlink rejection, per-container error isolation, `http_host_is_loopback` truth table, `tick_does_not_request_redraw`, `pump_action_results_marks_dirty`, `apply_snapshot_populates_container_cache`, `quit_cancels_shutdown_token`, `palette_matcher_is_cached`, `filter_debounce_coalesces_bursts`, `broadcast_arc_frame_shared_across_subscribers`, `contains_ignore_case` ASCII + Unicode paths.
- `cargo check / test --workspace / clippy -D warnings / fmt / deny`: all green.

## [0.3.0] - 2026-04-25

Major feature release: the **Containers** tab (replaces `ctop`) with full Docker/Podman integration via [bollard](https://github.com/fussybeaver/bollard). Auto-detection at startup means `muxtop` gains a fourth tab on any host running a container engine with no extra flags.

### Added

#### Containers (`muxtop-core`, `muxtop-tui`, `muxtop`)
- New `Tab::Containers` (keybind `Alt+4`) with a full rendering path in `muxtop-tui/src/ui/containers.rs`: sortable table of containers with columns NAME / IMAGE (truncated to 30 chars) / STATE / CPU % / MEM used/limit / NET RX/TX / UPTIME, color-coded by state (running=green, paused/restarting=yellow, dead=red, exited/created=dim), zebra stripes, summary bar with engine kind + running/total counts.
- Per-selected-row sparklines: CPU % and RX-delta (60-sample rings per container id, dropped when a container disappears).
- Sort cycles 6 fields: CPU, Mem, Name, NetRx, NetTx, Uptime (`s` cycles, `I/S` toggles direction, header arrow).
- Filter by name / image / id (`/` to open, `Esc` to clear).
- Container actions: `F9` Stop (SIGTERM + 10s grace), `F10` Kill (SIGKILL), `F11` Restart, each gated by a y/n confirmation dialog. Disabled in remote mode with the same notice style as Processes kill/renice.
- 5 new palette commands: `SwitchToContainers`, `SortContainersByCpu/Mem/Name/NetRx`. 3 additional action commands (`StopContainer`, `KillContainer`, `RestartContainer`) with `F9`/`F10`/`F11` shortcut labels and exclusion from the palette in remote mode.
- Three render fallbacks: `containers = None` → "Waiting for data...", engine configured but `daemon_up = false` → "No container daemon detected" with a CLI hint, empty list → "No containers" or "No containers match filter".

#### Container engine (`muxtop-core`)
- `ContainerEngine` async trait (`async-trait` crate, see ADR-01 in `forge/24-epic1-container-engine-trait`) with methods `list_and_stats`, `stop`, `kill`, `restart`, `kind`.
- `DockerEngine` concrete implementation on top of `bollard 0.20`: handles Unix socket + HTTP/TCP targets, probes `/info` within 5 s, detects Docker / Podman / Unknown, fetches stats in parallel via `futures::stream::buffer_unordered(16)`, filters `ContainerNotFound` silently on race-with-removal.
- CPU percentage computed client-side from a cached `(cpu_usage, system_cpu_usage)` per container with `saturating_sub` on counter resets. First tick after startup yields 0 % — acceptable 2 s warm-up at the collector's 0.5 Hz refresh rate.
- Socket auto-detection (`detect_socket`) with fallback chain: `$DOCKER_HOST` → `/var/run/docker.sock` → `$XDG_RUNTIME_DIR/podman/podman.sock` → `/run/podman/podman.sock`. Pure path selection only (reachability is `DockerEngine::connect`'s job).
- `EnvLookup` trait for parallel-safe tests (no `std::env` global mutation).
- `EngineError` enum with granular variants (`ConnectFailed`, `ContainerNotFound`, `PermissionDenied`, `Timeout`, `Other`) and a `#[from] EngineError` bridge to `CoreError`.
- `Collector::with_container_engine(interval, Option<Arc<dyn ContainerEngine + Send + Sync>>)`: drives a second `tokio::time::interval(2s)` task that calls the engine and publishes the result (or `ContainersSnapshot::unavailable()`) into a shared `Arc<Mutex<Option<ContainersSnapshot>>>`. Each system-tick `SystemSnapshot` carries the latest container snapshot through the new `containers: Option<ContainersSnapshot>` field.

#### CLI (`muxtop`)
- `--docker-socket <PATH>` flag to override autodetection.
- `--no-containers` flag to disable the container engine entirely.
- `maybe_build_container_engine()` runs autodetection + `DockerEngine::connect` at startup; on failure it logs a tracing warning and degrades to a None engine so muxtop always boots. The built `Arc<dyn ContainerEngine>` is cloned into both the Collector (stats) and the TUI (actions) so both hit the same daemon.

#### Wire protocol (`muxtop-proto`)
- `ContainerSnapshot`, `ContainersSnapshot`, `ContainerState` (7 variants), `EngineKind` derive `Serialize`, `Deserialize`, `Encode`, `Decode`, `PartialEq`, `Clone`, `Debug` so they cross the wire via `WireMessage::Snapshot(SystemSnapshot)` unchanged.
- Integration tests: 20-container round-trip, `unavailable()` sentinel round-trip, 100-container frame-size sanity check (< 256 KiB vs the 4 MiB `MAX_FRAME_SIZE`).
- Criterion benches `containers_serialize_100` + `containers_deserialize_100` for regression tracking.

### Dependencies (workspace)
- `async-trait = "0.1"` — dyn-safe async trait macro (see ADR-01 in forge/24).
- `bollard = "0.20"` — Docker/Podman client (brings `hyper 1`, `http 1`, `futures 0.3`).
- `futures = "0.3"` — `stream::buffer_unordered`.
- `tempfile = "3"` added as dev-dep to `muxtop-core` for socket-detection tests.

### Changed
- `SystemSnapshot::collect` signature gained a third argument `containers: Option<ContainersSnapshot>`. All internal call sites updated; the Collector is the sole production caller and passes the latest container snapshot from its shared slot.
- `muxtop_tui::run` signature gained an `Option<Arc<dyn ContainerEngine + Send + Sync>>` parameter. `src/main.rs` forwards the autodetected engine; passing `None` disables actions (they surface "Container engine not configured" as a status message).
- `Tab::ALL` now has 4 variants; `Tab::next()` / `Tab::prev()` cycle through General → Processes → Network → Containers. Arrow / Tab / BackTab navigation updated accordingly.
- `FUTURE_TABS` in the tab bar no longer shows "Containers [soon]" — only "GPU [soon]" remains.

### Tests
- Workspace test count: 421 (v0.2.2) → **488** (v0.3.0). Breakdown of the +67 new tests: `muxtop-core` containers/container_engine/docker_engine (+44), `muxtop-tui` ui::containers + app container actions (+19), `muxtop-proto` integration (+4). One new `#[ignore]` integration test requires a live Docker daemon (run with `cargo test -- --ignored`).
- `cargo-deny check` remains clean with the new transitive deps (hyper 1.9, http 1.4, tokio-util features).

## [0.2.3] - 2026-04-24

### Added
- `scripts/bench-thomas.sh` now measures peak RSS over a 30 s headless collector run (uses `/usr/bin/time -l` on macOS, `/usr/bin/time -v` on Linux). Gives a publishable memory footprint number for comparison with other monitors.
- Hidden `--bench-run <secs>` flag on the `muxtop` binary: runs the collector + `AppState::apply_snapshot` / `recompute_visible` loop without a TUI, then exits. Lets external tools measure steady-state RSS without a TTY.
- `cargo run --example alloc_profile -p muxtop-tui` (also `just bench-alloc`) — runs the hot paths (`PaletteState::refilter`, `sort_processes`, `AppState::apply_snapshot`) under the `dhat` global allocator and reports per-iteration allocation counts and bytes. Complements the criterion time benches for catching allocation regressions.

### Security
- Bump `rustls-webpki` to 0.103.13 to remediate **RUSTSEC-2026-0104**.

## [0.2.2] - 2026-04-20

### Performance
- `PaletteState::refilter_excluding` no longer allocates a throwaway `Vec<Command>` on every call; the empty-input hot path is **−84 %** faster (178 ns → 28 ns). Other palette variants improve 3–11 %.
- `sort_processes` uses `sort_by_cached_key` for `Name` / `User` fields so `to_lowercase()` runs O(n) instead of O(n log n) times. `name_asc/5000` drops from 4.69 ms to **765 µs (−84 %)**; `cpu_desc/5000` from 966 µs to **436 µs (−55 %)**.
- `muxtop --about` no longer builds a Tokio multi-threaded runtime before printing. `main()` is now synchronous and constructs the runtime only when entering the TUI path. Cuts `--about` startup from an effective cold-start cost to ~18 ms on warm runs.

### Fixed
- `scripts/bench-thomas.sh` now warms up the binary with `--version` before timing `--about`, so measurements don't capture the one-time macOS Gatekeeper scan cost of a freshly-rebuilt binary.

## [0.2.1] - 2026-04-16

### Fixed
- Clippy lints: replaced `sort_by` with `sort_by_key` for cleaner sort expressions, and collapsed single-branch `if` blocks inside `match` arms into match guards.
- CI: fixed `cargo publish` workflow to include `muxtop-proto` in the correct dependency order, and fixed a bash `errexit` bug that silently swallowed publish errors.

## [0.2.0] - 2026-04-16

### Added

#### Network (`muxtop-core` — Epic 11)
- `NetworkInterfaceSnapshot`, `NetworkSnapshot`, and `NetworkHistory` types in a new `network` module tracking per-interface bytes, packets, and errors.
- `NetworkHistory` circular buffer (60-entry `VecDeque`) computing bandwidth (bytes/s with timestamp-based division) and sparkline data.
- `SystemSnapshot` extended with a `networks` field collected from `sysinfo::Networks` on each tick.
- Guard hardening: saturating arithmetic for totals, capacity min 2, counter-reset handling, `refresh(false)` in hot path.

#### Network tab (`muxtop-tui` — Epic 12)
- New `Tab::Network` with `Alt+3` keybinding and full navigation.
- Interface table with columns: Interface, State, RX/s, TX/s, Total RX/TX, Errors; color-coded rates (green RX, yellow TX, red errors).
- Summary bar showing total bandwidth and active/total interface count.
- RX/TX sparklines for the selected interface using `NetworkHistory`.
- Per-tab selection, scroll, sort (6 fields: name, rx rate, tx rate, total rx, total tx, errors), and filter state.
- 5 new command palette commands: `SwitchToNetwork`, `SortNetByRx/Tx/Name/Errors`.

#### Wire protocol (`muxtop-proto` — Epic 13)
- New `muxtop-proto` crate implementing the muxtop wire protocol.
- Length-prefixed framing: 4B big-endian length + 1B message type + bincode payload.
- Async `FrameReader` / `FrameWriter` over `tokio::AsyncRead` / `AsyncWrite`.
- `WireMessage` enum: `Snapshot`, `Heartbeat`, `Error`, `Hello`, `Welcome`.
- `MAX_FRAME_SIZE` capped at 4 MiB to limit DoS surface.
- `Serialize`, `Deserialize`, `Encode`, `Decode`, and `PartialEq` derives on all public core types.
- `SystemSnapshot.timestamp` migrated from `Instant` to `timestamp_ms: u64` (milliseconds since Unix epoch) to enable wire serialization.

#### Server daemon (`muxtop-server` — Epic 14)
- New `muxtop-server` crate: TCP daemon that broadcasts system snapshots to connected clients over the muxtop wire protocol.
- Hello/Welcome handshake, token authentication (`--token` / `MUXTOP_TOKEN`), and constant-time comparison.
- `--max-clients` semaphore limiting concurrent connections.
- Heartbeat frame emitted every 5 seconds per client.
- Snapshot broadcast relay from the local collector.
- Graceful shutdown via `CancellationToken`.

#### Remote monitoring (`muxtop-proto` + `muxtop-tui` + CLI — Epic 15)
- `RemoteCollector` TCP client in `muxtop-proto`: connects to a `muxtop-server`, performs Hello/Welcome handshake, and streams `SystemSnapshot` frames through the same `mpsc` channel interface as the local `Collector`.
- Exponential backoff reconnection (1 s → 30 s cap, resets on successful connection).
- `ConnectionEvent` channel for real-time TUI status notifications.
- `--remote host:port` CLI flag: spawns `RemoteCollector` instead of local `Collector`.
- `--token` flag and `MUXTOP_TOKEN` env var for server authentication.
- `ConnectionMode` enum (`Local` | `Remote { hostname, addr }`) in `CliConfig` and `AppState`.
- Remote mode TUI: header displays `→ remote:hostname:port`; kill/renice actions and palette commands disabled with a clear notice; footer hides Kill/Nice hints; warning emitted when `--refresh` is combined with `--remote`.

#### TLS & Security Hardening (`muxtop-server`, `muxtop-proto`, CLI)
- TLS encryption for all client-server communication via `tokio-rustls` (rustls 0.23). All data is now encrypted in transit — tokens, snapshots, and heartbeats are never sent in plaintext.
- Self-signed certificate auto-generation with `--tls-generate` via `rcgen`: generates cert+key, prints SHA-256 fingerprint to stderr, persists to `~/.local/share/muxtop/`.
- Server TLS configuration: `--tls-cert` / `--tls-key` flags for PEM-encoded certificate and private key files.
- Client TLS verification: `--tls-ca <path>` to trust a specific CA/self-signed cert, `--tls-skip-verify` for development (insecure, with warning).
- Mandatory authentication: server refuses to start without `--token` / `MUXTOP_TOKEN` (minimum 16 characters). Client requires `--token` for `--remote` connections. No more unauthenticated plaintext mode.
- `WireMessage` custom `Debug` impl redacting `auth_token` as `[REDACTED]` to prevent accidental token leakage in logs.
- TLS handshake timeouts on both server (10s) and client (5s) to prevent slowloris-style resource exhaustion.
- Private key file created with `0o600` permissions atomically on Unix (no TOCTOU race).
- Generic `AsyncRead`/`AsyncWrite` handler in `client::handle()` — works transparently with TLS streams.
- 6 new TLS integration tests: TLS handshake, snapshot streaming over TLS, cert rejection, skip-verify, auth rejection over TLS, full streaming.

#### Tests & Benchmarks (Epic 16)
- 7 new `muxtop-core` network edge-case unit tests: multi-interface, empty snapshots, sparkline TX, bandwidth, and `is_up` heuristic.
- 2 new `muxtop-server` E2E tests: multi-snapshot streaming (3 snapshots) and snapshot content verification (CPU, memory, processes, networks, timestamp fields).
- Network benchmarks: `NetworkSnapshot::collect`, `NetworkHistory::push_60`, bandwidth calculation with sparklines.
- Proto benchmarks: snapshot serialize/deserialize with 3 000 processes, frame encode/decode round-trip.

#### Documentation
- `CONTRIBUTING.md`: contributor guide covering prerequisites, dev setup, crate architecture, branch model, commit conventions, code standards, and PR process.

---

## [0.1.1] - 2026-04-15

### Added

#### Distribution
- `.deb` package generation for Linux targets (x86_64 and aarch64) via `cargo-deb`, attached to GitHub Releases for Debian/Ubuntu installation.
- Homebrew tap (`lucasschimmel/homebrew-tap`) with a formula supporting macOS (Intel + Apple Silicon) and Linux (x86_64 + aarch64).
- Automatic Homebrew formula update in the release workflow on each new tag.

### Fixed

#### Security
- Addressed findings from security audit SEC-20260415: refactored action handling in `muxtop-core`, hardened confirmation dialog, and reduced collector surface area.

## [0.1.0] - 2026-04-15

Initial release of **muxtop** — a modern, multiplexed system monitor for the terminal.

### Added

#### Core (`muxtop-core`)
- `SystemSnapshot` collecting CPU, memory, swap, and per-process data via `sysinfo`.
- Process sort (CPU, memory, PID, name, user), filter, and tree builder (parent/child hierarchy).
- Async 1 Hz collector running on a dedicated `tokio` task with graceful shutdown via a cancel token.
- Kill (`SIGTERM`/`SIGKILL`) and renice actions on live processes using `libc`.
- `Display` and `FromStr` implementations for `SortField`, enabling case-insensitive CLI parsing.
- End-to-end integration tests for the collector and process pipeline.
- Criterion benchmark targets: `process_bench` (sort, filter, tree, flatten at 100–5000 procs) and `snapshot_bench` (full `SystemSnapshot::collect`).

#### TUI (`muxtop-tui`)
- Terminal lifecycle management: raw mode, alternate screen, RAII restore guard, and panic hook that restores the terminal before unwinding.
- `AppState` with `Tab` enum, keyboard input dispatch, and mpsc snapshot consumption.
- Crossterm event loop with non-blocking polling and per-frame snapshot drain.
- 4-zone layout: header, tab bar, scrollable content area, and footer.
- `Alt+1` / `Alt+2` and arrow-key tab navigation between General and Processes tabs.
- **General tab**: per-core CPU gauge bars, memory and swap bars, and a system info line (hostname, OS, uptime, kernel).
- **Processes tab**: sortable table (CPU, memory, PID, name, user), inline filter bar (`/`), process tree toggle (`F5`), and column header indicators.
- **Command palette** (`Ctrl+P`): fuzzy-matched command registry powered by `nucleo`; commands for sort, filter, kill, and navigation.
- Kill and renice workflow: `F9` (SIGTERM), `F10` (SIGKILL), `F7` / `F8` (renice ±1) behind a `y`/`n` confirmation dialog.
- `ConfirmAction` enum with per-action prompt text rendered as a centered overlay.
- Status message bar in the footer with auto-clear after 5 seconds; green for success, red for error.
- `Esc` clears the active filter; `I` reverses sort order.
- `CliConfig` struct carrying `--filter`, `--sort`, and `--tree` flags from the CLI into `AppState`.
- `TermCaps` with `ColorSupport` detection from `$TERM` / `$COLORTERM` / `$LANG` at startup.
- `detect_terminal_caps()` for runtime color and Unicode detection.
- **Tokyo Night** TrueColor theme (`theme.rs`) with full palette (background, foreground, accents, status colors), ANSI 16-color fallback for basic terminals, and a `gauge_color()` helper for green/yellow/red gradients.
- Alternating zebra-stripe row backgrounds in the Processes table using the `surface` theme color.
- Bold selected row text and cyan (`accent_primary`) column headers for stronger visual hierarchy.
- Powerline-style system info bar and footer key-hint strip.
- ASCII fallback for non-Unicode terminals: block characters (`#`/`-`), sort arrows (`v`/`^`), tree connectors (`|--`/`\--`), filter cursor (`_`).
- Unit tests covering `CliConfig`, `ConfirmAction::prompt()`, `next_sort_field()`, `AppState::with_config()`, and edge cases (empty snapshot, `PageDown`/`PageUp`/`Home`/`End`).
- Criterion benchmark target `app_bench`: `recompute_visible` (flat, tree, filtered) and palette re-filter.

#### CLI & Distribution
- `--filter <PATTERN>` to pre-seed the process filter on launch.
- `--sort <FIELD>` to set the initial sort column (cpu, mem, pid, name, user).
- `--tree` to start in process tree view.
- `--refresh <HZ>` to override the collector tick rate.
- `--about` flag printing version, license, repository URL, and a no-telemetry pledge.
- POSIX-compatible `scripts/install.sh`: detects OS/arch, downloads the correct binary from GitHub Releases, verifies SHA-256 checksum, and installs to `/usr/local/bin` (root) or `~/.local/bin` (non-root).
- GitHub Actions release workflow uploading `install.sh` alongside pre-built binaries and checksum files.

#### CI / Tooling
- GitHub Actions CI pipeline: `cargo check`, `clippy`, `test`, `fmt`, `cargo-deny` audit, and a bench compile check (`--no-run`) on every push and pull request.
- `cargo-deny` configuration for license and advisory auditing (deny.toml, cargo-deny 0.19 schema).
- `clippy.toml` with MSRV pinned to 1.88.
- `scripts/bench-thomas.sh` macro-benchmark measuring release build time, binary size, startup latency, and all CLI flag paths.

#### Documentation
- Launch-ready README with tagline, badges, one-liner install (Cargo + curl), feature overview, benchmark results, keybinding reference, roadmap, privacy pledge, contributing guide, and license.

### Fixed
- TUI clippy warnings: `items_after_test_module` (moved `run()` above `#[cfg(test)]`) and `io_other_error` (use `std::io::Error::other()`).
- Security: bump `time` crate to v0.3.47 to remediate **RUSTSEC-2026-0009** (stack exhaustion via crafted RFC 2822 input).
- General tab layout: compute CPU block height dynamically from core count to eliminate the large empty gap when few cores are present.
- Wrap Memory bars in a bordered block consistent with the CPU block style.

### Changed
- MSRV bumped from 1.85 to 1.88 to pull in `time` v0.3.47 and enable let-chain collapsing.
- `deny.toml` migrated to cargo-deny 0.19 schema (removed deprecated `advisory` / `license` top-level fields).
- `muxtop-tui::run(rx)` signature extended to `run(rx, config)` accepting `CliConfig` + `TermCaps`.
- `bar_empty` color separated from `selection_bg` so gauge empty portions no longer inherit the selection highlight.
