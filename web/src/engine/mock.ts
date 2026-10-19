// TypeScript stand-in for the WASM module (same functions, BigInt ladder math).
// Port of crates/pricing/src/ladder.rs. Used only when src/wasm is not built.
import { getAddressDecoder } from "@solana/kit";
import { type Engine, type Level, type MarketView, OFFSET, type Side } from "./types";

const PRICE_SCALE = 1_000_000_000_000n;
const BPS = 10_000n;
const U64_MAX = (1n << 64n) - 1n;
const SWAP_DISCRIMINATOR = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

const addressDecoder = getAddressDecoder();

function view(data: Uint8Array): DataView {
  if (data.length < OFFSET.size) throw new Error("InvalidAccountData");
  return new DataView(data.buffer, data.byteOffset, data.byteLength);
}

function key(data: Uint8Array, start: number): string {
  return addressDecoder.decode(data.subarray(start, start + 32));
}

function ceilDiv(a: bigint, d: bigint): bigint {
  return (a + d - 1n) / d;
}

export function decodeMarket(data: Uint8Array): MarketView {
  const v = view(data);
  return {
    admin: key(data, 8),
    updater: key(data, 40),
    baseMint: key(data, 72),
    quoteMint: key(data, 104),
    baseVault: key(data, 136),
    quoteVault: key(data, 168),
    midPrice: v.getBigUint64(OFFSET.midPrice, true),
    lastUpdateSlot: v.getBigUint64(OFFSET.lastUpdateSlot, true),
    windowStartMid: v.getBigUint64(OFFSET.windowStartMid, true),
    askConsumed: v.getBigUint64(OFFSET.askConsumed, true),
    bidConsumed: v.getBigUint64(OFFSET.bidConsumed, true),
    levelSize: v.getBigUint64(OFFSET.levelSize, true),
    maxAgeSlots: v.getBigUint64(OFFSET.maxAgeSlots, true),
    halfSpreadBps: v.getUint16(OFFSET.halfSpreadBps, true),
    levelStepBps: v.getUint16(OFFSET.levelStepBps, true),
    maxMoveBps: v.getUint16(OFFSET.maxMoveBps, true),
    numLevels: v.getUint8(OFFSET.numLevels),
    paused: v.getUint8(OFFSET.paused) !== 0,
  };
}

function offsetBps(m: MarketView, level: number): bigint {
  return BigInt(m.halfSpreadBps) + BigInt(level) * BigInt(m.levelStepBps);
}

function askPrice(m: MarketView, level: number): bigint {
  return ceilDiv(m.midPrice * (BPS + offsetBps(m, level)), BPS);
}

function bidPrice(m: MarketView, level: number): bigint {
  return (m.midPrice * (BPS - offsetBps(m, level))) / BPS;
}

function levelPrice(m: MarketView, side: Side, level: number): bigint {
  return side === "buy" ? askPrice(m, level) : bidPrice(m, level);
}

function consumedOf(m: MarketView, side: Side): bigint {
  return side === "buy" ? m.askConsumed : m.bidConsumed;
}

function validate(m: MarketView): void {
  if (m.midPrice === 0n || m.levelSize === 0n || m.numLevels === 0) {
    throw new Error("InvalidParams");
  }
}

export function quoteExactIn(
  data: Uint8Array,
  side: Side,
  amountIn: bigint,
): { amountOut: bigint; consumedAfter: bigint } {
  const m = decodeMarket(data);
  validate(m);
  if (amountIn === 0n) throw new Error("ZeroInput");
  let consumed = consumedOf(m, side);
  let remaining = amountIn;
  let out = 0n;
  while (remaining > 0n) {
    const level = consumed / m.levelSize;
    if (level >= BigInt(m.numLevels)) throw new Error("InsufficientDepth");
    const price = levelPrice(m, side, Number(level));
    const avail = m.levelSize - (consumed % m.levelSize);
    let take: bigint;
    if (side === "buy") {
      const costFull = ceilDiv(avail * price, PRICE_SCALE);
      if (remaining >= costFull) {
        remaining -= costFull;
        take = avail;
        out += avail;
      } else {
        take = (remaining * PRICE_SCALE) / price;
        remaining = 0n;
        out += take;
      }
    } else {
      take = remaining < avail ? remaining : avail;
      out += (take * price) / PRICE_SCALE;
      remaining -= take;
    }
    consumed += take;
    // A zero-size take cannot advance, which would loop forever.
    if (take === 0n) break;
  }
  if (out === 0n) throw new Error("ZeroOutput");
  if (out > U64_MAX) throw new Error("MathOverflow");
  return { amountOut: out, consumedAfter: consumed };
}

export function isFresh(currentSlot: bigint, lastUpdateSlot: bigint, maxAgeSlots: bigint): boolean {
  const age = currentSlot > lastUpdateSlot ? currentSlot - lastUpdateSlot : 0n;
  return age <= maxAgeSlots;
}

export function swapIxData(side: Side, amountIn: bigint, minOut: bigint): Uint8Array {
  const out = new Uint8Array(25);
  out.set(SWAP_DISCRIMINATOR, 0);
  out[8] = side === "buy" ? 0 : 1;
  const v = new DataView(out.buffer);
  v.setBigUint64(9, amountIn, true);
  v.setBigUint64(17, minOut, true);
  return out;
}

export function ladder(data: Uint8Array, side: Side): Level[] {
  const m = decodeMarket(data);
  validate(m);
  const consumed = consumedOf(m, side);
  const levels: Level[] = [];
  for (let i = 0; i < m.numLevels; i++) {
    const start = BigInt(i) * m.levelSize;
    const end = start + m.levelSize;
    const remaining = consumed >= end ? 0n : consumed > start ? end - consumed : m.levelSize;
    levels.push({ price: levelPrice(m, side, i), remaining });
  }
  return levels;
}

export function priceToUi(
  priceScaled: bigint,
  baseDecimals: number,
  quoteDecimals: number,
): number {
  // quote atoms per base atom * 10^(baseDecimals - quoteDecimals) = quote units per base unit
  return (Number(priceScaled) / Number(PRICE_SCALE)) * 10 ** (baseDecimals - quoteDecimals);
}

export const mockEngine: Engine = {
  decodeMarket,
  quoteExactIn,
  isFresh,
  swapIxData,
  ladder,
  priceToUi,
};
