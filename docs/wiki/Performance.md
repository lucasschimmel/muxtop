# Performance

muxtop's premise is that a monitor which costs noticeable resources is a monitor
that changes what it measures. The budgets below are treated as constraints, not
aspirations — a feature that breaks one does not ship until it stops breaking it.

## Published figures

Measured on a MacBook Air M3 (8 GB, macOS 26.5.1) with ~430 processes, via the
repository's own benchmark. Medians of five runs, ranges beside them:

| Metric | Budget | Measured |
|---|---|---|
| Startup (`--about`) | < 100 ms | **6.1 ms** (5.8–6.1) |
| Binary size | < 10 MiB | **6.66 MiB** (LTO + strip) |
| FPS (TUI) | > 30 | ~60, event-driven — idle ≈ 0 redraws |
| Peak RSS (30 s) | < 15 MiB | **12.56 MiB** (htop ≈ 15, btop ≈ 40) |
| Peak footprint (30 s) | — | **5.66 MiB** |

```sh
just bench-thomas       # macro: build, size, startup, RSS + footprint
just bench              # criterion micro-benchmarks
just bench-alloc        # dhat heap profile of the hot paths
```

Numbers move with hardware, process count and terminal. Reproduce them on your
own box before comparing.

### Why two memory numbers

macOS reports two, and they differ by more than a factor of two, so publishing
one alone means picking a story.

**Peak RSS** is mach's `resident_size` — what `top` and Activity Monitor's
older columns show. It counts clean, file-backed, *shared* pages of the dyld
shared cache: the text of every system library the process touched, shared with
every other process on the machine and never written by us. It is the only
figure comparable with the htop and btop numbers above, which is why it stays.

**Physical footprint** is `phys_footprint` — what the kernel actually charges
the process, and roughly what muxtop would free if it exited. It is the half
that responds to optimisation.

A change that reduces code the process never pages in — most binary-size work —
moves neither. A change that removes a per-tick allocation moves footprint and
may leave RSS flat. Judging memory work on RSS alone therefore reports most
real savings as zero, and reports loading one extra system framework as a
multi-megabyte regression.

**These figures were previously measured wrong, in both directions.** The table
above stood at 5.3 MiB binary and 11.3 MiB RSS while describing v0.3.1, four
releases after that binary stopped existing. The startup figure was worse than
stale: the benchmark timed a single process spawn between two `date` calls,
which on this machine cost 6–19 ms on their own, so it reported ~12 ms for a
binary that starts in about 6. Roughly half the published number was the
stopwatch. Both are fixed in `scripts/bench-thomas.sh`, which now amortises
startup over many execs and prints its own measurement cost beside the result.

## Why it is cheap

**Rendering is event-driven, not frame-driven.** An idle muxtop does
approximately zero redraws — it repaints on a key press, a mouse event, a
terminal resize or a new snapshot, and otherwise sleeps. This is where most of
the difference against a fixed-FPS TUI comes from.

**Collection runs on four independent loops**, each at the rate its data source
deserves:

| Loop | Rate | Why |
|---|---|---|
| System / processes | 1 Hz | `sysinfo`, cheap, and the numbers change every second |
| Containers | 0.5 Hz | The engine computes stats per container — the expensive one |
| Cluster | 0.2 Hz | API server round trips are slow and the data is slow-moving |
| GPU | 1 Hz | NVML queries and sysfs reads are cheap |

Nothing blocks the UI: collection is `tokio` tasks, and the render loop reads the
latest snapshot rather than waiting for one. A cluster that stops responding
freezes its own tab's data, not your keyboard.

**The release profile is tuned for size and speed**: `lto = "fat"`,
`codegen-units = 1`, `strip = "symbols"`, `panic = "abort"`. That is what turns a
multi-megabyte debug build into a 5.3 MiB static binary.

---

## Tuning on a large host

### Poll less often

```sh
muxtop --refresh 5      # seconds, 1–3600
```

This is the single biggest lever. On a host you are watching rather than
debugging, 5 or 10 seconds is plenty, and it scales down every loop.

### Drop what you are not looking at

```sh
muxtop --no-containers    # skip container stats — usually the most expensive
muxtop --no-kube          # skip API server polling
muxtop --no-gpu           # skip the NVML / sysfs probe
```

Each flag removes both the startup probe and the polling loop. On a host with
2000 containers, `--no-containers` is dramatic.

### On thousands of processes

The process table is sorted and filtered on every snapshot. At 3000+ processes:

- Start filtered — `--filter <pattern>` — so the table you sort is smaller
- Tree view (`t`) costs more than the flat list: it builds the parent/child
  hierarchy each cycle
- Raise `--refresh`

muxtop is designed to stay responsive at 3000+ processes. If it does not on your
host, that is a bug worth
[reporting](https://github.com/lucasschimmel/muxtop/issues) with the process
count and the profile.

### On the server side

`muxtop-server` pays the collection cost *and* the serialisation cost of every
snapshot, for every connected client:

```sh
muxtop-server --refresh 5 --max-clients 4 --token-file /etc/muxtop/token --tls-generate
```

`--max-clients` (default 8) bounds the multiplier. Disabling a data source on the
server disables it for everyone, which is usually what you want on a monitoring
host.

---

## Measuring for yourself

```sh
# Startup, the isolated path
time muxtop --about

# Peak RSS over 30 s (Linux)
/usr/bin/time -v muxtop --refresh 1        # look at "Maximum resident set size"

# Peak RSS (macOS)
/usr/bin/time -l muxtop --refresh 1        # look at "maximum resident set size"

# Where the allocations are
just bench-alloc                            # dhat profile of the render hot path
```

If you file a performance issue, the useful numbers are: process count
(`ps aux | wc -l`), container count, whether the Kube and GPU tabs are active,
`--refresh`, and the RSS you measured.

---

## The constraints, for contributors

Any change must keep:

| | Contributor ceiling | Published budget |
|---|---|---|
| Peak RSS (30 s) | **< 13 MiB** | < 15 MiB |
| Binary size | **< 7 MiB** | < 10 MiB |
| Startup (`--about`) | **< 30 ms** | < 100 ms |
| Peak footprint (30 s) | tracked, not yet bounded | — |

**There are two tiers on purpose, and they are not in conflict.** The published
budgets are the promise made to users in the README; the contributor ceilings
are tighter, and they are the ones a pull request is held to. They exist so the
published promise is never the thing under pressure — by the time a change
threatens the user-facing number it has already had to argue with a stricter
one. The issue templates quote the contributor tier because that is what a
proposal has to clear. Neither figure has ever been lowered to accommodate a
measurement.

**The `< 7 MiB` binary ceiling was written before muxtop had a Kubernetes
client, and was exceeded from v0.4 to v0.7.** It was kept rather than restated
upward, and v0.8 brought the binary back under it at 6.66 MiB. A budget you
have drifted past is a debt, not a mistake in the budget.

**Footprint is measured but not yet bounded.** v0.8 is the first release to
report it, and one machine on one platform is not a baseline — a ceiling set
from a single MacBook Air would bind Linux and Windows contributors to a number
nobody measured there. It will be set once there are figures from all three CI
platforms.

A feature that needs more than any of these argues for itself in the pull
request.
