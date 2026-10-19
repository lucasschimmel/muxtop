# muxtop — Justfile
# Run `just --list` to see all available recipes.

# Show all recipes
default:
    @just --list

# Build the workspace (debug)
build:
    cargo build --workspace

# Build release binary (static musl for Linux)
build-release target="x86_64-unknown-linux-musl":
    cross build --release --target {{target}}

# Run muxtop in debug mode
run *ARGS:
    cargo run -- {{ARGS}}

# Full CI check (fmt + clippy + deny + test)
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo deny check
    cargo test --workspace --locked

# Run tests
test:
    cargo test --workspace --locked

# Format code
fmt:
    cargo fmt --all

# Run clippy lints
clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Dev mode with bacon (continuous check)
dev:
    bacon clippy

# Run criterion micro-benchmarks
bench:
    cargo bench --workspace --locked

# Run the Thomas macro-benchmark (build, startup, RSS)
bench-thomas:
    ./scripts/bench-thomas.sh

# Run the dhat heap allocation profile on hot paths
bench-alloc:
    cargo run --release --example alloc_profile -p muxtop-tui

# Prepare a release (requires cargo-release)
release version:
    cargo release {{version}}

# Native PTY lifecycle (Linux/macOS; Python standard library only)
test-exec-pty:
    cargo test -p muxtop-tui --lib --no-run --locked --message-format=json | python3 scripts/test-exec-pty.py --cargo-json
