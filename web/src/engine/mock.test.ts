import { describe, expect, test } from "bun:test";
import { decodeMarket, isFresh, ladder, priceToUi, quoteExactIn, swapIxData } from "./mock";
import { OFFSET } from "./types";

// SOL/USDC at $200, 1 SOL per level, 4 levels, 2 bps half spread, 1 bps step (same as the Rust ladder tests).
function market(
  over: { askConsumed?: bigint; bidConsumed?: bigint; mid?: bigint } = {},
): Uint8Array {
  const data = new Uint8Array(OFFSET.size);
  const v = new DataView(data.buffer);
  v.setBigUint64(OFFSET.midPrice, over.mid ?? 200_000_000_000n, true);
  v.setBigUint64(OFFSET.lastUpdateSlot, 100n, true);
  v.setBigUint64(OFFSET.askConsumed, over.askConsumed ?? 0n, true);
  v.setBigUint64(OFFSET.bidConsumed, over.bidConsumed ?? 0n, true);
  v.setBigUint64(OFFSET.levelSize, 1_000_000_000n, true);
  v.setBigUint64(OFFSET.maxAgeSlots, 20n, true);
  v.setUint16(OFFSET.halfSpreadBps, 2, true);
  v.setUint16(OFFSET.levelStepBps, 1, true);
  v.setUint16(OFFSET.maxMoveBps, 10, true);
  v.setUint8(OFFSET.numLevels, 4);
  return data;
}

describe("mock engine", () => {
  test("decodes fields", () => {
    const m = decodeMarket(market({ askConsumed: 5n }));
    expect(m.midPrice).toBe(200_000_000_000n);
    expect(m.askConsumed).toBe(5n);
    expect(m.numLevels).toBe(4);
    expect(m.paused).toBe(false);
  });

  test("ladder prices match the pricing crate", () => {
    const asks = ladder(market(), "buy").map((l) => l.price);
    expect(asks).toEqual([200_040_000_000n, 200_060_000_000n, 200_080_000_000n, 200_100_000_000n]);
    expect(ladder(market(), "sell")[0]?.price).toBe(199_960_000_000n);
  });

  test("ladder remaining follows consumed", () => {
    const rem = ladder(market({ askConsumed: 1_500_000_000n }), "buy").map((l) => l.remaining);
    expect(rem).toEqual([0n, 500_000_000n, 1_000_000_000n, 1_000_000_000n]);
  });

  test("buy inside level 0", () => {
    // 200.04 USDC buys exactly 1 SOL at level 0.
    const q = quoteExactIn(market(), "buy", 200_040_000n);
    expect(q.amountOut).toBe(1_000_000_000n);
    expect(q.consumedAfter).toBe(1_000_000_000n);
  });

  test("sell crosses a level boundary", () => {
    const q = quoteExactIn(market(), "sell", 1_500_000_000n);
    // 1 SOL at 199.96 plus 0.5 SOL at 199.94.
    expect(q.amountOut).toBe(199_960_000n + 99_970_000n);
    expect(q.consumedAfter).toBe(1_500_000_000n);
  });

  test("errors carry the pricing error name", () => {
    expect(() => quoteExactIn(market(), "buy", 0n)).toThrow("ZeroInput");
    expect(() => quoteExactIn(market(), "sell", 5_000_000_000n)).toThrow("InsufficientDepth");
    expect(() => quoteExactIn(market(), "sell", 1n)).toThrow("ZeroOutput");
    expect(() => quoteExactIn(market({ mid: 0n }), "buy", 1n)).toThrow("InvalidParams");
  });

  test("freshness boundary", () => {
    expect(isFresh(120n, 100n, 20n)).toBe(true);
    expect(isFresh(121n, 100n, 20n)).toBe(false);
  });

  test("swap data layout", () => {
    const d = swapIxData("sell", 5n, 7n);
    expect(d.length).toBe(25);
    expect(d[8]).toBe(1);
    expect(new DataView(d.buffer).getBigUint64(9, true)).toBe(5n);
    expect(new DataView(d.buffer).getBigUint64(17, true)).toBe(7n);
  });

  test("priceToUi", () => {
    expect(priceToUi(200_000_000_000n, 9, 6)).toBeCloseTo(200, 9);
  });
});
