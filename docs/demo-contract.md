# Demo interface contract

Shared contract between the WASM module, the bot server, and the web app. Change it here first.

## WASM module `propamm-wasm`
Built by `make wasm` with `wasm-bindgen --target web` into `web/src/wasm/` (generated, gitignored). All u64 values are JS `bigint`. Prices are pricing-crate scaled (quote atoms per base atom x 1e12). Errors throw `Error` whose message is the pricing/program error name (e.g. `InsufficientDepth`, `ZeroOutput`; also `WrongSize`, `WrongDiscriminator` for bad account data and `InvalidSide`). `quoteExactIn` returns `Quote = { amountOut: bigint; consumedAfter: bigint }`. `ladder` marks levels below the consumed amount with `remaining = 0`.

```ts
type Side = "buy" | "sell"; // trader's view: buy = pay quote, receive base
interface MarketView {
  admin: string; updater: string; baseMint: string; quoteMint: string; baseVault: string; quoteVault: string; // base58
  midPrice: bigint; lastUpdateSlot: bigint; windowStartMid: bigint;
  askConsumed: bigint; bidConsumed: bigint; levelSize: bigint; maxAgeSlots: bigint;
  halfSpreadBps: number; levelStepBps: number; maxMoveBps: number; numLevels: number; paused: boolean;
}
interface Level { price: bigint; remaining: bigint } // remaining base atoms at this level given consumed
export function decodeMarket(data: Uint8Array): MarketView;          // full account data incl. 8-byte discriminator
export function quoteExactIn(data: Uint8Array, side: Side, amountIn: bigint): { amountOut: bigint; consumedAfter: bigint };
export function isFresh(currentSlot: bigint, lastUpdateSlot: bigint, maxAgeSlots: bigint): boolean;
export function swapIxData(side: Side, amountIn: bigint, minOut: bigint): Uint8Array;
export function ladder(data: Uint8Array, side: Side): Level[];      // ask levels for "buy", bid levels for "sell"
export function priceToUi(priceScaled: bigint, baseDecimals: number, quoteDecimals: number): number;
```

## Bot server (`propamm-bot run --serve 127.0.0.1:8788 [--faucet]`)
Binds 127.0.0.1 only. CORS allows `http://localhost:5173` and `http://127.0.0.1:5173`.

- `GET /config` -> `{ programId, market, baseMint, quoteMint, baseVault, quoteVault, baseDecimals, quoteDecimals, rpcUrl, wsUrl, tokenProgram }` (base58 strings).
- `WS /stream` -> JSON messages:
  - `{ "type": "tick", "tsMs": number, "venues": { "<feed>": { "bid": number, "ask": number, "mid": number } }, "fair": { "price": number, "confBps": number, "live": number } | null, "chain": { "slot": number, "mid": number, "lastUpdateSlot": number, "ageSlots": number, "maxAgeSlots": number, "paused": boolean } | null, "inventory": { "base": number, "quote": number, "skewBps": number } | null, "pythDevBps": number | null, "publishing": boolean, "offsetBps": number }` every 250 ms. Venue keys: `binance_perp`, `binance_spot`, `okx_perp`, `bybit_perp`, `hyperliquid`, `coinbase` (reference). Prices in USD (UI units); venue quotes are the raw BBO (USDT-quoted perps and Binance spot are not converted). A venue without a quote in the last 5 s is left out of `venues`. Binance, OKX, Bybit, Hyperliquid and Coinbase feeds are only connected with `--serve`; the extra venues never feed the fair value. Ticks are sent only while a client is connected. `fair.price` excludes the demo offset; while `offsetBps` is nonzero the bot targets `fair.price x (1 + offsetBps / 10^4)`, which is what the web charts as the fair value.
  - `{ "type": "update", "signature": string, "targetSlot": number, "landedSlot": number | null, "delaySlots": number | null, "mid": number, "status": "sent" | "landed" | "failed" }`
  - `{ "type": "swap", "signature": string, "slot": number, "side": "buy" | "sell", "amountIn": string, "amountOut": string, "trader": string }` (from program logs, decoded `SwapEvent`; amounts are atom strings).
- `POST /control` body `{ "publishing"?: boolean, "offsetBps"?: number, "offsetSeconds"?: number }` -> 200 with `{ "publishing": boolean, "offsetBps": number, "offsetSecondsLeft": number }`. `publishing=false` stops sending updates (the price goes stale). `offsetBps` (|x| <= 1000, 0 clears) adds a temporary offset to fair for `offsetSeconds` (default 10, max 300; simulated price jump; applied before skew and the band clamp, so the on-chain move limit still applies). `offsetSeconds` without `offsetBps` and out-of-range values answer 400 `{ "error": string }` and change nothing.
- `POST /faucet` body `{ "owner": base58 }` -> creates the owner's ATAs for both mints, mints 50 base + 10,000 quote (UI units), airdrops 1 SOL for fees. Enabled only with `--faucet` and only when `rpcUrl` is localhost; otherwise 403 `{ "error": string }`. Success: 200 `{ "signature": string, "airdropSignature": string }`; an invalid `owner` is 400, a chain failure is 502.

The server refuses any bind address other than 127.0.0.1. `WS /stream` rejects a browser `Origin` outside the two CORS origins (403). Every route returns 403 unless the `Host` header is exactly `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding defense). A client that falls 256 messages behind or stalls for 5 s is disconnected; reconnect to resume.

## Web (`web/`)
bun + Vite + React + TypeScript (strict) + Biome + lightweight-charts + `@solana/kit`. Dev server on 5173. Burner keypair per session. Expects the bot at `http://127.0.0.1:8788` (override with `VITE_BOT_URL`) and the validator at the config's `rpcUrl`/`wsUrl`. Browser E2E: `web/e2e/demo.ts` (`make demo-e2e`).

Swap transaction accounts (order): trader (signer), market (w), baseMint, quoteMint, baseVault (w), quoteVault (w), traderBase ATA (w), traderQuote ATA (w), tokenProgram. Data: `swapIxData(side, amountIn, minOut)`.
