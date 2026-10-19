# Reliability and security audit remediation

**Latest status (2026-10-19):** the Kubernetes migration in the final section resolves the unsafe-libyaml limitation recorded below. All rerun local checks pass, including `just check`, Rust 1.88, strict Rustdoc and seven Criterion benchmark builds. Earlier dated results are retained as history.

Date: 2026-10-14. Base: `perf/v0.8-optimisation-pass` at `b7ca5a4`, clean working tree, five commits ahead of `origin/develop` and none behind at session start. Changes preserve the optimization commits. No release or merge performed.

## Corrections

- Docker CPU history uses the full daemon ID consistently. Uptime comes from inspect `StartedAt`, refreshes across restart, and is explicitly unknown when inspect is unavailable. Inspect adds one bounded request per container per collection within the existing parallel fanout.
- Kubernetes resource publications distinguish complete, partial and failed polls internally. Only successful polls update freshness; total failure becomes unavailable immediately, and monotonic cache expiry emits `ClusterError::Stale`. Failed resource categories are cleared rather than displayed as fresh. Resource and metrics responses from obsolete namespace generations cannot publish. KUBECONFIG path lists use the platform separator and kube's real file-merging semantics.
- Network/GPU render, selection, inspector and copy share sorted/filter projections; Kubernetes inspectors use the existing sorted index caches. GPU process PID/name and device vendor matching agree across all paths. Snapshot shrink clamps selections and scroll offsets on all views. Paused process filtering flushes at its deadline independently of redraw, and on Escape. Textual sorting targets the active subview or explicitly rejects unsupported columns.
- The remote UI consumes ordered local connection/sample updates and persistently marks disconnected, authenticating and failed sessions stale. A reconnected session stays stale until its own accepted fresh sample. A silent open connection expires after three heartbeat intervals.
- Server writes and handshake reads are bounded and cancellable; client tasks are joined on shutdown and release their permits. Closing the collector receiver cancels and joins secondary loops, including pending engine polls.
- Admission rejection logging is aggregated on an interval. Client/server logging uses a bounded nonblocking worker and a single file segment capped at 2 MiB, including oversized existing files and oversized records. `tracing-appender` is now used intentionally.
- Server integration tests call production listener/handshake/client code, replacing copied server implementations. Unused root `tokio-rustls` and `rustls-pki-types` declarations and obsolete dead-code allowances were removed; native-library lifetime guards remain. `just check` now executes cargo-deny, and local/CI Clippy, test and benchmark commands consistently use the lockfile.

## Regression coverage

- Core `t_ac_01_*`: successive full-ID CPU samples, creation versus real start/restart, unavailable inspect.
- Core `t_ac_02_*`: real temporary kubeconfig files, initial/total/partial failures and recovery, monotonic expiry, delayed resource and metrics replies during scope change.
- TUI `audit_*`: sorted/reversed/filtered inspector/copy identity, GPU PID/vendor matching, shrinking/empty lists, active textual sorts and controlled filter deadline through the production idle handler.
- Core/proto/server/TUI `t_ac_04_*`: closed receiver and blocked engine termination, silent remote expiry, heartbeat liveness, actual TLS reconnect and fresh sample, ordered status backpressure, stale UI states, blocked writes with timeout/cancellation and permit release.
- Log regression: file cap across reopen, oversized pre-existing files, repeated records and oversized records. Production server integration covers TLS trust, auth success/wrong/missing token, snapshot contents, multi-client fanout, repeated snapshots, admission before TLS, pre-auth size cap and cancellation of pending TLS/Hello.

## Dependency review

Rustls is locked at `0.23.46` (from `0.23.38`); webpki at `0.103.15`. The feature graph retains explicit ring, with no aws-lc-rs or Brotli. This removes the four reported Rustls advisories. Primary references checked on 2026-10-14:

- [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285.html): encryption-level validation; authenticated transcript remains intact, not an authentication bypass.
- [RUSTSEC-2026-0346](https://github.com/RustSec/advisory-db/blob/main/crates/rustls/RUSTSEC-2026-0346.md): session-value Debug exposure with custom stores; no such workspace usage found.
- [RUSTSEC-2026-0348](https://github.com/RustSec/advisory-db/blob/main/crates/rustls/RUSTSEC-2026-0348.md): Brotli feature absent.
- [RUSTSEC-2026-0349](https://github.com/RustSec/advisory-db/blob/main/crates/rustls/RUSTSEC-2026-0349.md): hybrid key-share prediction; classic ring provider retained.

## Remaining limitations

- `cargo deny check` still fails [RUSTSEC-2024-0449](https://rustsec.org/advisories/RUSTSEC-2024-0449.html), an unmaintained `unsafe-libyaml` advisory via kube-client .99 / serde_yaml. No patched compatible version exists. [Kube 4](https://github.com/kube-rs/kube/releases/tag/4.0.0) replaces the parser but also requires k8s-openapi .28 and changes retries/timeouts. This major migration is outside the corrective pass; no new waiver was added and the existing bincode exception is unchanged. This maintenance advisory is not evidence of an exploit.
- Public snapshot schemas and the wire format remain unchanged. Kubernetes partial availability is recorded internally and in diagnostics; there is no new per-resource health field in remote UI snapshots.
- Verification uses temporary files, simulated Docker/Kubernetes HTTP services and GPU samples, local TLS sockets, duplex IO and controlled time on macOS. No real Docker/Kubernetes/GPU service or additional OS validation is claimed.
- The graph builder used its grep fallback. Cargo-mutants is unavailable; no mutation score is claimed. Semgrep's initial full Rust scan had eight rule timeouts under host contention; its existing native unsafe-use findings are not new defects. Gitleaks found no secrets in the tracked diff; its historical finding was a documentation placeholder.

## Verification results

- `cargo test --workspace --locked`: **998 passed, 0 failed, 2 ignored** (real Docker/Kubernetes environment tests), repeated successfully with `CARGO_TARGET_DIR=/tmp/muxtop-verify-debug` and `--jobs 2` after the last code change.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed, including test/example/benchmark targets; repeated successfully on the internal target directory after the last code change.
- `CARGO_TARGET_DIR=/tmp/muxtop-verify-target cargo bench --workspace --bench '*' --no-run --locked --jobs 2`: **all seven Criterion benchmarks compiled**, retaining full LTO. No benchmark measurements claimed.
- `cargo deny check`: advisories failed only on unsafe-libyaml; bans, licenses and sources passed.
- Final Rustls feature graph: 0.23.46, ring enabled, aws-lc-rs and Brotli absent.
- Gitleaks scanned the tracked patch and all new Rust files: no leaks found.
- Independent FABER review: **13/13 active criteria passed**, including a follow-up after preserving the public error enum and correcting a sort test fixture.

Behavioral regression evidence: restoring the original engine sources and compatible authored tests made all **8 engine regressions fail**. Reintroducing the original TUI methods and idle scheduling gap made **8 of 12 TUI audit tests fail** (the remaining shared-projection tests stayed fixed). Corrected sources were restored before the final green workspace run. These deliberate checks do not imply an automated mutation score.

The initial unfiltered optimized benchmark build on the external drive failed with `rustc-LLVM ERROR: IO failure on output stream: Input/output error` and a compiler SIGBUS. The successful retry used the internal disk, two build jobs and explicit Criterion benchmark targets. The initial aggregate command is not reported as passing; optimized unit-test harnesses were not part of the retry. Source code and release optimization settings were unchanged.

## Revalidation — 2026-10-19

Resumed the existing local patch at `b7ca5a4` on `perf/v0.8-optimisation-pass`.
The independent review found no confirmed defect requiring another Rust source
change. Added the missing Unreleased changelog entries and refreshed local
verification records. No commit, push, merge or release was performed.

Validation ran on macOS ARM64, Rust 1.95.0, with build artifacts on internal
storage and two build jobs:

| Check | Result |
|---|---|
| `cargo fmt --all -- --check`, `git diff --check` | Passed |
| `cargo test --workspace --locked --jobs 2` | 998 passed, 0 failed, 2 ignored |
| `cargo clippy --workspace --all-targets --locked --jobs 2 -- -D warnings` | Passed |
| `cargo +1.88 check --workspace --all-targets --locked --jobs 2` | Passed on the declared MSRV |
| `cargo doc --workspace --no-deps --document-private-items --locked --jobs 2` | Passed with `K8S_OPENAPI_ENABLED_VERSION=1.31`, `RUSTDOCFLAGS=-Dwarnings` |
| `cargo bench --workspace --bench '*' --no-run --locked --jobs 2` | All seven Criterion benchmarks compiled with full LTO; no performance measurements |
| `cargo deny check` | Failed only on RUSTSEC-2024-0449; bans, licenses and sources passed with existing warnings |
| Gitleaks on tracked diff and new Rust files | Zero findings |
| Semgrep `p/rust`, metrics disabled | 24 changed/new Rust files, 11 rules, zero findings or scan errors |
| Independent FABER review | 13/13 active criteria passed; no confirmed defects |

All 32 distinct regression tests referenced by the acceptance trace were found
passing in the new workspace test log: Docker (3), Kubernetes (5), TUI (12),
transport/lifecycle (11) and bounded logging (1). The dependency feature graph
still selects rustls 0.23.46 with ring, without aws-lc-rs or Brotli.

The pass remains **conditional on dependency maintenance**: the unmaintained
`unsafe-libyaml` parser is still pulled in by kube-client 0.99 through
`serde_yaml`. The earlier decision to handle the major kube/Kubernetes type
migration separately is retained. No new waiver was added, and `just check`
therefore still fails at its dependency gate.

The reviewer noted optional receiver-closure hardening while a remote connection
is being established or retried. It is not a confirmed regression: the production
CLI explicitly cancels the transport token on TUI exit. No change was made for
that observation. Cargo-mutants is still unavailable, so no new mutation score
is claimed. The real-service and cross-platform limitations above still apply.


## Kubernetes dependency migration — 2026-10-19

The user authorized the major dependency migration after the conditional
revalidation above. This section supersedes the earlier decision to defer it.

- Migrated kube/kube-client/kube-core from 0.99 to 4.0.0 and k8s-openapi from
  0.24 to 0.28. `serde_yaml` and `unsafe-libyaml` are absent from the resolved
  graph; the replacement is serde-saphyr 0.0.27. No advisory exception added.
- Constrained kube to `~4.0.0` to preserve Rust 1.88: kube 4.2 requires 1.89.
  Removed its unused runtime feature. The TLS graph retains rustls 0.23.46
  and ring without aws-lc-rs or Brotli.
- Selected the earliest supported bindings, Kubernetes 1.32, in every test
  and binary leaf. Updated docs, CI, semver and publishing environments to
  `K8S_OPENAPI_ENABLED_VERSION=1.32` together.
- Converted Kubernetes creation ages and Docker inspect start timestamps to
  Jiff. Four regression tests cover fractional seconds/time offsets,
  missing/pre-epoch/future timestamps, context/namespace overrides and
  immediate transient-failure reporting without hidden client retries.
- Disabled kube 4's default retries in both file-based and in-cluster
  configurations. The existing poll loops own retry cadence, while version,
  resource and metrics requests keep their explicit collection deadlines.
- Snapshot types and wire encoding are unchanged. The hidden public benchmark
  helper accepts k8s-openapi 0.28 Pods now; consumers of that helper see a Rust
  type-identity change, which belongs in the planned minor release. No
  workspace version bump or release is performed here.

Sources consulted: [kube 4.0 migration notes](https://github.com/kube-rs/kube/releases/tag/4.0.0),
[kube 4.2 MSRV](https://github.com/kube-rs/kube/releases/tag/4.2.0),
[Jiff timestamp API](https://docs.rs/jiff/0.2.38/jiff/struct.Timestamp.html#method.as_millisecond),
and the resolved crates' published Rust source.

| Check | Result |
|---|---|
| Kubernetes focused tests | 43 passed, 0 failed, 1 ignored |
| `cargo +1.88 check --workspace --all-targets --locked --jobs 2` | Passed |
| `cargo deny check` | All four gates passed, existing warnings only |
| `just check` | Passed: format, all-target Clippy, cargo-deny, 1,002 tests passed / 0 failed / 2 ignored |
| Strict Rustdoc with Kubernetes 1.32 environment | Passed |
| Seven Criterion benchmark builds with full LTO | All seven compiled; no performance measurements |
| Gitleaks tracked patch plus new Rust files | Zero findings |
| Semgrep p/rust | 24 changed/new Rust files, 11 rules, zero findings or errors |
| Independent FABER migration review | 13/13 criteria passed, no findings |

Local build artifacts use internal storage and two jobs on macOS ARM64.
No live Docker/Kubernetes/GPU service, other OS runtime, new performance
measurement or automated mutation score is claimed. Previously documented
performance figures predate this dependency migration. The existing bincode
advisory exception is unchanged. No commit, push, merge or release performed.
