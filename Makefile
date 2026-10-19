# Run `make check` before reporting any change; only failures matter.
.PHONY: check fmt fmt-check lint build-sbf test mutants deny wasm wasm-test demo-e2e

check: fmt-check lint build-sbf test

fmt:
	@cargo fmt --all

fmt-check:
	@cargo fmt --all -- --check

lint:
	@cargo clippy --workspace --all-targets --quiet -- -D warnings
	@cargo clippy -p propamm --features fast-update --quiet -- -D warnings

# Needs the Solana toolchain on PATH (see docs/toolchain.md). Builds the Anchor program and the
# Pinocchio port (anchor build), then the test-only fast-update build of the Anchor program into
# target/deploy/fast. Tests load target/deploy/{propamm,propamm_pinocchio}.so and fast/propamm.so.
# The fast build reuses the Anchor program keypair so it never generates a different program id.
# --no-idl: Anchor's IDL build cannot see through the nested `MarketState` type from propamm-pricing.
build-sbf:
	@out=$$(anchor build --no-idl 2>&1) || { echo "$$out"; exit 1; }
	@mkdir -p target/deploy/fast
	@cp target/deploy/propamm-keypair.json target/deploy/fast/propamm-keypair.json
	@out=$$(cargo build-sbf --tools-version v1.57 --manifest-path programs/propamm/Cargo.toml --features fast-update --sbf-out-dir target/deploy/fast 2>&1) || { echo "$$out"; exit 1; }

test:
	@cargo test --workspace --quiet

# Mutation testing for the pricing crate (slow; run when pricing logic changes).
mutants:
	@PROPTEST_CASES=64 cargo mutants -p propamm-pricing --timeout 20 --no-shuffle

# Supply-chain audit (advisories, licenses, bans, sources). Needs cargo-deny; not part of `check`.
deny:
	@cargo deny check

# WASM bindings for the demo web app (docs/demo-contract.md). Needs the wasm32-unknown-unknown
# target and a `wasm-bindgen` CLI that matches the pinned crate version (docs/toolchain.md).
wasm:
	@cargo build -p propamm-wasm --release --target wasm32-unknown-unknown --quiet
	@wasm-bindgen --target web --out-dir web/src/wasm target/wasm32-unknown-unknown/release/propamm_wasm.wasm

# Loads the generated module in bun and compares it with the Rust fixtures.
wasm-test: wasm
	@bun crates/wasm/tests/smoke.ts

# Browser E2E of the demo with headless system Chrome; writes target/e2e/*.png. Needs the stack up
# (README.md): scripts/localnet.sh, the bot with --serve 127.0.0.1:8788 --faucet, `bun run dev`.
demo-e2e:
	@cd web && bun e2e/demo.ts
