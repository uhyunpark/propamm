import { describe, expect, test } from "bun:test";
import { formatBps, formatUnits, formatUsd, gapBps, parseUnits, shortKey } from "./format";

describe("format", () => {
  test("parseUnits", () => {
    expect(parseUnits("1.5", 9)).toBe(1_500_000_000n);
    expect(parseUnits("0.000001", 6)).toBe(1n);
    expect(parseUnits(".5", 6)).toBe(500_000n);
    expect(parseUnits("1.1234567", 6)).toBeNull();
    expect(parseUnits("", 6)).toBeNull();
    expect(parseUnits("abc", 6)).toBeNull();
    expect(parseUnits("-1", 6)).toBeNull();
  });

  test("formatUnits", () => {
    expect(formatUnits(1_500_000_000n, 9)).toBe("1.5");
    expect(formatUnits(1_234_567_890_000n, 9, 3)).toBe("1,234.567");
    expect(formatUnits(0n, 6)).toBe("0");
    expect(formatUnits(-250_000n, 6)).toBe("-0.25");
  });

  test("bps and price", () => {
    expect(gapBps(100.5, 100)).toBeCloseTo(50, 9);
    expect(formatBps(1.234)).toBe("1.23 bps");
    expect(formatBps(Number.NaN)).toBe("-");
    expect(formatUsd(200.04)).toBe("200.040");
    expect(formatUsd(null)).toBe("-");
  });

  test("shortKey", () => {
    expect(shortKey("ABCDEFGHIJKLMNOP")).toBe("ABCD...MNOP");
    expect(shortKey("abc")).toBe("abc");
  });
});
