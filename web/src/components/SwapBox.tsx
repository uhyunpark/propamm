import { useMemo, useState } from "react";
import type { Engine, Side } from "../engine/engine";
import type { SwapOutcome, Swapper } from "../hooks/useSwapper";
import type { Wallet } from "../hooks/useWallet";
import type { BotConfig, Tick } from "../lib/api";
import { describeErrorName, errorMessage, explainError } from "../lib/errors";
import { formatBps, formatUnits, formatUsd, parseUnits, shortKey } from "../lib/format";
import { averagePrice, costBps, minOutFor, summarizeQuote } from "../lib/quote";

interface Props {
  config: BotConfig;
  engine: Engine;
  data: Uint8Array | null;
  tick: Tick | null;
  wallet: Wallet;
  swapper: Swapper;
}

type QuoteState =
  | { ok: true; out: bigint; avg: number; cost: number | null; levels: number; depthLeft: bigint }
  | { ok: false; error: string | null };

export function SwapBox({ config, engine, data, tick, wallet, swapper }: Props) {
  const [side, setSide] = useState<Side>("buy");
  const [amountText, setAmountText] = useState("1000");
  const [slippageBps, setSlippageBps] = useState(50);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<SwapOutcome | null>(null);

  const inDecimals = side === "buy" ? config.quoteDecimals : config.baseDecimals;
  const outDecimals = side === "buy" ? config.baseDecimals : config.quoteDecimals;
  const inLabel = side === "buy" ? "USDC" : "SOL";
  const outLabel = side === "buy" ? "SOL" : "USDC";
  const amountIn = parseUnits(amountText, inDecimals);
  const onChainMid = tick?.chain?.mid ?? null;

  const quote = useMemo<QuoteState>(() => {
    if (!data || amountIn === null || amountIn === 0n) return { ok: false, error: null };
    try {
      const q = summarizeQuote(engine, data, side, amountIn);
      const avg = averagePrice(
        side,
        amountIn,
        q.amountOut,
        config.baseDecimals,
        config.quoteDecimals,
      );
      return {
        ok: true,
        out: q.amountOut,
        avg,
        cost: onChainMid ? costBps(side, avg, onChainMid) : null,
        levels: q.levelsCrossed,
        depthLeft: q.depthLeft,
      };
    } catch (e) {
      return { ok: false, error: describeErrorName(errorMessage(e)) };
    }
  }, [engine, data, side, amountIn, onChainMid, config]);

  const stale = tick?.chain ? tick.chain.ageSlots > tick.chain.maxAgeSlots : false;
  const minOut = quote.ok ? minOutFor(quote.out, slippageBps) : null;

  const run = async () => {
    if (amountIn === null) return;
    setBusy(true);
    setMessage(null);
    setOutcome(null);
    try {
      setOutcome(await swapper(side, amountIn, slippageBps));
    } catch (e) {
      setMessage(explainError(e));
    } finally {
      setBusy(false);
    }
  };

  const faucet = async () => {
    setBusy(true);
    setMessage(null);
    try {
      await wallet.faucet();
      setMessage("faucet 완료: SOL, USDC를 받았습니다.");
    } catch (e) {
      setMessage(`faucet 실패: ${errorMessage(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const { balances } = wallet;
  const fmt = (v: bigint | null, d: number, f: number) => (v === null ? "-" : formatUnits(v, d, f));

  return (
    <section className="card" aria-labelledby="swap-title">
      <h2 id="swap-title">스왑</h2>
      <fieldset className="seg">
        <legend className="sr-only">방향</legend>
        {(["buy", "sell"] as const).map((s) => (
          <button
            key={s}
            type="button"
            aria-pressed={side === s}
            className={side === s ? "on" : ""}
            onClick={() => {
              setSide(s);
              setOutcome(null);
            }}
          >
            {s === "buy" ? "SOL 사기" : "SOL 팔기"}
          </button>
        ))}
      </fieldset>

      <label className="field">
        <span>보낼 수량 ({inLabel})</span>
        <input
          inputMode="decimal"
          value={amountText}
          onChange={(e) => setAmountText(e.target.value)}
          aria-invalid={amountIn === null}
        />
      </label>
      <label className="field inline">
        <span>슬리피지 허용 (bps)</span>
        <input
          type="number"
          min={0}
          max={1000}
          value={slippageBps}
          onChange={(e) => setSlippageBps(Math.min(1000, Math.max(0, Number(e.target.value) || 0)))}
        />
      </label>

      <div className="quote" aria-live="polite">
        {quote.ok ? (
          <dl className="rows">
            <div>
              <dt>받을 수량</dt>
              <dd className="num">
                {formatUnits(quote.out, outDecimals, 6)} {outLabel}
              </dd>
            </div>
            <div>
              <dt>평균 가격</dt>
              <dd className="num">{formatUsd(quote.avg)}</dd>
            </div>
            <div>
              <dt>기준 가격 대비 비용</dt>
              <dd className="num">{quote.cost === null ? "-" : formatBps(quote.cost)}</dd>
            </div>
            <div>
              <dt>지나가는 호가 레벨</dt>
              <dd className="num">{quote.levels}개</dd>
            </div>
            <div>
              <dt>최소 수령량</dt>
              <dd className="num">
                {minOut === null ? "-" : formatUnits(minOut, outDecimals, 6)} {outLabel}
              </dd>
            </div>
            <div>
              <dt>이 방향 남은 깊이</dt>
              <dd className="num">{formatUnits(quote.depthLeft, config.baseDecimals, 3)} SOL</dd>
            </div>
          </dl>
        ) : (
          <p className="muted">
            {quote.error ??
              (amountIn === null ? "수량을 올바르게 입력하세요." : "마켓 계정을 기다리는 중...")}
          </p>
        )}
        {stale ? (
          <p className="warn-text">
            가격이 오래되어 스왑이 거부됩니다 (StalePrice). 견적은 계정 데이터 기준입니다.
          </p>
        ) : null}
      </div>

      <div className="actions">
        <button
          type="button"
          onClick={run}
          disabled={busy || !quote.ok || !wallet.ata}
          className="primary"
        >
          {busy ? "처리 중..." : "스왑 실행"}
        </button>
        <button type="button" onClick={faucet} disabled={busy || !wallet.signer}>
          faucet 받기
        </button>
      </div>

      {message ? (
        <p className="error-text" role="alert">
          {message}
        </p>
      ) : null}
      {outcome ? (
        <p className="result" role="status">
          실제 받은 양{" "}
          <b className="num">
            {formatUnits(outcome.actualOut, outDecimals, 6)} {outLabel}
          </b>{" "}
          {outcome.actualOut === outcome.expectedOut ? (
            <span className="badge ok">견적 = 실제 ✓</span>
          ) : (
            <span className="badge bad">
              견적 ≠ 실제 ✗ (견적 {formatUnits(outcome.expectedOut, outDecimals, 6)}, 그 사이 가격이
              갱신됐을 수 있습니다)
            </span>
          )}
        </p>
      ) : null}

      <p className="muted small">
        지갑(burner) <code>{wallet.signer ? shortKey(wallet.signer.address) : "-"}</code> · SOL{" "}
        <span className="num">{fmt(balances.sol, 9, 4)}</span> · SOL 토큰{" "}
        <span className="num">{fmt(balances.base, config.baseDecimals, 4)}</span> · USDC{" "}
        <span className="num">{fmt(balances.quote, config.quoteDecimals, 2)}</span>
      </p>
    </section>
  );
}
