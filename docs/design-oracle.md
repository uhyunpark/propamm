# Fair-value Bot

## Principle
- The bot is the oracle. On-chain stores one price and enforces staleness. It never judges price correctness.
- Pyth is not the price source. It solves third-party trust, which a maker trading its own capital does not need. It is public, so it gives no edge.
- Pyth is used only as a circuit breaker: halt if |fair - Pyth| exceeds a threshold.

## Venue roles
| Role | Source | Notes |
|------|--------|-------|
| Input, primary | Binance USDⓈ-M perp `solusdt@bookTicker` | Has E/T timestamps. `wss://fstream.binance.com/ws/...` |
| Input, basis anchor | Binance spot `solusdt@bookTicker` | No exchange timestamp: use local receive time. Fallback host `data-stream.binance.vision` |
| Input, conversion | Binance spot `USDCUSDT` | |
| Held-out reference | Coinbase Exchange `level2_batch` SOL-USD | Never an input, else evaluation is circular. `wss://ws-feed.exchange.coinbase.com`, no auth, 50ms batches. Do not use `ticker`: it fires only on matches |
| Record-only (weight 0 in v1) | Hyperliquid `bbo` SOL | `wss://api.hyperliquid.xyz/ws`. Block-paced. Lags Binance ~700-800ms (Arrakis, Feb 2026) [secondary] |
| Record-only candidates | OKX bbo-tbt, Bybit orderbook.1 | Korea access verified for public WS data (probe 2026-10-04) |

- Upbit/Bithumb are KRW markets with a premium. Unsuitable as a USD reference.

## Normalization
- SOLUSDC = SOLUSDT / USDCUSDT, with EMA 5-30s on USDCUSDT.
- Coinbase SOL-USD is treated as USDC (books merged 2022) [secondary].

## Basis
- `b = perp_mid / spot_mid - 1`
- `perp_adj = perp_mid / (1 + b_hat)`
- `b_hat` = EMA with half-life 10-60s. Update only when both books are fresh (< 500ms).
- Optional funding prior from markPrice r/T.
- Pitfalls:
  - Basis jumps in liquidations.
  - Staleness gets absorbed as basis.
  - An EMA that is too fast erases the perp lead.

## Aggregation v1
1. Weighted mid per venue: size-weighted, imbalance clipped to 0.1-0.9.
2. Drop stale: BBO > 500ms, slow feeds > 2s, or use exponential decay weights.
3. Drop outliers: |p - median| > k * max(spread, c * sigma), k ~ 3.
4. Weighted mean of survivors.
5. Publish `fair` and `conf` (dispersion). `conf` maps to half_spread.

Halt if:
- fewer than 2 live sources, or
- |USDCUSDT - 1| > 30 bps (suggested threshold).

## Publish policy
- Send when |fair - onchain_mid| > x bps, or on heartbeat y ms with y < `max_age`.
- x and y are per-network.

## Offline evaluation
- Record raw ticks with local receive time.
- Error: `e_t(h) = (M_{t+h} - F_t) / F_t * 1e4`, for h in {0, 100ms, 1s, 5s}. M is the reference mid.
- Report: bias, RMSE, MAE, p50/p90/p99 of |e|.
- Baselines: primary-venue mid, equal-weight mean.
- Split: walk-forward. No random split.
- Add measured publish latency to t before looking up M.
- Markout for simulated quotes `q = F +/- half_spread`, fills from trade prints:
  - `markout_h = s * (M_{t+h} - q)`, s = side sign.
  - Adverse selection = `markout_0 - markout_h`.
- Block bootstrap CIs are optional.

## Infra notes [secondary]
- Binance in AWS Tokyo (~72ms REST from AWS Seoul).
- OKX ~Hong Kong. Bybit ~Singapore. Coinbase likely US East.
- Hyperliquid validators in Tokyo.
- Solana stake/leaders concentrated in Frankfurt/Amsterdam (~half of block production, early Sept 2026).
- End-to-end feed -> send -> confirmed latency must be measured. On-chain landing likely dominates.

## Recording format
- Path: `data/raw/<feed>/<YYYY-MM-DD>/<HH>.jsonl.zst`, UTC, by local receive time. `/data` is gitignored.
- Line: `{"recv_ns":<unix ns>,"feed":"<name>","msg":<raw message verbatim>}`.
- Files are zstd frames, rotated hourly and flushed every 5s. A restart inside an hour appends a new frame; `zstd -dc` reads it as one stream.
- Skipped to save space: keepalive replies (OKX literal `pong`, `{"channel":"pong"}`, `{"op":"pong"}` / Bybit `ret_msg":"pong"`) and non-JSON text. Subscription acks are kept.
- The feed table lives in `crates/bot/src/feeds.rs`. Parsers are in `crates/bot/src/parse.rs` (see Implementation).

## Verified endpoints (probe 2026-10-04)
- Korea access works for all 7 venues, including OKX and Bybit.
- Binance perp `aggTrade` and `markPrice` stream only on `wss://fstream.binance.com/market/ws/...`. On `/ws/` and `/public/ws/` they connect but stay silent. `bookTicker` works on `/ws/`.
- Binance spot uses `data-stream.binance.vision`. `usdcusdt` can be quiet for a while (silence threshold 60s).
- Coinbase `level2_batch` is acked as `level2_50`, with ~100ms batch lag. Heartbeat 1/s.
- Hyperliquid sends a snapshot of old trades right after subscribing to `trades`. It is recorded; consumers must drop it (by `time` or by position before the first live message).
- Clock skew between local receive time and exchange timestamps is within a few ms.

## Implementation
Code: `crates/bot/src/{parse,fair,replay,eval}.rs`. Run: `propamm-bot eval --data data/raw [--from/--to unix_s] [--latency-ms 150]`. Report: [`docs/eval/2026-10-05.md`](eval/2026-10-05.md).

### Parsers
- Enum-dispatched `Parser` per feed file, output `Event::{Bbo, Trade, Mark}` with `recv_ns` and `exch_ts_ms: Option<u64>` (`None` for Binance spot and Coinbase). Prices are `f64`.
- Coinbase: `snapshot` resets the book (also covers reconnects), `l2update` size 0 removes a level, a `Bbo` is emitted only when the best price or size changes. `match` gives the trade with the taker side opposite to the maker `side`. `last_match` is skipped.
- Hyperliquid: the `trades` snapshot is dropped by age (`recv - time > 5 s`). The recordings contain no `isSnapshot` field.
- Bybit `orderbook.1` is always a full `snapshot` in the recordings. A one-sided `delta` keeps the other side.
- Binance `aggTrade`: `m = true` means the taker sold.

### Fair v1
| Parameter | Value |
|---|---|
| Inputs | Binance perp bbo (weight 2), Binance spot bbo (weight 1). Hyperliquid, OKX, Bybit weight 0 |
| Per-venue price | microprice `bid + I * (ask - bid)`, `I = bid_size / (bid_size + ask_size)` clipped to [0.1, 0.9] |
| Basis | per non-spot source, `b = p / spot - 1`, EMA half-life 30 s, updated only when both books are younger than 500 ms. `p_adj = p / (1 + b)` |
| USDC | EMA of the USDCUSDT mid, half-life 10 s. `fair = p_adj / usdc_ema` |
| Max quote age | 500 ms for perp, OKX, Bybit. 2 s for spot and Hyperliquid |
| Outliers | with 3 or more candidates: drop `\|p - median\| > 3 * max(spread, 1 * sigma)`. `sigma` is the EMA (half-life 10 s) of the survivors' weighted standard deviation |
| Output | `FairValue { ts_ns, fair, conf_bps, live_sources }`. `conf_bps` is the weighted standard deviation of survivors in bps |
| Halt | fewer than 2 live sources, USDCUSDT never seen, or `\|USDCUSDT - 1\| > 30 bps` |

Deviations from the sketch above:
- Spot gets the 2 s "slow feed" limit. `bookTicker` publishes only on change, so a quiet book is still valid. With 500 ms, v1 would halt whenever spot is quiet.
- USDCUSDT is not age-gated. The feed can be quiet for up to 60 s.
- With 2 candidates both sit equally far from the median, so outlier rejection is off. A large perp/spot disagreement shows up in `conf_bps` and does not halt. The bot should widen the quote on `conf_bps`.
- Other perps (Hyperliquid, OKX, Bybit) get their own basis EMA against spot, so a USDC-quoted venue is normalized the same way as a USDT one.
- The funding prior from `markPrice` is parsed (`Event::Mark`) and not used.

### Evaluation method
- One replay in `recv_ns` order (k-way merge of the hourly files). Every Binance perp or spot bbo triggers a sample of all candidates. The first 90 s are warm-up and not scored. A truncated last zstd frame is skipped.
- Reference M is the Coinbase SOL-USD mid, never an input. `e = (M_{t+L+h} - F_t) / F_t * 1e4`, L = 150 ms publish latency. A reference older than 2 s at the lookup time, or past the end of data, drops the sample.
- All candidates are scored on the same samples (every core candidate has a value). Samples overlap in time, so `n` overstates the independent count.
- Candidates: `perp_raw` (Binance perp mid, USDT, no adjustment), `spot_conv` (spot mid / USDC EMA), `equal_wt` (v1 engine with weights 1:1), `v1`, and v1 plus Hyperliquid, OKX, Bybit at weight 0.25 each and all three together.
- Walk-forward: the first and second half of the scored time span, reported separately.
- Markout: quotes `F * (1 -/+ 2 bps)` published 150 ms before each Binance perp aggTrade. The bid fills when a taker-sell print is at or below it, the ask when a taker-buy print is at or above it. Prints are converted to the candidate's units with the same basis and USDC factor (perp prints are USDT perp prices, so comparing raw prints to a USDC fair would fill the bid almost every time). `markout_h = side * (M_{t+h} - q) / q`. No queue position or fill probability is modeled.
- Lead-lag: correlation of 500 ms mid returns between each source and the Binance perp mid on a 100 ms grid, for lags from -300 to 1500 ms.

## Findings
Dataset: 2026-10-05 00:38 to 02:34 UTC, 115 min, 1.17 M samples, v1 halted on 0.09% of them. Full tables in the [report](eval/2026-10-05.md). Caveat: the Coinbase reference only gets a new point when its top of book changes and samples more than 2 s after the last change are dropped, so scored samples lean slightly toward active periods.
- **Raw perp mid is biased.** It sits 3.2 bps above Coinbase at every horizon (basis plus the USDT/USDC gap). Basis adjustment and USDC conversion remove it: v1 bias is -0.2 bps and RMSE at h = 0 drops from 3.3 to 0.7 bps.
- **v1 matches the equal-weight mean and beats spot-only at 1 s and 5 s.** At 150 ms latency, RMSE is 0.71 / 0.71 / 0.98 / 1.81 bps for v1 against 0.77 / 0.84 / 1.12 / 1.92 for `spot_conv` at h = 0 / 100 ms / 1 s / 5 s. The 2:1 perp weight did not beat 1:1, so the weight choice is not tuned. The gain over spot-only comes from the perp's faster updates. Both halves of the data agree (second half slightly better).
- **Hyperliquid, OKX and Bybit add almost nothing.** Adding all three at weight 0.25 cuts RMSE at h = 0 from 0.71 to 0.68 bps and nothing at 1 s or more. Hyperliquid trails Binance perp by about 750 ms by return correlation (0.56 at 750 ms, 0.07 at 0) and has a 240 ms median transport lag. OKX is level with Binance (0 ms), Bybit trails by about 50 ms. Hyperliquid stays at weight 0.
- **Coinbase trails Binance by about 150 ms**, so the h = 0 and 100 ms rows measure against a lagging reference. With L = 0 instead of 150 ms, v1 RMSE at 100 ms goes from 0.71 to 0.78 bps. Errors at h = 5 s (1.8 bps) are dominated by price movement, not source error. The h = 0 error (0.7 bps) is near the Coinbase half spread (about 0.4 bps), the floor of this reference.
- **Quoting 2 bps around F loses to adverse selection.** v1 fills have a mean markout of +2.05 bps at 0, -0.08 bps at 1 s and -0.36 bps at 5 s (152 fills). The edge is gone within 1 s, so the half spread must exceed 2 bps, and `max_age` plus the update cadence matter more than a better fair value. 150 fills is a small sample; treat the sign as indicative.

## Publishing
Code: `crates/bot/src/{publish,pyth,run}.rs`. Run: `propamm-bot run --config data/localnet.json` (`--rpc`/`--ws` default to the config's URLs).

### Policy (`publish.rs`, pure and unit-tested)
- Target mid = fair adjusted by inventory skew, then clamped into `[mid * (1 - max_move_bps), mid * (1 + max_move_bps)]` (the program's `move_band`) around the CURRENT on-chain mid. Each update opens a new window whose reference is the previous mid, so the band moves with every update.
- Skew: `skew_bps = -skew_max_bps * clamp((base_value_share - 0.5) * 2, -1, 1)`, with the vault balances valued at fair. Too much base lowers the mid.
- Send when `|target - on-chain mid| >= threshold_bps`, or when `slot - last_update_slot >= heartbeat_slots`. The heartbeat must be below `max_age_slots` (`PolicyParams::validate`; the bot refuses to start otherwise). Defaults: threshold 1 bps, heartbeat `max_age_slots / 2`, skew max 3 bps.
- At most one `update_price` per slot. While the previous update is not yet visible in the market account, the bot also skips the next slot, because the band reference would be stale. A landing delay of 2 or more slots can still produce `AlreadyUpdatedThisSlot`; it is only a wasted fee.
- Clamped for more than 20 slots in a row: warning that recommends the admin `force-price`.
- Halt (send nothing): the fair engine halts, or the Pyth breaker trips. The on-chain price then goes stale after `max_age_slots` and swaps stop, which is the intended fail-safe. The market having no price or being paused also sends nothing, and so does market state last confirmed (ws or RPC) more than 6 s ago (`Hold::StaleState`).

### Pyth breaker (`pyth.rs`)
- Hermes `/v2/updates/price/latest` for `Crypto.SOL/USD` (id `ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d`, verified via `/v2/price_feeds?query=SOL&asset_type=crypto`), polled every 2 s. Trips when `|fair - pyth| > pyth_max_dev_bps` (default 50).
- Hermes unreachable, rejecting, or older than 30 s: log a warning and keep running. The breaker guards against the bot's own feeds being wrong. Halting on a third-party outage would let a Hermes problem stop the market, and the staleness check and move limit bound the damage anyway.
- As of 2026-10-05 the public Hermes returns 401 for price endpoints, so the breaker is inactive unless `--pyth-url` points to a working endpoint.

### Transactions
- ComputeBudget limit about 4,000 (plus slot modulo 64, so two sends of the same mid on one cached blockhash are different transactions) and price `--cu-price`. Blockhash cached for 4 s. Send with `skipPreflight`, no waiting. Signature status is polled every 200 ms. Reads use `processed` commitment.
- Per update the bot logs target slot, landed slot, delay, fair, target, on-chain mid before and after. A status line every 10 s. A summary on exit with the landing delay histogram and the `|target - mid|` distribution.

### Localnet results (120 s, validator slot 400 ms, threshold 1 bps, demo params half spread 5 bps)
- 28 updates sent, 28 landed, 0 failed. Landing delay (landed slot minus the slot the bot saw when sending): 27 at 0 slots, 1 at 1 slot.
- `|target - on-chain mid|`, sampled every 100 ms: p50 0.16 bps, p90 0.68, p99 1.74, max 2.50; 4.6% of samples at or above the 1 bps threshold. Nothing was clamped (real SOL moves are far below 100 bps per slot).
- Swaps (buy and sell, 1 to 30 SOL equivalents): actual output equals the pricing-crate quote, also when an update lands between quote and swap (the tool replays the quote at the mid and window from the program's `SwapEvent`).
- Bot stopped: the swap failed with `StalePrice` (6014) once the price was older than 20 slots. Bot restarted: the next swap succeeded.
- Brief fair halts (a quiet perp book older than 500 ms leaves one live source) occurred every few tens of seconds in this quiet market and lasted under a second.

### Devnet results
Run on 2026-10-05: `propamm-bot run --config data/devnet.json --serve 127.0.0.1:8788` against the public `api.devnet.solana.com` for about 6 minutes (defaults for a remote host: status poll 1000 ms, market refresh 3000 ms). Program `2PuFKsYDoYaVWFnSxVGgbD2Xvsz3nymLu2WuctvF6V3F`, market `ZAyJyncP3UN2BeU8uKKD65x9F6iiUBXxGgmKjRf4AFk`.

| Metric | Value |
|---|---|
| Updates sent / landed / failed | 167 / 163 / 4 (all `AlreadyUpdatedThisSlot`) |
| Landing delay in slots | p50 1, p90 2, max 13; histogram 1:132 2:22 4:1 5:4 6:2 9:1 13:1 |
| Landing delay in ms (slot time about 237 ms, from `getRecentPerformanceSamples`) | p50 about 240, p90 about 470, max about 3,100 |
| `abs(target - on-chain mid)`, 2,753 samples at 100 ms | p50 0.06 bps, p90 0.58, p99 1.71, max 3.54; 2.8% at or above the 1 bps threshold |
| HTTP 429 responses / RPC retries | 0 / 0 |
| Swaps | buy 100 USDC -> 0.828676813 SOL, sell 1 SOL -> 120.555921 USDC; quote equals actual |

- Compared with localnet (0 to 1 slot, 400 ms slots), devnet lands mostly 1 slot later (the 0-slot case needs the update in the same slot the bot saw) and has a tail up to 13 slots. A delay of 2 or more slots can produce `AlreadyUpdatedThisSlot` when the bot sends again before the previous update is visible.
- The fair engine halted briefly many times (a quiet perp book leaves one live source), as on localnet.
- The delay is measured in slots from the bot's ws slot. The 1000 ms status poll does not affect it, because the landed slot comes from the signature status.
