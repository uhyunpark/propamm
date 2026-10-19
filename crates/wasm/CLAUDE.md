# propamm-wasm

- wasm-bindgen bindings for the demo web app. Interface: `docs/demo-contract.md`. Build: `make wasm` (output `web/src/wasm/`, gitignored), smoke test: `make wasm-test`.
- `wasm-bindgen` is pinned `=0.2.129` and must equal the CLI version (`docs/toolchain.md`).
- Each export is a thin wrapper over a plain Rust fn (`market_view`, `quote`, `ladder_levels`, `swap_ix_data`, `price_ui`) that returns `Result<_, &'static str>`; tests call those natively, never the `#[wasm_bindgen]` fns.
- Structs go through `serde-wasm-bindgen` with u64 as bigint and camelCase names; TypeScript types come from the `typescript_custom_section` in `lib.rs`.
- `tests/fixtures/{market.hex,golden.json}` are shared by the Rust tests and `tests/smoke.ts`. Regenerate: `cargo test -p propamm-wasm regenerate -- --ignored`.
- Logic comes from `propamm-pricing` and `propamm-client`; do not reimplement quoting here.
