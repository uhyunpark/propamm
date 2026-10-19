# Toolchain

## Host
- rustc 1.97.1, pinned via `rust-toolchain.toml`.
- `cargo build-sbf` uses its own platform-tools toolchain.

## Pins
| Dependency | Pin | Note |
|------------|-----|------|
| anchor-lang / anchor-spl | `=1.2.x` | 2.0.0-rc.1 is a different framework. Never use `*` |
| litesvm | 0.17 | |
| pinocchio | 0.11.2 | AccountView/Address API |
| pinocchio-token | 0.7.0 | |
| pinocchio-system | 0.6.1 | CPI helpers for the Pinocchio port |
| wasm-bindgen | `=0.2.129` (crate) | Must equal the installed CLI: `cargo install wasm-bindgen-cli --version 0.2.129 --locked`, plus `rustup target add wasm32-unknown-unknown`. The CLI refuses a mismatched `.wasm` |
| Agave | explicit tag at install | Latest v4.3.0 on 2026-09-18. Anchor 1.2 CI uses 4.1.2. Record the tag in Anchor.toml `[toolchain]` |

## Install (aarch64-apple-darwin)
- Agave 4.1.2: `sh -c "$(curl -sSfL https://release.anza.xyz/v4.1.2/install)"`. Gives solana-cli 4.1.2, solana-test-validator 4.1.2, cargo-build-sbf 4.1.0.
- PATH: `export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"`. The installer appended this line to `~/.profile` and `~/.zprofile`. Nothing else was edited.
- Anchor: `cargo install --git https://github.com/otter-sec/anchor avm --force --locked` (--locked worked, avm 1.2.0, ~2 min), then `avm install 1.2.0 && avm use 1.2.0`. `anchor --version` gives anchor-cli 1.2.0.
- Platform-tools: `anchor build` fetched v1.57 into `~/.cache/solana/v1.57/platform-tools` (about 1.4 GB, rustc 1.95.0-dev). `cargo build-sbf --version` reports default v1.54, but Anchor picks the newest. Check with `cargo build-sbf --version` and the cache dir if a build behaves oddly.
- v1.57 (`anchor build`, `make build-sbf`) vs v1.54 (plain `cargo build-sbf` default): v1.54 output uses 3-8% more CU (Pinocchio update_price 969 vs 1,007; Anchor 1,992 vs 2,124). The CU budgets pass under both. `make build-sbf` pins `--tools-version v1.57` for the fast-path build so all test `.so` files match.
- Build output: `target/deploy/<name>.so`.
- `anchor init <name> --no-git --no-install --test-template litesvm` needs no node/yarn. Generated `Anchor.toml` has an empty `[toolchain]` (add the Agave tag there manually). The generated workspace sets `[profile.release] overflow-checks = true, lto = "fat", codegen-units = 1` and a `rust-toolchain.toml` pinning host 1.89.0.
- rust-toolchain.toml interaction: no problem. With the project's host pin (1.97.1) in the workspace root, both `cargo build-sbf` (in the program dir) and `anchor build` produce the `.so`. build-sbf uses its own linked rustup toolchain (`1.95.0-sbpf-solana-v1.57`) and ignores the host pin. The pinned host toolchain is used only for the host (test) build.

## Verified compatibility (cargo check, 2026-10-04)
- Pubkey types unify across anchor-lang 1.2 (solana-pubkey 3 -> solana-address 2.x), litesvm 0.17, pinocchio 0.11, jupiter-amm-interface. No conversion.
- AccountMeta/Instruction unify too.

## Known issues
- (a) jupiter-amm-interface 0.6.1 breaks on fresh resolve (wildcard deps pull solana-account-decoder 4.x).
  - Fix: `cargo update -p solana-account-decoder --precise 3.1.14` and `-p solana-account --precise 3.4.0`.
  - 1.0.0-beta.0 resolves clean.
  - Git HEAD (95bd184) has a newer API (AccountProvider, AmmError) and uses solana-account 4, which unifies with litesvm.
- (b) solana-account 3 vs 4 type split between published Jupiter crates and litesvm 0.17. Use a 5-field converter, in one place.
- (c) jupiter-amm-test-kit pins litesvm 0.14. Use a separate workspace/lockfile, or our own parity test (primary).
- (d) Commit `Cargo.lock`. No blanket `cargo update` (wincode/sysvar drift breaks litesvm).
- (e) LiteSVM `warp_to_slot` only sets Clock.slot. Set `unix_timestamp` via get_sysvar/set_sysvar. CU comes from `meta.compute_units_consumed`.
- (f) zero_copy alignment: 8-byte header, no u128 fields.
- (g) Anchor release profile must set `overflow-checks = true`, or `anchor build` fails.

## Lint friction with Anchor 1.2 (verified)
- Add `unexpected_cfgs` check-cfg for `target_os = "solana"`.
- In the program crate, allow `clippy::wildcard_imports`, `clippy::needless_pass_by_value`, `clippy::missing_errors_doc`.
- Do not enable `missing_docs` there (macro output trips it).
- Declare program features: `custom-heap`, `custom-panic`, `anchor-debug`, `no-log-ix-name`, `idl-build = ["anchor-lang/idl-build"]`, `cpi`, `no-entrypoint`.
- No macro-origin hits for `arithmetic_side_effects`, `unwrap_used`, `result_large_err`.

## WASM
- `make wasm`: `cargo build -p propamm-wasm --release --target wasm32-unknown-unknown`, then `wasm-bindgen --target web --out-dir web/src/wasm` (output is gitignored). `make wasm-test` runs `crates/wasm/tests/smoke.ts` in bun against the generated module.
- `propamm-client` compiles for wasm32 as is (solana-pubkey/instruction need no getrandom backend). The module is about 60 KB.
- bun 1.3.14 runs the smoke test; `init({ module_or_path: bytes })` is the non-deprecated call form.

## Dev tools (installed 2026-10-04)
- cargo-mutants 27.1.0: `make mutants`. Baseline on pricing: 65 mutants, 58 caught, 0 missed, 4 unviable, 3 timeouts (infinite loops = detected).
- cargo-deny 0.20.2: `deny.toml`, `make deny`.

## cargo-deny notes
Expect unmaintained warnings to ignore:

| Crate | Advisory |
|-------|----------|
| ansi_term 0.12.1 | RUSTSEC-2021-0139 |
| bincode 1.3.3 | RUSTSEC-2025-0141 |
| derivative 2.2.0 | RUSTSEC-2024-0388 |
| libsecp256k1 0.7.2 | RUSTSEC-2025-0161 |
| paste 1.0.15 | RUSTSEC-2024-0436 |

- Check the arrayref advisory against the locked version.

## Anchor fallback fast path
- `pub fn fallback(program_id, accounts, data)` inside `#[program]` compiles.
- It skips per-ix `try_accounts` and Borsh. It still pays entrypoint account deserialization.
- Use custom 1-byte discriminators to avoid collisions.
- Not in the IDL.

## Build findings
- `anchor build` IDL step fails for `#[account(zero_copy)]` wrapping an external Pod type (`MarketState` lacks `IdlBuild`; orphan rules block an impl). `make build-sbf` uses `anchor build --no-idl`.
- anchor-spl needs `token_2022` compiled in (with `token`) for `token::mint/authority` constraints; the program still only talks to SPL Token.
- zero_copy needs a direct `bytemuck` dep (with `derive`, `min_const_generics`) in the program crate.
- Constraints run after all `init`s, so a constraint cannot pre-empt an init failure.
- cargo-deny: `BSD-2-Clause` (arrayref) added to the allow list.
- Test deps pinned to the versions already in the lockfile: solana-keypair 3.1.2, solana-signer 3.0.1, solana-message 4.6.0, solana-transaction 4.3.0, solana-transaction-error 3.4.0, solana-instruction-error 2.5.0, litesvm-token 0.17.0.

## Public devnet RPC
- `api.devnet.solana.com` rate-limits per IP (HTTP 429 under bursty traffic; the first `setup` attempt failed on it). The bot's JSON-RPC client retries 429, 500/502/503/504 and transport timeouts up to 6 attempts with backoff 250 ms doubling to 8 s, jitter of 50 to 100%, and honors `Retry-After` (seconds form, capped at 8 s). `sendTransaction` retries resend the same signed bytes (same signature). `requestAirdrop` is never retried. The summary on exit prints the 429 and retry counts.
- `run --status-poll-ms N --refresh-ms N` set the signature-status and market-refresh intervals. Defaults: 200 ms and 2000 ms on localhost, 1000 ms and 3000 ms on any other RPC host.
- A 6-minute run (about 170 sends, 1 status poll per second while updates are pending) saw no 429.
- `setup --cluster devnet --base-mint M --quote-mint Q` resumes an earlier attempt: existing mints, accounts, market and deposits are kept, inventory is topped up only when below target, and `force_price` runs only when the mid is 0. Devnet inventory targets: admin 200 SOL-token and 40,000 USDC-token, trader 20 and 4,000 (an earlier attempt had already minted and deposited the larger local amounts, which stay).
