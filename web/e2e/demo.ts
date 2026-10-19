// Browser E2E for the demo, driving the system Chrome (headless) with puppeteer-core.
// Needs the stack up (see README.md): scripts/localnet.sh, the bot with
// `--serve 127.0.0.1:8788 --faucet`, and `bun run dev` in web/. Run: `make demo-e2e`.
// Writes screenshots to target/e2e/ and exits non-zero when a check fails or the page throws.
import { mkdirSync } from "node:fs";
import puppeteer, { type Page } from "puppeteer-core";

const APP = process.env.DEMO_URL ?? "http://127.0.0.1:5173";
const BOT = process.env.VITE_BOT_URL ?? "http://127.0.0.1:8788";
const CHROME =
  process.env.CHROME_PATH ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const OUT = new URL("../../target/e2e/", import.meta.url).pathname;

const results: { name: string; ok: boolean; detail: string }[] = [];
const pageErrors: string[] = [];
const consoleErrors: string[] = [];

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Polls `fn` until it returns a truthy value or `ms` passes. */
async function waitFor<T>(
  label: string,
  fn: () => Promise<T | null | undefined | false>,
  ms = 15_000,
): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v as T;
    if (Date.now() > end) throw new Error(`timed out waiting for ${label}`);
    await sleep(250);
  }
}

async function check(name: string, fn: () => Promise<string>): Promise<void> {
  try {
    const detail = await fn();
    results.push({ name, ok: true, detail });
    console.log(`PASS ${name}: ${detail}`);
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    results.push({ name, ok: false, detail });
    console.log(`FAIL ${name}: ${detail}`);
  }
}

const num = (s: string | null): number | null => {
  const m = s?.replace(/,/g, "").match(/-?\d+(\.\d+)?/);
  return m ? Number(m[0]) : null;
};

/** Text of the `dd` after the `dt` that starts with `label`, inside `scope`. */
const stat = (page: Page, scope: string, label: string) =>
  page.evaluate(
    (scope, label) => {
      const dts = document.querySelectorAll(`${scope} dt`);
      for (const dt of dts) {
        if (dt.textContent?.trim().startsWith(label)) {
          return dt.nextElementSibling?.textContent?.trim() ?? null;
        }
      }
      return null;
    },
    scope,
    label,
  );

const text = (page: Page, selector: string) =>
  page.evaluate((s) => document.querySelector(s)?.textContent?.trim() ?? null, selector);

/** Clicks the enabled button whose text starts with `label`. */
async function click(page: Page, label: string): Promise<void> {
  await waitFor(`button "${label}"`, () =>
    page.evaluate((label) => {
      const b = [...document.querySelectorAll("button")].find((x) =>
        x.textContent?.trim().startsWith(label),
      );
      if (!b || b.disabled) return false;
      b.click();
      return true;
    }, label),
  );
}

/** Sets the input inside the `label.field` whose caption starts with `caption` (React-safe). */
async function fill(page: Page, caption: string, value: string): Promise<void> {
  const ok = await page.evaluate(
    (caption, value) => {
      const field = [...document.querySelectorAll("label.field")].find((l) =>
        l.querySelector("span")?.textContent?.trim().startsWith(caption),
      );
      const input = field?.querySelector("input");
      if (!input) return false;
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setter?.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return true;
    },
    caption,
    value,
  );
  if (!ok) throw new Error(`input "${caption}" not found`);
}

/** Price age from the header, `[age, maxAge]`. */
async function age(page: Page): Promise<[number, number] | null> {
  const t = await text(page, ".slotinfo .age b");
  const m = t?.match(/(\d+)\s*\/\s*(\d+)/);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

const PRICE = "section[aria-labelledby=price-title]";
const SWAP = "section[aria-labelledby=swap-title]";
const SCN = "section[aria-labelledby=scn-title]";

/** One swap through the UI. Returns the badge text, or throws with the error shown. */
async function swapOnce(page: Page): Promise<{ ok: boolean; text: string }> {
  // Send right after a price update landed (age 1 is the minimum the stream shows): the swap
  // then usually executes before the bot's next update can move the price under the quote.
  await waitFor("a fresh price update", async () => ((await age(page))?.[0] ?? 99) <= 1, 20_000);
  await click(page, "스왑 실행");
  return waitFor(
    "swap result",
    () =>
      page.evaluate((scope) => {
        const root = document.querySelector(scope);
        const badge = root?.querySelector(".result .badge");
        if (badge) {
          const text = root?.querySelector(".result")?.textContent ?? "";
          return { ok: badge.classList.contains("ok"), text };
        }
        const err = root?.querySelector(".error-text")?.textContent;
        if (err) return { ok: false, text: `error: ${err}` };
        return null;
      }, SWAP),
    30_000,
  );
}

/** Swap with the current form; one retry when a price update raced the quote. */
async function swapMatches(page: Page, side: "SOL 사기" | "SOL 팔기", amount: string) {
  await click(page, side);
  await fill(page, "보낼 수량", amount);
  const quoted = await waitFor("WASM quote", () => stat(page, SWAP, "받을 수량"));
  for (let attempt = 1; attempt <= 2; attempt++) {
    const r = await swapOnce(page);
    if (r.ok) return `quote ${quoted}, ${r.text.trim()} (attempt ${attempt})`;
    if (r.text.startsWith("error:")) throw new Error(r.text);
    console.log(`  retrying: ${r.text.trim()}`);
  }
  throw new Error("quote != actual twice in a row");
}

async function control(body: object): Promise<void> {
  await fetch(`${BOT}/control`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  }).catch(() => undefined);
}

async function shoot(page: Page, name: string, scheme: "light" | "dark"): Promise<void> {
  await page.emulateMediaFeatures([{ name: "prefers-color-scheme", value: scheme }]);
  await sleep(800); // chart repaint and a few ticks
  await page.screenshot({ path: `${OUT}${name}.png`, fullPage: true });
}

async function main(): Promise<void> {
  mkdirSync(OUT, { recursive: true });
  const browser = await puppeteer.launch({ executablePath: CHROME, headless: true });
  try {
    const page = await browser.newPage();
    await page.setViewport({ width: 1440, height: 900 });
    await page.emulateMediaFeatures([{ name: "prefers-color-scheme", value: "light" }]);
    page.on("pageerror", (e) => pageErrors.push(e instanceof Error ? e.message : String(e)));
    page.on("console", (m) => {
      if (m.type() === "error") consoleErrors.push(m.text());
    });
    await page.goto(APP, { waitUntil: "domcontentloaded" });

    await check("live data", async () => {
      const pills = await waitFor("header pills OK", async () => {
        const p = await page.evaluate(() =>
          [...document.querySelectorAll(".pills .pill")].map((x) => ({
            ok: x.classList.contains("ok"),
            text: x.textContent?.trim() ?? "",
          })),
        );
        return p.length === 3 && p.every((x) => x.ok) ? p : null;
      });
      if (!pills.some((p) => p.text === "WASM")) throw new Error("engine pill is not WASM");
      const fair = await waitFor("fair", async () => num(await stat(page, PRICE, "공정 가격")));
      const mid = await waitFor("on-chain mid", async () =>
        num(await stat(page, PRICE, "기준 가격 (온체인)")),
      );
      const series = await waitFor("chart series", async () => {
        const n = Number(await page.$eval(".chart", (el) => el.getAttribute("data-series") ?? 0));
        return n >= 3 ? n : null;
      });
      return `pills ${pills.map((p) => p.text).join("/")}, fair ${fair}, mid ${mid}, ${series} series`;
    });

    await check("faucet", async () => {
      await click(page, "faucet 받기");
      const line = await waitFor(
        "balances",
        async () => {
          const t = await text(page, `${SWAP} p.small`);
          return t && /USDC\s+10,000/.test(t) && /SOL 토큰\s+50/.test(t) ? t : null;
        },
        30_000,
      );
      return line.replace(/\s+/g, " ");
    });

    await check("buy (quote = actual)", () => swapMatches(page, "SOL 사기", "500"));

    await check("timeline markers", async () => {
      const counts = await waitFor(
        "update and swap markers",
        async () => {
          const c = await page.evaluate(() => ({
            upd: document.querySelectorAll(".timeline .mark.upd").length,
            swp: document.querySelectorAll(".timeline .mark.swp").length,
          }));
          return c.upd > 0 && c.swp > 0 ? c : null;
        },
        10_000,
      );
      return `${counts.upd} update, ${counts.swp} swap markers`;
    });

    await check("sell (quote = actual)", () => swapMatches(page, "SOL 팔기", "2"));

    await check("stop bot -> StalePrice", async () => {
      await click(page, "봇 멈추기");
      const [a, max] = await waitFor(
        "price age > max age",
        async () => {
          const v = await age(page);
          return v && v[0] > v[1] ? v : null;
        },
        30_000,
      );
      await click(page, "스왑 실행"); // clears the previous result before sending
      const msg = await waitFor("error message", () => text(page, `${SWAP} .error-text`), 30_000);
      if (!msg.includes("StalePrice")) throw new Error(`expected StalePrice, got: ${msg}`);
      return `age ${a}/${max}, shown: ${msg}`;
    });

    await check("resume", async () => {
      await click(page, "다시 시작");
      const [a, max] = await waitFor(
        "fresh price",
        async () => {
          const v = await age(page);
          return v && v[0] <= 3 ? v : null;
        },
        30_000,
      );
      const swap = await swapMatches(page, "SOL 사기", "200");
      return `age back to ${a}/${max}; swap after resume: ${swap}`;
    });

    await check("price jump +50bps", async () => {
      const before = await waitFor("mid", async () =>
        num(await stat(page, PRICE, "기준 가격 (온체인)")),
      );
      await click(page, "가격 급변 +50bps");
      let best = 0;
      const end = Date.now() + 8_000;
      while (Date.now() < end) {
        const mid = num(await stat(page, PRICE, "기준 가격 (온체인)"));
        if (mid !== null) best = Math.max(best, ((mid - before) / before) * 10_000);
        if (best >= 30) break;
        await sleep(250);
      }
      if (best < 20) throw new Error(`on-chain mid rose only ${best.toFixed(1)} bps`);
      const label = await page.evaluate(
        (scope) => document.querySelector(`${scope} dt`)?.textContent ?? "",
        PRICE,
      );
      return `on-chain mid +${best.toFixed(1)} bps (from ${before}); fair label "${label}"`;
    });

    await check("order splitting", async () => {
      const single = await waitFor("single", () => stat(page, SCN, "한 번에 사기"));
      const split = await waitFor("split", () => stat(page, SCN, "10번 나눠 사기"));
      const diff = BigInt((await stat(page, SCN, "차이"))?.match(/-?\d+/)?.[0] ?? "1");
      if (diff > 0n) throw new Error(`split ${split} > single ${single}`);
      await click(page, "주문 쪼개기 시연");
      const msg = await waitFor(
        "split execution",
        async () => {
          // The split status line is the only unstyled status paragraph in the card.
          const t = await text(page, `${SCN} p[role=status]:not([class])`);
          return t && !t.includes("실행 중") ? t : null;
        },
        90_000,
      );
      if (!msg.includes("합계")) throw new Error(`split execution failed: ${msg}`);
      return `preview single ${single}, split ${split}, diff ${diff} atom; executed: ${msg}`;
    });

    await check("timeline after scenarios", async () => {
      const c = await page.evaluate(() => ({
        upd: document.querySelectorAll(".timeline .mark.upd").length,
        swp: document.querySelectorAll(".timeline .mark.swp").length,
        stale: document.querySelectorAll(".timeline .slot.stale").length,
      }));
      if (c.upd === 0 || c.swp === 0) throw new Error(JSON.stringify(c));
      return `${c.upd} update, ${c.swp} swap markers in the last 40 slots`;
    });

    await check("screenshots", async () => {
      await shoot(page, "desktop-light", "light");
      await shoot(page, "desktop-dark", "dark");
      // Width only: changing isMobile/hasTouch makes puppeteer reload the page and lose its state.
      await page.setViewport({ width: 390, height: 844 });
      await shoot(page, "mobile-light", "light");
      await shoot(page, "mobile-dark", "dark");
      return "desktop-{light,dark}.png, mobile-{light,dark}.png";
    });
  } finally {
    await control({ publishing: true }); // never leave the bot paused
    await browser.close();
  }
}

await main().catch((e) => {
  results.push({ name: "run", ok: false, detail: String(e) });
  console.log(`FAIL run: ${e}`);
});
if (consoleErrors.length) console.log(`console errors:\n  ${consoleErrors.join("\n  ")}`);
if (pageErrors.length) console.log(`uncaught page errors:\n  ${pageErrors.join("\n  ")}`);
const failed = results.filter((r) => !r.ok).length + pageErrors.length;
console.log(failed ? `${failed} failure(s)` : `all ${results.length} checks passed`);
process.exit(failed ? 1 : 0);
