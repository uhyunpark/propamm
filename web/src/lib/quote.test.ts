import { describe, expect, test } from "bun:test";
import { mockEngine } from "../engine/mock";
import { OFFSET } from "../engine/types";
import { explainError, findCustomCode, programErrorName } from "./errors";
import { averagePrice, compareSplit, costBps, levelsTouched, minOutFor } from "./quote";

function market(): Uint8Array {
  const data = new Uint8Array(OFFSET.size);
  const v = new DataView(data.buffer);
  v.setBigUint64(OFFSET.midPrice, 200_000_000_000n, true);
  v.setBigUint64(OFFSET.levelSize, 1_000_000_000n, true);
  v.setBigUint64(OFFSET.maxAgeSlots, 20n, true);
  v.setUint16(OFFSET.halfSpreadBps, 2, true);
  v.setUint16(OFFSET.levelStepBps, 1, true);
  v.setUint8(OFFSET.numLevels, 4);
  return data;
}

describe("quote helpers", () => {
  test("levelsTouched", () => {
    const size = 100n;
    expect(levelsTouched(0n, 50n, size)).toBe(1);
    expect(levelsTouched(0n, 100n, size)).toBe(1);
    expect(levelsTouched(0n, 101n, size)).toBe(2);
    expect(levelsTouched(150n, 250n, size)).toBe(2);
    expect(levelsTouched(10n, 10n, size)).toBe(0);
  });

  test("average price and cost", () => {
    const avg = averagePrice("buy", 200_040_000n, 1_000_000_000n, 9, 6);
    expect(avg).toBeCloseTo(200.04, 9);
    expect(costBps("buy", avg, 200)).toBeCloseTo(2, 6);
    expect(costBps("sell", 199.96, 200)).toBeCloseTo(2, 6);
  });

  test("minOut", () => {
    expect(minOutFor(10_000n, 50)).toBe(9_950n);
  });

  test("splitting never beats one big order", () => {
    const r = compareSplit(mockEngine, market(), "buy", 500_000_000n, 10);
    expect(r.parts.length).toBe(10);
    expect(r.split <= r.single).toBe(true);
    expect(r.single - r.split <= 20n).toBe(true);
  });
});

describe("errors", () => {
  test("code table", () => {
    expect(programErrorName(6000)).toBe("Unauthorized");
    expect(programErrorName(6014)).toBe("StalePrice");
    expect(programErrorName(6016)).toBe("InsufficientVaultBalance");
    expect(programErrorName(6017)).toBeNull();
  });

  test("finds codes in nested errors and logs", () => {
    expect(findCustomCode({ cause: { context: { code: 6014 } } })).toBe(6014);
    expect(findCustomCode({ cause: { InstructionError: [0, { Custom: 6014 }] } })).toBe(6014);
    expect(findCustomCode(new Error("custom program error: 0x177e"))).toBe(6014);
    expect(explainError({ cause: { Custom: 6014 } })).toContain("StalePrice");
  });
});
