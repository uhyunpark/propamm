# propamm-pinocchio (program)

Pinocchio 0.11 port of `programs/propamm`: same account bytes, PDAs, semantics, error codes and check order. Design and constraint mapping: `docs/design-onchain.md` (CU ladder).

- Program id keypair is outside the repo: `~/.config/solana/propamm-pinocchio-keypair.json`. Copy it to `target/deploy/propamm_pinocchio-keypair.json` (gitignored); the pubkey is in `declare_id!`.
- Build: `cargo build-sbf --manifest-path programs/propamm-pinocchio/Cargo.toml` -> `target/deploy/propamm_pinocchio.so`. Tests: `crates/propamm-tests/tests/pinocchio.rs` (differential against the Anchor program).
- Every Anchor constraint must have an explicit equivalent here (`src/checks.rs` names the Anchor check each helper replaces). A change to the Anchor program needs the same change here, or the parity tests fail.
- `no_std`, no allocator, no logs. Every `unsafe` block needs a `// SAFETY:` comment.
