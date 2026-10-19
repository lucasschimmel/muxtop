# Contributing to muxtop

Thank you for your interest in contributing! This document covers everything you need to get started.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Prerequisites](#prerequisites)
- [Development Setup](#development-setup)
- [Project Architecture](#project-architecture)
- [Making Changes](#making-changes)
- [Code Standards](#code-standards)
- [Testing](#testing)
- [Submitting a Pull Request](#submitting-a-pull-request)
- [License](#license)

---

## Code of Conduct

Be respectful and constructive: argue about code, not about each other. The full
text — including how to report a problem — is in
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

**Security issues do not go through the normal flow.** If you have found a
vulnerability, [report it
privately](https://github.com/lucasschimmel/muxtop/security/advisories/new)
rather than opening an issue or a pull request — a public report is a disclosure.
See [SECURITY.md](SECURITY.md).

---

## Prerequisites

| Tool | Version | Purpose |
|------|---------|---------|
| Rust | stable (≥ 1.88) | Compiler and toolchain |
| `just` | latest | Task runner |
| `bacon` | latest | Optional — continuous check during dev |
| `cargo-deny` | latest | Dependency audit (required for CI) |

Install the Rust toolchain via [rustup](https://rustup.rs/). The others can be installed with:

```sh
cargo install just bacon cargo-deny
```

---

## Development Setup

```sh
git clone https://github.com/lucasschimmel/muxtop.git
cd muxtop
cargo build --workspace
```

Run `just` (no arguments) to see all available recipes:

```sh
just
```

Key recipes:

| Recipe | Description |
|--------|-------------|
| `just build` | Debug build |
| `just run` | Run muxtop locally |
| `just dev` | Continuous clippy via bacon |
| `just check` | Full CI check (fmt + clippy + deny + test) |
| `just test` | Run the test suite |
| `just bench` | Run criterion benchmarks |
| `just fmt` | Auto-format code |

---

## Project Architecture

muxtop is a Cargo workspace with four crates:

```
muxtop-core     — Data collection engine (CPU, memory, processes, network, disk)
muxtop-tui      — Terminal UI built with ratatui (tabs, panels, fuzzy search, keybindings)
muxtop-proto    — Wire protocol and binary serialization for remote monitoring
muxtop-server   — TCP daemon exposing muxtop-core data over the network
```

The binary entrypoint lives in `src/main.rs` at the workspace root and wires up the crates. When in doubt about where to place new code, prefer keeping data collection in `muxtop-core` and rendering concerns in `muxtop-tui`.

---

## Making Changes

### Branch model

- **`develop`** — active development, target for all PRs
- **`main`** — stable releases only, never commit directly

Always branch off `develop`:

```sh
git switch develop
git pull
git switch -c feat/your-feature
```

### Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(tui): add disk I/O panel
fix(core): handle zombie processes in process tree
perf(proto): reduce allocation in snapshot serialization
docs: update keybindings in README
test(core): add edge cases for network collector
```

Common scopes: `core`, `tui`, `proto`, `server`, `cli`, `ci`, `docs`.

---

## Code Standards

### Formatting

Enforced via `rustfmt` with the config in `rustfmt.toml` (edition 2024, max line width 100). Run before committing:

```sh
just fmt
```

### Lints

All clippy warnings are treated as errors (`-D warnings`). Run:

```sh
just clippy
```

Address every warning — do not use `#[allow(...)]` without a comment explaining why it is necessary.

### MSRV

The minimum supported Rust version is **1.88**. Do not use features stabilized after that version without updating `Cargo.toml` and `clippy.toml` accordingly.

### Dependencies

New dependencies must pass `cargo deny check`. Prefer crates already in `[workspace.dependencies]`. Avoid adding heavy dependencies for small utilities.

---

## Testing

```sh
just test        # unit + integration tests
just bench       # criterion benchmarks (compile + run)
```

- Place unit tests in `#[cfg(test)]` modules within the source file.
- Place integration tests under `tests/` at the workspace root or within each crate's `tests/` directory.
- Benchmarks live under each crate's `benches/` directory using criterion.

CI runs tests on **Ubuntu** and **macOS**. If your change is platform-specific, note it in the PR.

### Workspace-wide checks and the `k8s-openapi` feature gate

`k8s-openapi` (transitive via `kube`) refuses to compile a library crate that
doesn't enable a `v1_*` feature, but it equally refuses to let library crates
enable one in their `[dependencies]`. The workaround is simple but **the CI
command must include `--all-targets`** so dev-deps activate:

```sh
cargo check --workspace --all-targets   # ✅ green — dev-deps enable v1_32
cargo check --workspace                 # ❌ red on the muxtop-core lib build
```

`just check` already wraps the correct invocation.

### Testing against Docker / Podman

The Docker integration test in `muxtop-core/src/docker_engine.rs` is gated by
`#[ignore]`. It needs a running Docker or Podman daemon. Run with:

```sh
cargo test -p muxtop-core --lib docker_engine -- --ignored
```

### Testing against Kubernetes

The Kubernetes integration test in `muxtop-core/src/kube_engine.rs::tests::
integration_connect_and_snapshot` is also gated by `#[ignore]`. It connects
to whatever cluster `$KUBECONFIG` / `~/.kube/config` resolves to, waits up
to 10 s for the resource-poll loop to publish the first snapshot, then
asserts at least one node was returned.

The shortest path is `kind`:

```sh
# 1. Spin up a local cluster (one-time install: brew install kind / scoop install kind)
kind create cluster

# 2. Run the ignored kube tests
cargo test -p muxtop-core --lib kube_engine -- --ignored

# 3. Tear it down
kind delete cluster
```

Other targets work too — `k3d cluster create`, an `~/.kube/config` pointed
at EKS / GKE / AKS, or any reachable kubeconfig context. The test reads
the **active** context, so set it before running:

```sh
kubectl config use-context <name>
cargo test -p muxtop-core --lib kube_engine -- --ignored
```

There are no live Kubernetes calls in the default test suite — the unit
tests use `serde_json::from_value` to construct typed `Pod` / `Node` /
`Deployment` objects directly, so they run on hosts with no cluster.

---

## Submitting a Pull Request

1. Ensure `just check` passes locally — this runs fmt, clippy, deny, and tests.
2. Keep the scope of a PR focused. One feature or fix per PR.
3. Update `CHANGELOG.md` under the `[Unreleased]` section following the existing format.
4. Update the documentation that your change would otherwise make wrong — the
   README, the `--help` text, and [`docs/wiki/`](docs/wiki) for anything a user
   acts on. **The wiki is generated from `docs/wiki/`**: editing a page in the
   browser works until the next release overwrites it.
5. Open the PR against the `develop` branch.
6. Fill out the PR description: what changed, why, and how to test it. The
   template's checklist has extra sections for changes that touch a published
   crate's API, security-relevant code, or a platform boundary.

CI will run automatically. PRs cannot be merged until all checks pass.

### What CI runs

Beyond fmt, clippy, `cargo deny` and tests on Ubuntu, macOS and Windows:

| Job | Checks |
|---|---|
| `MSRV` | The workspace still compiles on the declared minimum, Rust 1.88 |
| `Rustdoc` | `cargo doc` with `-Dwarnings` — catches broken intra-doc links |
| `Coverage` | `cargo-llvm-cov`; the summary is in the run, the lcov file is an artifact |
| `Semver` | `cargo-semver-checks` against the published `muxtop-core`, `muxtop-proto` and `muxtop-tui` |

A separate scheduled workflow re-audits `Cargo.lock` against the RUSTSEC
database daily and files an issue when a new advisory lands, since nothing else
would trigger a build for an advisory published against code that already
shipped.

---

## License

By contributing to muxtop, you agree that your contributions will be dual-licensed under the terms of the **MIT** and **Apache 2.0** licenses, consistent with the rest of the project. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
