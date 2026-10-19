// Mirrors the WASM module API in docs/demo-contract.md. Change the contract first.

export type Side = "buy" | "sell"; // trader's view: buy = pay quote, receive base

export interface MarketView {
  admin: string;
  updater: string;
  baseMint: string;
  quoteMint: string;
  baseVault: string;
  quoteVault: string;
  midPrice: bigint;
  lastUpdateSlot: bigint;
  windowStartMid: bigint;
  askConsumed: bigint;
  bidConsumed: bigint;
  levelSize: bigint;
  maxAgeSlots: bigint;
  halfSpreadBps: number;
  levelStepBps: number;
  maxMoveBps: number;
  numLevels: number;
  paused: boolean;
}

export interface Level {
  price: bigint;
  remaining: bigint;
}

export interface Engine {
  decodeMarket(data: Uint8Array): MarketView;
  quoteExactIn(
    data: Uint8Array,
    side: Side,
    amountIn: bigint,
  ): { amountOut: bigint; consumedAfter: bigint };
  isFresh(currentSlot: bigint, lastUpdateSlot: bigint, maxAgeSlots: bigint): boolean;
  swapIxData(side: Side, amountIn: bigint, minOut: bigint): Uint8Array;
  ladder(data: Uint8Array, side: Side): Level[];
  priceToUi(priceScaled: bigint, baseDecimals: number, quoteDecimals: number): number;
}

export type EngineKind = "wasm" | "mock";

/** Byte offsets of the Market account (8-byte discriminator + MarketState). */
export const OFFSET = {
  midPrice: 200,
  lastUpdateSlot: 208,
  askConsumed: 216,
  bidConsumed: 224,
  levelSize: 232,
  maxAgeSlots: 240,
  halfSpreadBps: 248,
  levelStepBps: 250,
  maxMoveBps: 252,
  numLevels: 254,
  paused: 255,
  windowStartMid: 264,
  size: 272,
} as const;
