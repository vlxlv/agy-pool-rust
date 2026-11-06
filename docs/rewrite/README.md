# Rewrite plans

The design and phase plans used to port agy-pool from Go to Rust, kept as a record. They were written while the port lived in agy-pool-go's `rust/` directory, so paths such as `rust/src/...`, `scripts/rust-test.sh` and `internal/...` refer to that layout; the Rust paths are now relative to this repository's root and the sandbox script is `scripts/test.sh`.

- [`2026-11-05-rust-rewrite.md`](2026-11-05-rust-rewrite.md): overall design and phases
- [`2026-11-06-rust-rewrite-phase2.md`](2026-11-06-rust-rewrite-phase2.md) to [`phase5`](2026-11-06-rust-rewrite-phase5.md): per-phase task plans (phase 5 is the candidate soak and cutover)
