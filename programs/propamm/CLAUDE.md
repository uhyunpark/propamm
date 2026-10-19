# propamm (program)

Anchor 1.2 program, SPL Token only. Account layout and pricing live in `crates/pricing`; design in `docs/design-onchain.md`.

- Build: `make build-sbf` (`anchor build --no-idl`; IDL gen fails on the nested external `MarketState`). Output `target/deploy/propamm.so`.
- Program id keypair is outside the repo: `~/.config/solana/propamm-program-keypair.json`. Copy it to `target/deploy/propamm-keypair.json` (gitignored) before building. Never commit keypairs. The pubkey is in `declare_id!` and `Anchor.toml`.
- Admin check is an explicit `constraint ... @ Unauthorized` (`has_one` cannot read nested bytes).
- `anchor-spl` needs the `token_2022` feature compiled in because `token::` constraints reference `token_interface`; we only use SPL Token.
- Crate denies `arithmetic_side_effects`, `unwrap_used`, `expect_used`. Use checked ops.
- Handlers reuse pricing-crate validation (`Ladder::validate_shape`); do not duplicate rules here.
- Feature `fast-update` (off by default): raw 1-byte-tag `update_price` via the Anchor `fallback` (`src/fast.rs`). Account order is `[updater, market]` (same as `UpdatePrice`) with the normal handler's error codes. The fast build is test-only (CU ladder, `target/deploy/fast/propamm.so`): never deploy it. `make build-sbf` copies `target/deploy/propamm-keypair.json` into `target/deploy/fast/` first so it keeps the same program id. The default build must stay free of it. See `docs/design-onchain.md`, CU ladder.
