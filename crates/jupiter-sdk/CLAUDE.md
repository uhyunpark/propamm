# propamm-jupiter

Jupiter `Amm` adapter (`PropAmm`) over `jupiter-amm-interface`, pinned to git rev `95bd184` (its solana-account 4 unifies with litesvm 0.17).

- Not submission-ready: which interface version Jupiter integrates is unconfirmed, and a real listing needs a Jupiter-assigned `Swap` variant (we emit `Swap::Placeholder` with the side byte).
- Quote = pricing crate `Ladder::quote_exact_in` on a snapshot. `fee_amount` is 0: the spread is inside the price.
- Discriminators are hardcoded constants; `tests/parity.rs` checks them against sha256 and the program.
- `tests/parity.rs` replaces `jupiter-amm-test-kit` (pins litesvm 0.14). It needs `make build-sbf` first.
- Lint: `unwrap_used` / `expect_used` denied in `src/`, allowed in tests.
