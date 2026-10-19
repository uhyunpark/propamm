import type { BotConfig, SwapEvent } from "./api";

/** Parses a decimal string into atoms. Returns null for empty, invalid or too-precise input. */
export function parseUnits(text: string, decimals: number): bigint | null {
  const s = text.trim();
  if (!/^\d*\.?\d*$/.test(s) || s === "" || s === ".") return null;
  const [whole = "", frac = ""] = s.split(".");
  if (frac.length > decimals) return null;
  return BigInt(whole || "0") * 10n ** BigInt(decimals) + BigInt(frac.padEnd(decimals, "0") || "0");
}

/** Formats atoms as a decimal string with at most `maxFrac` fraction digits (truncated, trailing zeros trimmed). */
export function formatUnits(atoms: bigint, decimals: number, maxFrac = decimals): string {
  const negative = atoms < 0n;
  const abs = negative ? -atoms : atoms;
  const base = 10n ** BigInt(decimals);
  const whole = abs / base;
  const frac = (abs % base).toString().padStart(decimals, "0").slice(0, maxFrac).replace(/0+$/, "");
  const grouped = whole.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${negative ? "-" : ""}${grouped}${frac ? `.${frac}` : ""}`;
}

export function formatBps(bps: number, digits = 2): string {
  if (!Number.isFinite(bps)) return "-";
  return `${bps.toFixed(digits)} bps`;
}

export function formatUsd(value: number | null | undefined, digits = 3): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "-";
  return value.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

/** Signed gap of `value` relative to `ref` in bps. */
export function gapBps(value: number, ref: number): number {
  return ((value - ref) / ref) * 10_000;
}

/** "in -> out" of a swap event in UI units. */
export function swapAmounts(e: SwapEvent, config: BotConfig): string {
  const inDec = e.side === "buy" ? config.quoteDecimals : config.baseDecimals;
  const outDec = e.side === "buy" ? config.baseDecimals : config.quoteDecimals;
  return `${formatUnits(BigInt(e.amountIn), inDec, 4)} → ${formatUnits(BigInt(e.amountOut), outDec, 4)}`;
}

export function shortKey(key: string): string {
  return key.length > 10 ? `${key.slice(0, 4)}...${key.slice(-4)}` : key;
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
