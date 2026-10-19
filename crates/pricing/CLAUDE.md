# propamm-pricing

`no_std`, integer-only pricing core (price ladder) and the shared `MarketState` account layout.

- Units: price = quote atoms per base atom x `PRICE_SCALE` (1e12). See crate docs in `src/lib.rs`.
- Every division goes through `src/math.rs` (`mul_div_floor` / `mul_div_ceil`). Rounding favors the maker.
- Layout offsets are pinned by `tests/layout.rs`. Changing them breaks the program and the Pinocchio port.
- Lints deny arithmetic side effects, unwrap/expect/panic, and indexing: use `checked_*` ops.
- Design: `docs/design-onchain.md` (Pricing).
