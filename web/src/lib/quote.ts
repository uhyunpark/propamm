import type { Engine, MarketView, Side } from "../engine/engine";
import { OFFSET } from "../engine/types";

export interface QuoteSummary {
  amountOut: bigint;
  consumedAfter: bigint;
  levelsCrossed: number;
  depthLeft: bigint;
}

/** Number of ladder levels a fill touches, given consumed before and after. */
export function levelsTouched(before: bigint, after: bigint, levelSize: bigint): number {
  if (after <= before || levelSize === 0n) return 0;
  return Number((after + levelSize - 1n) / levelSize - before / levelSize);
}

export function consumedOf(m: MarketView, side: Side): bigint {
  return side === "buy" ? m.askConsumed : m.bidConsumed;
}

export function summarizeQuote(
  engine: Engine,
  data: Uint8Array,
  side: Side,
  amountIn: bigint,
): QuoteSummary {
  const m = engine.decodeMarket(data);
  const q = engine.quoteExactIn(data, side, amountIn);
  const depthLeft = engine.ladder(data, side).reduce((sum, l) => sum + l.remaining, 0n);
  return {
    amountOut: q.amountOut,
    consumedAfter: q.consumedAfter,
    levelsCrossed: levelsTouched(consumedOf(m, side), q.consumedAfter, m.levelSize),
    depthLeft,
  };
}

/** Average execution price in UI quote units per UI base unit. */
export function averagePrice(
  side: Side,
  amountIn: bigint,
  amountOut: bigint,
  baseDecimals: number,
  quoteDecimals: number,
): number {
  const quote = Number(side === "buy" ? amountIn : amountOut) / 10 ** quoteDecimals;
  const base = Number(side === "buy" ? amountOut : amountIn) / 10 ** baseDecimals;
  return quote / base;
}

/** Cost versus the reference price in bps (positive = worse for the trader). */
export function costBps(side: Side, avg: number, ref: number): number {
  return side === "buy" ? (avg / ref - 1) * 10_000 : (1 - avg / ref) * 10_000;
}

export function minOutFor(amountOut: bigint, slippageBps: number): bigint {
  return (amountOut * BigInt(10_000 - slippageBps)) / 10_000n;
}

export interface SplitResult {
  single: bigint;
  split: bigint;
  parts: bigint[];
}

/**
 * Compares one big exact-in quote with `n` equal parts quoted in sequence.
 * Each part starts from the consumed counter the previous part left behind (written into a copy).
 */
export function compareSplit(
  engine: Engine,
  data: Uint8Array,
  side: Side,
  total: bigint,
  n: number,
): SplitResult {
  const single = engine.quoteExactIn(data, side, total).amountOut;
  const copy = new Uint8Array(data);
  const view = new DataView(copy.buffer);
  const offset = side === "buy" ? OFFSET.askConsumed : OFFSET.bidConsumed;
  const part = total / BigInt(n);
  const parts: bigint[] = [];
  let sum = 0n;
  for (let i = 0; i < n; i++) {
    const amount = i === n - 1 ? total - part * BigInt(n - 1) : part;
    const q = engine.quoteExactIn(copy, side, amount);
    parts.push(q.amountOut);
    sum += q.amountOut;
    view.setBigUint64(offset, q.consumedAfter, true);
  }
  return { single, split: sum, parts };
}
