# On-chain Design

## Market account
- Market PDA seeds: `["market", admin, base_mint, quote_mint]`. Including the admin stops anyone from squatting the per-pair address. Bump stored in `state.bump`.
- Vault PDAs: `["vault", market, mint]`, SPL token accounts with `authority = market`. Addresses stored in `state.base_vault` / `state.quote_vault`.
- `#[account(zero_copy)] #[repr(C)] struct Market { state: MarketState }`, space = `8 + MARKET_STATE_SIZE` (8 + 264 = 272 bytes). `MarketState` lives in the pricing crate so the Pinocchio port reuses it.
- Fields (see `crates/pricing/src/state.rs`):
  - `admin`, `updater`
  - `base_mint`, `quote_mint`, `base_vault`, `quote_vault` (raw `[u8; 32]`)
  - `mid_price`, `last_update_slot`, `max_age_slots`
  - `ask_consumed_base_atoms`, `bid_consumed_base_atoms`
  - `level_size_base_atoms`, `num_levels`, `half_spread_bps`, `level_step_bps`, `max_move_bps`
  - `paused`, `bump`, explicit padding
  - `window_start_mid` (u64) at offset 256, appended after the padding. Existing offsets stay pinned.
- Layout rules:
  - 8-byte discriminator header.
  - `#[repr(C)]` + bytemuck Pod.
  - No u128 fields (16-byte alignment trap). Use u128 only as intermediates.
  - A `size_of` assert test.
- A new market has `mid_price = 0`, so it is not tradable until the first admin `force_price`.

## Roles
| Role | Key | Allowed |
|------|-----|---------|
| admin | cold | initialize, set_params, deposit, withdraw, set_paused, set_updater, force_price |
| updater | hot, on bot server | update_price only (no other power; the admin cannot post via update_price) |

- A stolen hot key alone cannot withdraw. It can post bad prices, hence the guards below.
- Admin check: `constraint = market.load()?.state.admin == admin.key().to_bytes() @ Unauthorized` on every admin instruction. `has_one` does not work because the key sits in nested raw bytes. The admin must also sign.
- `initialize`: the payer becomes admin. The updater must not be the default key or the admin (`InvalidUpdater`); `set_updater` applies the same rule.

## Instructions
| Instruction | Args | Who |
|-------------|------|-----|
| initialize | `InitParams { updater, level_size_base_atoms, num_levels, half_spread_bps, level_step_bps, max_age_slots, max_move_bps }` | admin (payer) |
| deposit | `amount` | admin |
| withdraw | `amount` | admin |
| set_params | `MarketParams` (same fields minus `updater`) | admin |
| set_updater | `new_updater` | admin |
| set_paused | `paused: bool` | admin |
| update_price | `mid` | updater |
| swap | `side: SwapSide { BuyBase, SellBase }`, `amount_in`, `min_out` | anyone |
| force_price | `mid` | admin |

- Deposit/withdraw: the mint must be the market's base or quote mint (`InvalidMint`), the vault must be the stored vault for that mint (`InvalidVault`), the admin token account must be owned by the admin with that mint. Both use `transfer_checked`; withdraw signs with the market PDA seeds rebuilt from `state.admin` + stored mints + bump, matching init exactly.
- Parameter validation: `Ladder::validate_shape` from the pricing crate (everything except what needs a real mid), which also requires the per-side depth to fit in u64, plus `0 < max_age_slots <= MAX_AGE_SLOTS` (1_000, about 4 min at 250 ms, so staleness cannot be disabled) and `0 < max_move_bps <= MAX_MOVE_BPS` (100 = 1% per slot; larger legitimate moves go through admin `force_price`). `set_params` also sets both `consumed` counters to `u64::MAX`: the old counters are meaningless under new params, and the pricing walk then returns `InsufficientDepth` until the next price update resets them.
- `base_mint != quote_mint`: equal mints already fail at vault init (the vault PDAs collide). `initialize` also has an explicit `require_keys_neq!` (`InvalidMint`) as a guard against future seed changes.
- Errors: `PropAmmError` (`Unauthorized`, `InvalidParams`, `InvalidMint`, `InvalidVault`, `MathOverflow`, `InvalidUpdater`, `NotInitializedPrice`, `PriceMoveTooLarge`, `Paused`, `SlotRegression`, `AlreadyUpdatedThisSlot`, `StalePrice`, `SlippageExceeded`, `InsufficientVaultBalance`, plus mapped pricing errors).

- `update_price(mid)`: signer must be the stored `updater` (`Unauthorized`; the admin is rejected). Order of checks: `Paused`, `NotInitializedPrice` (`mid_price == 0`), `roll_window` (`SlotRegression` if `clock.slot < last_update_slot`, `AlreadyUpdatedThisSlot` if `clock.slot == last_update_slot`), `PriceMoveTooLarge`, then `Ladder::validate()` with the new mid. Pause blocks it (chosen over letting it through: a paused market should not move).
- Move limit: `new_mid` within `window_start_mid +- floor(window_start_mid * max_move_bps / BPS)`, inclusive (`move_band` / `within_move_limit` in the pricing crate; floor makes the band narrower).
- One update per slot. Window transition (`MarketState::roll_window`): every successful `update_price` is in a later slot than the last update, so it resets both `consumed` to 0 and sets `window_start_mid` to the mid current before this update. A second update in the same slot (including the slot of a `force_price`) fails with `AlreadyUpdatedThisSlot`, so there is no ratcheting and no repeated depth refresh within a slot.
- `force_price(mid)`: admin only, allowed while paused, no move limit. Runs `Ladder::validate()`, then `MarketState::reset_window`: mid, `window_start_mid`, `last_update_slot = clock.slot`, both `consumed` = 0. First-price and unfreeze path.
- `set_params` sets `consumed = u64::MAX`. The invalidation is cleared by the next `update_price` (necessarily in a later slot than the last update) or by `force_price`.
- New errors: `NotInitializedPrice`, `PriceMoveTooLarge`, `Paused`, `SlotRegression`, `AlreadyUpdatedThisSlot`.
- `Ladder::validate` also requires the deepest ask price (`ask_price(num_levels - 1)`) to fit in u64, so every ask price is representable. This does not rule out `MathOverflow` in a buy: clearing one huge level can cost more than u64. `walk_buy` compares that cost in u128 and converts only the amount actually charged, so a small buy on such a ladder succeeds.

### swap
- `swap(side, amount_in, min_out)`, ExactIn only, any signer. `BuyBase` = pay quote, receive base; `SellBase` = pay base, receive quote.
- Accounts with fixed roles: `trader` (signer), `market` (mut), `base_mint` / `quote_mint` (`address =` stored mints, `InvalidMint`), `base_vault` / `quote_vault` (mut, `address =` stored vaults, `InvalidVault`), `trader_base` / `trader_quote` (mut, `token::mint` = the fixed mint, `token::authority = trader`), `token_program` (SPL Token only).
- Order of checks: `Paused`, `NotInitializedPrice`, `StalePrice` (`is_fresh`), `quote_exact_in(side, state.consumed(side), amount_in)` (`ZeroInput`, `ZeroOutput`, `InsufficientDepth`, `MathOverflow`), `SlippageExceeded` (`amount_out < min_out`), `InsufficientVaultBalance` (out vault holds less than `amount_out`).
- State write: only `set_consumed(side, fill.consumed_after)`. No window roll; `mid_price`, `last_update_slot`, `window_start_mid` and the other side's counter are untouched.
- CPIs (`transfer_checked`, after the market borrow is dropped): the trader pays the in-vault, then the market PDA signs the out-vault payout. Seeds come from `Market::signer_seeds` (shared with `withdraw`).
- Event: `SwapEvent { market, trader, side, amount_in, amount_out, consumed_after, mid_price, slot }`. Events go through program logs, which a crafted transaction can truncate (10 KB limit). Indexers that need completeness should use `emit_cpi!` (not adopted in v1).
- Tests: `crates/propamm-tests/tests/swap.rs`.

### CU
The authoritative, current numbers are the table in "CU ladder" below. Earlier Anchor measurements (update_price 1,990, swap BuyBase 14,945, worst case 31,154) have not moved by more than a few CU since.

- Reproduce: `cargo test -p propamm-tests compute_units -- --nocapture`.
- `compute_units_per_instruction` asserts budgets of 2,150 (force_price) and 2,400 (update_price), about 20% headroom. `swap` (in `swap.rs::compute_units`) asserts 17,700 (BuyBase) and 17,100 (SellBase); `compute_units_worst_case` (32 levels, amount walks all of them) asserts 37,400 and 31,900. The Pinocchio port is compared against these.
- deposit/withdraw are dominated by the SPL Token `transfer_checked` CPI. initialize pays for three account creations.

## Pricing
Model: a price ladder ("ticks with fixed liquidity"). Per side, `num_levels` (<= 32) levels hold `level_size_base_atoms` each. Prices derive from mid on the fly:
- ask level i (trader buys base): `ceil(mid * (BPS + half_spread + i*step) / BPS)`
- bid level i (trader sells base): `floor(mid * (BPS - half_spread - i*step) / BPS)`

Units:
- Price = quote atoms per base atom x `PRICE_SCALE` (1e12). SOL/USDC at $200 = `200_000_000_000`.
- Range up to ~1.8e7 quote atoms per base atom. Low-priced many-decimal tokens lose digits.
- `BPS_DENOM` = 10,000. Integer math only; `u128` for intermediates, never in structs.

Rules:
- `consumed` (base atoms) is tracked per side. The window is a slot: it resets only on the first price update in a new slot (see Security decisions).
- No cross-side healing: buys never change bid consumption and vice versa.
- Depth cap per side per window = `num_levels * level_size_base_atoms`. Beyond it: `InsufficientDepth`. No partial fills; ExactIn consumes all input.
- Only `mul_div_floor` / `mul_div_ceil` divide.

| Operation | Rounding |
|---|---|
| ask level price | ceil |
| bid level price | floor |
| quote cost to clear the rest of an ask level | ceil |
| base received for a partial ask level | floor (leftover quote stays with maker) |
| quote received per bid level | floor |
| `amount_out == 0` | `ZeroOutput` error |

- Per-level rounding means output is monotonic in consumed only up to one rounding unit per level boundary crossed (1 output atom on sells; about 1 quote atom worth of base on buys).

## Anti order-splitting
Two layers:
1. Within a slot window: cumulative `consumed`, reset by the (single) price update of each slot. Verified on Brady's prop-amm: splitting never beats a single trade.
2. Across windows: the bot folds inventory skew into mid.

- Risk seen in Brady: segment size changes when total quantity changes via opposite-side flow.

## Guards
- Staleness: `slot - last_update_slot > max_age_slots` rejects. HumidiFi reportedly reverts with `0xfaded` [secondary].
- Max move per slot window, measured against `window_start_mid`, with an unfreeze path (admin `force_price`).
- Depth cap per slot window = max loss per window.
- Pause: blocks swap and update_price; never blocks deposit, withdraw or admin instructions.

## Security decisions
1. **Window = slot, one update per slot.** `update_price` is rejected when `clock.slot == last_update_slot` (`AlreadyUpdatedThisSlot`), so every successful update starts a new window. Rationale: with several updates per slot, a stolen updater key could loop [update, full-depth swap] many times in one transaction.
2. **Move limit against the window start.** The limit is measured against the mid at the start of the current slot window, stored in the new field `window_start_mid`. MarketState grew from 256 to 264 bytes by appending a u64 after the padding; existing offsets stay pinned.
3. **First price via `force_price` only.** A market starts with `mid == 0`; only admin `force_price` sets the first price, and `update_price` rejects while `mid == 0`. `force_price` also unfreezes the market after a large legitimate move.
4. **`update_price(mid)` takes only the mid.** Spread is admin-only via `set_params`, so a hot key cannot zero the spread or break bid prices. `update_price` runs full `Ladder::validate()` with the new mid.
5. **Swap derives everything from `side`.** BuyBase: in = quote_vault, out = base_vault. SellBase is the reverse. User token accounts are checked with `token::mint` against those fixed mints. Direction is never derived from caller-supplied mints (unit-confusion attack: pay 1 SOL, receive 5,000 USDC).
6. **Pause scope.** Pause blocks swap and update_price. It never blocks deposit, withdraw, or admin instructions.
7. **No admin rotation in v1.** Documented risk: a lost cold key locks funds. If added later, use two-step propose/accept.
8. **Residual risk.** With one update per slot and `max_move_bps = b`, a stolen updater key can move the mid by at most +-b per slot (compounding; 4 slots/s at 250 ms, so at b = 100 bps about +-4% per second) and take at most one depth per side per window. A window is one slot, but a slot can see the unused rest of the previous window plus a fresh one, so over K slots the total is at most (K+1)*D per side. A buy-low/sell-high round trip needs two slots and nets about (2b) * D * W per two slots (D = depth, W = notional per unit of depth). Mitigation: monitoring plus pause; operators should set b well below the cap.
   - Incident runbook: pause -> `set_updater(new)` -> `force_price(fair)` -> unpause. Run `force_price` after rotating the updater: on an unpaused market it gives an attacker who still holds the key a fresh window.
9. **Other fixes.** PDA seeds include the admin; `set_params` invalidates the window; updater cannot be the default key or the admin; `max_move_bps <= 100`, `max_age_slots <= 1_000`; ladder depth must fit u64.

## Swap requirements
All satisfied in `programs/propamm/src/instructions/swap.rs`. Covering tests in `crates/propamm-tests/tests/swap.rs`: 1 `buy_happy_path_matches_offchain_quote` / `sell_happy_path_matches_offchain_quote`; 2 `set_params_invalidates_until_next_update`, `depth_exhaustion_accumulation_and_refresh`; 3 `staleness_boundary`; 4 happy-path tests (state equality); 5 `paused_rejects_and_unpause_restores`; 6 `unpriced_market_rejects`; 7 `unit_confusion_attempts_fail`.

1. Swap never rolls the window, resets `consumed`, or writes `last_update_slot` / `window_start_mid`.
2. Pass `state.consumed(side)` straight into `quote_exact_in` (the `u64::MAX` sentinel then yields `InsufficientDepth`). Never clamp, normalize or wrap it. Write back only via `set_consumed(side, fill.consumed_after)`.
3. Check `is_fresh(clock.slot, last_update_slot, max_age_slots)`.
4. Never price off `window_start_mid`.
5. Check `paused`.
6. Return an explicit `NotInitializedPrice` when `mid_price == 0`, for a clear error.
7. Vaults and mints are chosen by `side`, never from caller-supplied mints.

## Scenario results
`crates/propamm-tests/tests/scenarios.rs`; run `cargo test -p propamm-tests --test scenarios -- --nocapture`. SOL/USDC, mid 200.00, +-2 bps, one 50 SOL level per side, `max_age_slots` 8, `max_move_bps` 100. Maker P&L is valued at the external price.

| Scenario | Maker P&L (USDC) | Notes |
|---|---|---|
| update_first (external 200.40, update lands first) | +0.4008 | Trader buys 10 SOL at 200.44008. A 50 SOL arb would lose 2.004, so it stays out. |
| update_late (arb lands before the update) | -17.5992 | Arb buys 50 SOL at 200.04: -18.00 (full window depth; one more USDC fails `InsufficientDepth`). Then +0.4008 from the trader. |
| stale_rejected | n/a | Age 8 trades, age 9 fails `StalePrice` on both sides. `update_price` (+10 bps) or `force_price` restores trading. |
| split_order_never_helps | n/a | 1 level: 10 x 5 SOL = 50 SOL (equal). 8 levels of 10 SOL: single 50.000000000 SOL, 10 x split 49.999999978 SOL. |
| compromised updater, buy side, K=8 | -3501.95 | Mid walks down 1% per slot, attacker buys 50 SOL each slot. Bound 3501.95 (closed form 14,373.98). |
| compromised updater, sell side, K=8 | -3668.54 | Mid walks up 1% per slot, attacker sells 50 SOL each slot. Bound 3668.54. |
| stalled updater | -14.40 | Remaining ask depth 40 SOL taken at the stale 200.04 vs 200.40, then `StalePrice` at the next slot. |

- Bound used in the test (per side): sum over the K+1 windows of `D * max(0, F - ask(L_k))` with `L_k` the lowest mid reachable after k updates (floor band, compounding); the sell side is symmetric. An attacker moves the mid one way per slot, so the total is at most the sum of both sides. The measured loss matches the tight bound, since the attack is the extreme path; the closed form `(K+1) * D * (F - ask(L_K))` per side is looser.
- Runbook check: after pause -> `set_updater(new)` -> `force_price(200.00)` -> unpause, the old key fails `Unauthorized`, the new key can post, and swaps price at 200.04 again.

## Per-network config
- Slot time differs by network.
- Mainnet ~250ms since 2026-09-18 (SIMD-0525, epoch 1037).
- 200ms stage is active on devnet/testnet per solana.com. Mainnet date TBD.
- `max_age_slots` and bot heartbeat are per-network config.
- The UI reads slot duration. It never assumes one.

## Write locks
- `update_price` writes Market.
- `swap` writes Market (`consumed`) + vaults.
- Update and swaps serialize on Market. Accepted for v1.

## CU expectations
| Item | Number | Source |
|------|--------|--------|
| Anchor overhead per instruction | ~450-550 CU | Anchor bench / COMPUTE_UNITS.md, Agave 4.2 |
| p-token transfer | 79 CU | [secondary, Helius] |
| SPL Token transfer | 4,645 CU | [secondary, Helius] |
| Real prop AMM update | 47-676 CU | see below |
| HumidiFi update | 47 CU | operator interview [secondary] |
| Prop AMM update, median | 485-676 CU | arXiv 2609.38056 |
| Prop AMM swap | >= 16,938 CU | arXiv 2609.38056 |

- Swap is dominated by two token CPIs.
- A Pinocchio rewrite of `update_price` should cut CU by a multiple. Swap gains are modest.

## CU ladder
Three rungs over the same account bytes and pricing crate. Reproduce: `cargo test -p propamm-tests --test pinocchio cu_ladder -- --nocapture` (LiteSVM 0.17, release `.so`).

1. **Anchor**: `programs/propamm`, default build.
2. **Anchor fast path**: cargo feature `fast-update` (off by default, test-only build, never deployed). `make build-sbf` builds it: `cargo build-sbf --tools-version v1.57 --manifest-path programs/propamm/Cargo.toml --features fast-update --sbf-out-dir target/deploy/fast`, after copying `target/deploy/propamm-keypair.json` to `target/deploy/fast/` so the build keeps the Anchor program id.
   - Raw `update_price` through Anchor's `fallback` (`programs/propamm/src/fast.rs`): data `[0x01, mid as u64 LE]`, accounts `[updater (signer), market (writable)]`, the same order as Anchor's `UpdatePrice`.
   - `0x01` is the first byte of no instruction discriminator (`0f 1b 3d 5b 95 af b7 f2 f8`) nor the event-CPI tag (`e4`). A compile-time assert checks this, so fast-path data can never reach a normal handler.
   - Checks in the normal handler's order, so a multi-fault input returns the same code as `update_price`: updater signs (3010), market owner = program (3007), data >= 8 bytes (3001), `Market` discriminator (3002), market writable (2000), data >= 272 (3003), stored updater (`Unauthorized`, 6000). Then `apply_update_price`, the same function the normal handler calls. Unlike the Pinocchio port, the fast path accepts oversized markets like Anchor does.
   - Anchor runs `mut` before the `load()` size check (constraint order on `market`), so a read-only market of the wrong size gives 2000, not 3003.
   - Without the feature, `fallback` returns `InstructionFallbackNotFound` (101), which is what Anchor returns when a program has no fallback. The default build's CU are unchanged.
3. **Pinocchio port**: `programs/propamm-pinocchio`, program id `HTsHg7iAkb5YtrV9in5537j8iijh1VHrk1t2dFMpB287`. pinocchio 0.11.2, pinocchio-token 0.7.0, pinocchio-system 0.6.1, `no_std`, no allocator.
   - Same market bytes (Anchor `Market` discriminator + `MarketState`) and PDA seeds (derived under its own id), same account order per instruction.
   - Data: a 1-byte tag (0 initialize, 1 deposit, 2 withdraw, 3 set_params, 4 set_updater, 5 set_paused, 6 update_price, 7 swap, 8 force_price), then the same little-endian fields Borsh writes for the Anchor args. So the Pinocchio data is `[tag] ++ anchor_data[8..]`.
   - Error codes are numerically identical: `PropAmmError` keeps 6000-6016, account checks reuse the Anchor framework codes, token-data errors are the builtin `InvalidAccountData` / `UninitializedAccount`.

Authoritative CU table (LiteSVM 0.17, release `.so` built with platform-tools v1.57, after the security-review changes). This is the only current table.

| Instruction | Anchor | Anchor fast path | Pinocchio |
|---|---|---|---|
| initialize | 21,740-32,240 | - | 11,900-20,710 |
| deposit | 6,262 | - | 1,811 |
| withdraw | 6,375 | - | 1,874 |
| set_params | 1,221 | - | 265 |
| set_updater | 1,148 | - | 180 |
| set_paused | 1,091 | - | 153 |
| force_price | 1,784 | - | 767 |
| update_price | 1,992 | 1,625 | 969 |
| swap BuyBase (one level) | 14,945 | - | 5,083 |
| swap SellBase (one level) | 14,448 | - | 4,574 |
| swap BuyBase, 32 levels | 31,154 | - | 21,292 |
| swap SellBase, 32 levels | 26,514 | - | 16,640 |

- All three `.so` files are built with platform-tools v1.57 (what `anchor build` passes). Plain `cargo build-sbf` defaults to v1.54 here, which costs 3-8% more (Pinocchio update_price 1,007, Anchor 2,124, fast path 1,748). All asserted budgets pass under both.
- initialize varies with the bump search of its three `find_program_address` calls.
- The fast path skips Anchor's per-instruction work (log line, Borsh, `try_accounts`, `exit`) but still pays the Anchor entrypoint's account deserialization: -368 CU (-18%).
- The full Pinocchio port is where `update_price` drops by a multiple: 1,992 -> 969, about 2x. Most of the rest is the shared pricing checks. From the deltas set_paused (152) -> force_price (766) -> update_price (969), the clock read plus `validate_mid` cost about 600 CU and `within_move_limit` + `roll_window` about 200. These are u128 `mul_div`s, and SBF has no native 128-bit division. The 47-676 CU of real prop AMMs implies much less on-chain validation; we keep it (security decision 4).
- Swap: the expectation "swap stays dominated by token CPIs" holds only partly. LiteSVM 0.17 runs p-token at the Tokenkeg address, 105 CU per `transfer_checked` (program logs). The cost is the CPI itself: about 1,100-1,200 CU per transfer in the port (invoke cost plus the 105 CU call), about 2,000 through anchor-spl.
  - Pinocchio, one level: the two CPIs are about 2,300 of 5,083 CU (~45%). With 32 levels the ladder walk dominates (about 400-500 CU per level).
  - Anchor: about 11,800 CU go by before the first CPI (nine accounts, six token-account unpacks, the handler), so the framework, not the token program, dominates.
  - On a cluster where Tokenkeg is the legacy SPL Token, add several thousand CU per transfer (4,645 for a legacy transfer, see CU expectations) to every swap, deposit and withdraw row.
- Budgets (measured + ~20%) are asserted in `cu_ladder` for the Pinocchio update_price, force_price, deposit, withdraw and the four swaps, and for the fast path.

### Anchor constraint -> Pinocchio check
Every check runs in Anchor's order: argument decoding, then accounts in declaration order (type checks), the duplicate-mutable check, field constraints in order, then the handler. Helpers in `src/checks.rs` name the Anchor check they replace.

| Anchor | Pinocchio (`programs/propamm-pinocchio/src`) | Error |
|---|---|---|
| `declare_id!` check in `entry` | `lib.rs` | 4100 |
| dispatch without fallback; Borsh args (bool 0/1, enum variant, length) | `lib.rs`; `state::Args`, swap side | 101; 102 |
| `try_accounts` consumes accounts in order | `checks::next` | 3005 |
| `Signer` | `checks::signer` | 3010 |
| `#[account(mut)]` | `checks::writable` | 2000 |
| `AccountLoader<Market>`: owner, discriminator | `checks::market` | 3007, 3001, 3002 |
| `load()` size | `checks::load` / `load_mut` | 3003 |
| `Account<Mint>` / `Account<TokenAccount>`: not initialized, owner SPL Token, `unpack` (length, state) | `checks::mint`, `checks::token_account` | 3012, 3007, builtin |
| `Program<Token>` / `Program<System>`: id, executable | `checks::program` | 3008, 3009 |
| duplicate mutable accounts (Funds: vault, admin_token; Swap: the four token accounts) | `checks::no_duplicates` | 2040 |
| admin / updater `constraint` | `admin::admin_only`, `funds::funds`, `price::update_price` | 6000 |
| Funds: mint is a market mint, vault is that mint's stored vault | `funds::funds` (`vault_for`) | 6002, 6003 |
| Swap `address =` stored mints / vaults | `swap::swap_accounts` | 6002, 6003 |
| `token::mint`, `token::authority` | `funds::funds`, `swap::swap_accounts` | 2014, 2015 |
| `init` + `seeds` + `bump` (market, two vaults) | `initialize::process`, `init_vault` (`find_program_address`) | 2006 |
| `init` create, or top up + allocate + assign for a pre-funded PDA; payer is not the account | `initialize::init_account` | System, 4101 |
| vault `token::mint = mint`, `token::authority = market` on init | `InitializeAccount3` CPI, then `checks::token_account` | SPL |
| `rent_exempt` on `init` accounts; `Rent::get()` for the `init` lamports | `initialize::rent_exempt`, `rent::Rent` | 2005 |
| `MarketParams::validate`, `validate_updater`, `base_mint != quote_mint` | `state.rs`, `initialize::process` | 6001, 6008, 6002 |
| handler checks (paused, unpriced, window, move limit, `validate_mid`, freshness, quote, slippage, vault balance) | `price.rs`, `admin.rs`, `swap.rs` | 6004-6016 |
| PDA signer seeds from stored state (withdraw, swap) | `state::MarketSeeds` | - |

Rent: pinocchio 0.11's `Rent` reads only the first `u64` of the sysvar and ignores `exemption_threshold`. That is right for SIMD-0194 (`{6960, 1.0, 50}`) and wrong on a cluster that still has the legacy shape (`{3480, 2.0, 50}`): the port would create the market and vaults at half the real minimum and fail the 2005 check, or fund PDAs too low. `src/rent.rs` reads 16 bytes through `sol_get_sysvar` (rate, threshold) and computes `(128 + data_len) * rate * multiplier` exactly like `solana-rent`, with no floating point: multiplier 1 for threshold `1.0`, 2 for `2.0`. Any other threshold, a rate above `solana-rent`'s overflow guard, or `data_len` above 10 MiB fails with `InvalidArgument` (`solana-rent` panics on BPF for those). The same minimum funds the PDAs and drives the 2005 check. Covered by `initialize_matches_under_both_rent_shapes`.

Not ported because they cannot fail after the checks above: `load_mut`'s writable check (3006), `load_init`'s zero-discriminator check (3000), the repeated discriminator checks of every `load`, `exit`'s discriminator rewrite.

Deviations from the Anchor program:
- No logs at all: no "Instruction: X" line and no `SwapEvent`. Indexers read the instruction and the token balance changes, or use the Anchor build.
- The port requires the market to be exactly 272 bytes; Anchor's loader (and the fast path) accepts `>= 272`. Both programs only create 272-byte markets and nobody else can resize a program-owned account, so no reachable input behaves differently. Pinned by `market_size_bounds`: an oversized market works on Anchor and returns 3003 on the port.
- `spl_token` `unpack`'s `COption` tag checks are not ported (unreachable for SPL-Token-owned accounts).
- Accounts beyond the 9th are ignored (Anchor ignores remaining accounts too). Error messages differ; codes do not.

Tests (`crates/propamm-tests/tests/pinocchio.rs`): a differential harness runs every instruction on both programs in one LiteSVM and asserts the same outcome (same error code or success) and, after every call, the same market bytes (except the vault addresses and bump), lamports and token balances. It covers every instruction's happy path and the failure cases of the Anchor program (admin-only, updater rules, window and move limit, stale, slippage, depth, unit confusion and account substitution, wrong token program, non-signer, read-only, missing accounts, bad argument bytes, pre-funded PDAs). The fast path is compared byte-for-byte with `update_price` from the same snapshot, for successes and every failure mode, and `fast_path_error_parity` checks that multi-fault inputs return the same code on both. Further cases: multi-fault token accounts, frozen trader account, undersized and oversized markets, wrong owner or discriminator on swap and funds, read-only accounts, extra trailing accounts, both Rent shapes. Tests need `target/deploy/propamm_pinocchio.so` and `target/deploy/fast/propamm.so` (`make build-sbf`).

## Reference implementation
- github.com/benedictbrady/prop-amm.
- Pinocchio 0.8, ladder of 7 prices / 6 segments per side, `consumed` reset on oracle update.

## Aggregator adapter
- `crates/jupiter-sdk` (package `propamm-jupiter`) implements Jupiter's `Amm` trait for a market. It decodes the Market account (8-byte discriminator + `MarketState`), keeps vault balances from `update`, and quotes with the same `Ladder::quote_exact_in` the program runs.
- `quote` is ExactIn only. It rejects unknown mint pairs, paused markets, `mid == 0`, stale prices (`is_fresh` against the `AmmContext` clock) and out-vault balances below `amount_out`. `fee_amount` is 0 because the spread is inside the price.
- `get_swap_and_account_metas` returns the `swap` account order above (trader signs) and `Swap::Placeholder { data: [side] }`. A real listing needs a Jupiter-assigned `Swap` variant. Which interface version Jupiter integrates is unconfirmed.
- `crates/jupiter-sdk/tests/parity.rs` asserts the trader's token deltas equal the quote exactly (single level, multi level, mid-level start). It replaces `jupiter-amm-test-kit`, which pins litesvm 0.14.
