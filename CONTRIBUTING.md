# Contributing

Small project, simple rules:

- **Read `docs/OVERVIEW.md` first**; every subsystem has a feature doc in
  `docs/features/` that states its scope, flow, and invariants. Changes ship
  with the matching doc updated.
- **Tests come with the change.** Everything is testable hermetically: process
  side effects go through the `Runner` trait (see `crates/rbs-setup`), remote
  transports through the `Transport` trait, S3 through injectable listings /
  the in-memory backend. No test may touch the network or another host.
- **Green means** `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace` — same as CI.
- Branch from `main` (`feat/…`, `fix/…`), one-line commit subjects, PRs
  describe what changed and why.
- Bug reports: `rbs doctor --remote` output plus `RBS_LOG=debug` around the
  failure is the fastest path to a fix.
