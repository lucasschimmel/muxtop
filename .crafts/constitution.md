---
criticity: production
updated: 2026-10-19
design: n/a
---
# Muxtop constitution

Derived from README, CONTRIBUTING, existing engine and terminal code.

- No telemetry. Network calls only implement explicitly enabled features.
- Unknown metrics stay optional and display as `—`.
- GPU and Kubernetes monitoring remain read-only.
- Container mutations are local-only and operate on the selected full daemon ID.
- Preserve public snapshot layouts and the bincode wire contract.
- Maintain Rust 1.88, edition 2024, and all three CI build targets.
- Reuse the detected daemon and existing UI keymap, notifications and themes.
- English source/docs/commits, Conventional Commits; branches from develop, PRs to develop.
