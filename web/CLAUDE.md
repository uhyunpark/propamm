# web

- Run: `make wasm` first (generates `src/wasm/`, gitignored), then `bun install && bun run dev` (port 5173).
- Gate: `bun run check` (biome + tsc), `bun run test` (pure logic), `bun run build`.
- The interface contract (WASM API, bot server API, swap account order) lives in `docs/demo-contract.md`. Change it there first.
- `src/engine/engine.ts` loads `src/wasm/propamm_wasm.js` via `import.meta.glob`; if absent it falls back to `src/engine/mock.ts` (BigInt port of the ladder math). The header shows "mock 엔진" in that case.
- Program error names in `src/lib/errors.ts` follow `programs/propamm/src/error.rs` (codes from 6000).
- State lives in hooks: `useBotStream`, `useMarketAccount`, `useWallet`.
- `e2e/demo.ts`: browser E2E with `puppeteer-core` and the system Chrome against the running stack (`make demo-e2e`); writes `target/e2e/*.png`. It reads the chart's `data-series` attribute (set in `PricePanel`).
- UI terms: 공정 가격 = the bot's computed price (`tick.fair`), 기준 가격 (온체인) = the mid the maker wrote on chain (`tick.chain.mid`). Swap cost is measured against the on-chain mid.
