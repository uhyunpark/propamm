# propamm-tests

LiteSVM 0.17 integration tests for the program. Tests live in `tests/`, one file per area (`scenarios.rs` holds the scenario and attack tests: SOL/USDC with 9/6 decimals, P&L read from vault balances, `--nocapture` prints the summary rows); the shared `Env` and helpers are in `tests/common/mod.rs`. `src/lib.rs` is empty.

- Needs `target/deploy/propamm.so`, `target/deploy/propamm_pinocchio.so` and `target/deploy/fast/propamm.so` (the `fast-update` build): run `make build-sbf` first (tests fail with a clear message if one is missing).
- Instructions are built from the program's generated `propamm::instruction` / `propamm::accounts` types (`cpi` feature).
- Token setup uses `litesvm-token`. Errors are asserted by code: `PropAmmError` via `u32::from`, SPL/System errors by raw custom code.
- CU table: `cargo test -p propamm-tests compute_units -- --nocapture` (prints; asserts the update_price/force_price budgets; swap CU: `--test swap compute_units`).
- `warp_to_slot` only sets the slot; set `unix_timestamp` through sysvars if needed (docs/toolchain.md).
