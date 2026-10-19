// Loads the generated WASM module in bun and checks it against the Rust fixtures
// (crates/wasm/tests/fixtures, verified by `cargo test -p propamm-wasm`). Run: `make wasm-test`.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import init, {
  decodeMarket,
  isFresh,
  ladder,
  priceToUi,
  quoteExactIn,
  swapIxData,
} from "../../../web/src/wasm/propamm_wasm.js";

const root = join(import.meta.dir, "..", "..", "..");
const fixtures = join(import.meta.dir, "fixtures");
await init({ module_or_path: readFileSync(join(root, "web/src/wasm/propamm_wasm_bg.wasm")) });

const data = Uint8Array.from(Buffer.from(readFileSync(join(fixtures, "market.hex"), "utf8").trim(), "hex"));
const golden = JSON.parse(readFileSync(join(fixtures, "golden.json"), "utf8")) as Record<
  "buy" | "sell",
  { amountIn: string; amountOut: string; consumedAfter: string }
>;

function eq(actual: unknown, expected: unknown, what: string) {
  if (actual !== expected) throw new Error(`${what}: got ${String(actual)}, expected ${String(expected)}`);
}

const m = decodeMarket(data);
eq(m.midPrice, 200_000_000_000n, "midPrice");
eq(m.askConsumed, 12_000_000_000n, "askConsumed");
eq(m.numLevels, 8, "numLevels");
eq(m.paused, false, "paused");
eq(typeof m.lastUpdateSlot, "bigint", "lastUpdateSlot type");

for (const side of ["buy", "sell"] as const) {
  const g = golden[side];
  const q = quoteExactIn(data, side, BigInt(g.amountIn));
  eq(q.amountOut, BigInt(g.amountOut), `${side} amountOut`);
  eq(q.consumedAfter, BigInt(g.consumedAfter), `${side} consumedAfter`);
}

const asks = ladder(data, "buy");
eq(asks.length, 8, "ladder length");
eq(asks[0].remaining, 0n, "ask level 0 remaining");
eq(asks[1].remaining, 8_000_000_000n, "ask level 1 remaining");

const ix = swapIxData("sell", 5n, 7n);
eq(ix.length, 25, "swap data length");
eq(ix[8], 1, "swap side byte");

eq(isFresh(120n, 100n, 20n), true, "isFresh");
eq(isFresh(121n, 100n, 20n), false, "isFresh stale");
eq(priceToUi(200_000_000_000n, 9, 6), 200, "priceToUi");

for (const [name, call, message] of [
  ["zero input", () => quoteExactIn(data, "buy", 0n), "ZeroInput"],
  ["depth", () => quoteExactIn(data, "sell", 1n << 60n), "InsufficientDepth"],
  ["short data", () => decodeMarket(new Uint8Array(3)), "WrongSize"],
] as const) {
  try {
    call();
  } catch (e) {
    eq(e instanceof Error, true, `${name} throws Error`);
    eq((e as Error).message, message, `${name} message`);
    continue;
  }
  throw new Error(`${name}: expected a throw`);
}
console.log("wasm smoke test ok");
