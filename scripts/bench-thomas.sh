#!/usr/bin/env bash
# bench-thomas.sh — Macro-benchmark for muxtop
#
# Measures:
#   1. Release build time
#   2. Binary size, with the section breakdown that explains it
#   3. Startup latency, amortised over many execs
#   4. Peak memory during a headless bench run — both the RSS a user sees in
#      `top` and, on macOS, the physical footprint the kernel actually charges
#
# Usage:
#   ./scripts/bench-thomas.sh
#   just bench-thomas
#
# ---------------------------------------------------------------------------
# Why this script does not look like a stopwatch
#
# Two of the three headline figures it used to print were artefacts of how it
# measured, not properties of the binary, and both errors ran in the flattering
# direction. They are worth stating so nobody reintroduces them.
#
# * **Startup was measured below the noise floor.** Wrapping a single `exec`
#   in two `date` calls charges the measurement for two process spawns. On a
#   MacBook Air M3 two back-to-back `date` calls with nothing between them cost
#   6–19 ms — and the script was reporting ~12 ms for a binary that actually
#   starts in ~6 ms. Roughly half the published figure was the stopwatch. The
#   error flattered nothing, it simply made the number meaningless: any change
#   worth less than 10 ms was invisible, which is every change we are likely to
#   make. Startup is now timed over `STARTUP_ITERS` execs inside one window, so
#   the two `date` spawns amortise away, and the harness's own per-exec share is
#   printed beside the result rather than left for the reader to trust.
#
# * **Peak RSS is not the memory muxtop costs — on macOS.** `/usr/bin/time -l`
#   reports mach's `resident_size`, which counts clean, file-backed, shared
#   pages of the dyld shared cache: the text of every system dylib the process
#   touched, shared with every other process on the machine and never written.
#   Measured on the same run, `resident_size` reads ~12.5 MiB where the
#   `phys_footprint` the kernel bills reads ~5.8 MiB. Neither number is wrong;
#   they answer different questions. RSS is what a user sees in `top` and is
#   the only figure comparable with htop and btop, so it stays. Footprint is
#   what we can actually reduce, so it is what regressions are judged on.
#
# A single sample of either is noisy — run-to-run spread of ±1.5 MiB is normal,
# driven by how many processes are alive on the host. Everything below is
# repeated `REPS` times and reported as a median with its observed range.
# ---------------------------------------------------------------------------

set -euo pipefail

BOLD='\033[1m'
DIM='\033[2m'
GREEN='\033[32m'
YELLOW='\033[33m'
CYAN='\033[36m'
RESET='\033[0m'

BIN="${BIN:-target/release/muxtop}"
BENCH_RUN_SECS="${BENCH_RUN_SECS:-30}"
REPS="${REPS:-5}"
STARTUP_ITERS="${STARTUP_ITERS:-200}"

# --- timing helpers --------------------------------------------------------

# Nanoseconds since the epoch. GNU date and macOS 26's date both support %N;
# older macOS prints a literal "N", which is not a number, so we fall back to
# python3. Only ever called twice per measurement window, never per iteration.
if [ "$(date +%N 2>/dev/null)" != "N" ] 2>/dev/null; then
    now_ns() { date +%s%N; }
else
    now_ns() { python3 -c 'import time; print(int(time.time()*1e9))'; }
fi

# median of the numeric arguments (integers or decimals)
median() {
    printf '%s\n' "$@" | sort -g | awk '{v[NR]=$1} END {
        if (NR % 2) printf "%s", v[(NR+1)/2]
        else printf "%.2f", (v[NR/2] + v[NR/2+1]) / 2
    }'
}

range() {
    printf '%s\n' "$@" | sort -g | awk 'NR==1 {lo=$1} END {printf "%s–%s", lo, $1}'
}

printf "${BOLD}${CYAN}muxtop macro-benchmark (Thomas)${RESET}\n"
printf "${DIM}──────────────────────────────────${RESET}\n"
printf "${DIM}%s reps · %s startup iters · %ss bench-run · %s processes alive${RESET}\n\n" \
    "$REPS" "$STARTUP_ITERS" "$BENCH_RUN_SECS" "$(ps ax | wc -l | tr -d ' ')"

# --- 1. build --------------------------------------------------------------

printf "${BOLD}[1/4] Building release binary...${RESET}\n"
build_start=$(now_ns)
cargo build --release --quiet 2>&1
build_end=$(now_ns)
build_ms=$(( (build_end - build_start) / 1000000 ))
printf "  Build time: ${GREEN}${build_ms} ms${RESET}\n"

if [ ! -f "$BIN" ]; then
    printf "  Binary not found at %s\n" "$BIN"
    exit 1
fi

# --- 2. size ---------------------------------------------------------------

printf "\n${BOLD}[2/4] Binary size...${RESET}\n"
size_bytes=$(wc -c < "$BIN" | tr -d ' ')
size_mib=$(awk -v b="$size_bytes" 'BEGIN {printf "%.2f", b/1048576}')
printf "  Binary size: ${GREEN}${size_mib} MiB${RESET} (${size_bytes} bytes)\n"

# The section breakdown is what makes a size regression diagnosable: a jump in
# __text is code, a jump in __const/__cstring is data or string literals.
if [ "$(uname)" = "Darwin" ] && command -v size > /dev/null 2>&1; then
    size -m "$BIN" 2>/dev/null \
        | awk '/Section __text|Section __const|Section __cstring/ {
                 printf "    %-12s %10d B\n", $2, $3 }' \
        | head -4
elif command -v size > /dev/null 2>&1; then
    size "$BIN" 2>/dev/null | tail -1 | awk '{printf "    text %s  data %s  bss %s\n", $1, $2, $3}'
fi

# --- 3. startup ------------------------------------------------------------

printf "\n${BOLD}[3/4] Startup latency (%s execs per window)...${RESET}\n" "$STARTUP_ITERS"

# Warm-up: the first execution of a freshly-built binary on macOS pays a
# Gatekeeper / XProtect scan (~500 ms) that has nothing to do with our code.
"$BIN" --version > /dev/null 2>&1 || true

# The same window with no exec in it. Divided by the same iteration count,
# this is precisely how much of each figure below is the harness rather than
# muxtop — which is the number the old single-exec method could not show, and
# the reason it reported ~12 ms for a ~6 ms binary.
floor_start=$(now_ns)
for _ in $(seq 1 "$STARTUP_ITERS"); do :; done
floor_end=$(now_ns)
floor_us=$(( (floor_end - floor_start) / STARTUP_ITERS / 1000 ))

# Prints its own report line and leaves the median in STARTUP_MEDIAN, rather
# than echoing it — a function that did both would have its display line
# swallowed by the caller's command substitution.
STARTUP_MEDIAN=""
startup_entry() {
    local label="$1"; shift
    local samples=()
    for _ in $(seq 1 "$REPS"); do
        local s e
        s=$(now_ns)
        for _ in $(seq 1 "$STARTUP_ITERS"); do "$@" > /dev/null 2>&1 || true; done
        e=$(now_ns)
        samples+=( "$(awk -v n="$((e - s))" -v i="$STARTUP_ITERS" 'BEGIN {printf "%.2f", n/i/1000000}')" )
    done
    STARTUP_MEDIAN=$(median "${samples[@]}")
    printf "  %-22s ${GREEN}%6s ms${RESET} ${DIM}(range %s)${RESET}\n" \
        "$label" "$STARTUP_MEDIAN" "$(range "${samples[@]}")"
}

startup_entry "--about"   "$BIN" --about
startup_ms="$STARTUP_MEDIAN"
startup_entry "--version" "$BIN" --version
startup_entry "--help"    "$BIN" --help
startup_entry "bad flag"  "$BIN" --invalid-flag
printf "  ${DIM}%-22s %6s µs/exec — harness share of the figures above${RESET}\n" \
    "(measurement cost)" "$floor_us"

# --- 4. memory -------------------------------------------------------------

printf "\n${BOLD}[4/4] Peak memory over %ss headless runs...${RESET}\n" "$BENCH_RUN_SECS"

rss_samples=()
fp_samples=()

for _ in $(seq 1 "$REPS"); do
    rss_tmp=$(mktemp)

    if [ "$(uname)" = "Darwin" ]; then
        /usr/bin/time -l "$BIN" --bench-run "$BENCH_RUN_SECS" > /dev/null 2>"$rss_tmp" &
        timed_pid=$!

        # Sample the footprint mid-run. vmmap needs the muxtop pid, not the
        # `time` wrapper's, and it can only suspend a target once — so this is
        # the single vmmap call per run.
        sleep $(( BENCH_RUN_SECS / 2 ))
        mux_pid=$(pgrep -P "$timed_pid" -n muxtop 2>/dev/null || true)
        [ -z "$mux_pid" ] && mux_pid=$(pgrep -n muxtop 2>/dev/null || true)
        if [ -n "$mux_pid" ]; then
            fp_kb=$(vmmap -summary "$mux_pid" 2>/dev/null \
                | awk -F': *' '/^Physical footprint \(peak\):/ {gsub(/K/,"",$2); print $2+0}')
            [ -n "${fp_kb:-}" ] && [ "${fp_kb:-0}" != "0" ] \
                && fp_samples+=( "$(awk -v k="$fp_kb" 'BEGIN {printf "%.2f", k/1024}')" )
        fi

        wait "$timed_pid" 2>/dev/null || true
        rss_bytes=$(awk '/maximum resident set size/ {print $1}' "$rss_tmp")
        [ -n "${rss_bytes:-}" ] \
            && rss_samples+=( "$(awk -v b="$rss_bytes" 'BEGIN {printf "%.2f", b/1048576}')" )
    else
        /usr/bin/time -v "$BIN" --bench-run "$BENCH_RUN_SECS" > /dev/null 2>"$rss_tmp"
        rss_kb=$(awk -F': *' '/Maximum resident set size/ {print $2}' "$rss_tmp")
        [ -n "${rss_kb:-}" ] \
            && rss_samples+=( "$(awk -v k="$rss_kb" 'BEGIN {printf "%.2f", k/1024}')" )
    fi

    rm -f "$rss_tmp"
done

if [ ${#rss_samples[@]} -gt 0 ]; then
    rss_mib=$(median "${rss_samples[@]}")
    printf "  %-22s ${GREEN}%6s MiB${RESET} ${DIM}(range %s) — comparable with htop/btop${RESET}\n" \
        "Peak RSS" "$rss_mib" "$(range "${rss_samples[@]}")"
else
    rss_mib="?"
    printf "  %-22s ${YELLOW}unavailable${RESET}\n" "Peak RSS"
fi

if [ ${#fp_samples[@]} -gt 0 ]; then
    fp_mib=$(median "${fp_samples[@]}")
    printf "  %-22s ${GREEN}%6s MiB${RESET} ${DIM}(range %s) — what the kernel charges us${RESET}\n" \
        "Peak footprint" "$fp_mib" "$(range "${fp_samples[@]}")"
else
    fp_mib="n/a"
    [ "$(uname)" = "Darwin" ] && printf "  %-22s ${YELLOW}unavailable${RESET}\n" "Peak footprint"
fi

# --- summary ---------------------------------------------------------------

printf "\n${BOLD}${CYAN}Summary${RESET} ${DIM}(medians of %s reps)${RESET}\n" "$REPS"
printf "${DIM}──────────────────────────────────${RESET}\n"
printf "  Build:            %8s ms\n" "$build_ms"
printf "  Binary:           %8s MiB\n" "$size_mib"
printf "  Startup:          %8s ms   (--about)\n" "$startup_ms"
printf "  Peak RSS:         %8s MiB  (over ${BENCH_RUN_SECS}s)\n" "$rss_mib"
printf "  Peak footprint:   %8s MiB  (over ${BENCH_RUN_SECS}s)\n" "$fp_mib"
printf "\n${GREEN}Done.${RESET}\n"
