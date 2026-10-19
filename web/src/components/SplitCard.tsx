import { useMemo, useState } from "react";
import type { Engine, MarketView } from "../engine/engine";
import type { Swapper } from "../hooks/useSwapper";
import type { BotConfig } from "../lib/api";
import { errorMessage, explainError } from "../lib/errors";
import { formatUnits, parseUnits } from "../lib/format";
import { compareSplit } from "../lib/quote";

const PARTS = 10;

interface Props {
  config: BotConfig;
  engine: Engine;
  data: Uint8Array | null;
  view: MarketView | null;
  swapper: Swapper;
}

export function SplitCard({ config, engine, data, view, swapper }: Props) {
  const [splitText, setSplitText] = useState("");
  const [splitMsg, setSplitMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Default: about 2.5 ladder levels of base, priced at the mid, so the buy crosses levels.
  const defaultText = useMemo(() => {
    if (!view) return "";
    const quoteAtoms = (view.levelSize * 5n * view.midPrice) / 2n / 1_000_000_000_000n;
    return formatUnits(quoteAtoms, config.quoteDecimals, 2).replace(/,/g, "");
  }, [view, config.quoteDecimals]);
  const total = parseUnits(splitText || defaultText, config.quoteDecimals);

  const preview = useMemo(() => {
    if (!data || total === null || total < BigInt(PARTS)) return null;
    try {
      return { ok: true as const, ...compareSplit(engine, data, "buy", total, PARTS) };
    } catch (e) {
      return { ok: false as const, error: errorMessage(e) };
    }
  }, [engine, data, total]);

  const executeSplit = async () => {
    if (total === null || !preview?.ok) return;
    setBusy(true);
    setSplitMsg(null);
    try {
      const part = total / BigInt(PARTS);
      let sum = 0n;
      for (let i = 0; i < PARTS; i++) {
        const amount = i === PARTS - 1 ? total - part * BigInt(PARTS - 1) : part;
        sum += (await swapper("buy", amount, 100)).actualOut;
        setSplitMsg(`${i + 1} / ${PARTS} 실행 중...`);
        await new Promise((r) => setTimeout(r, 500)); // let the account update arrive
      }
      // Each price update resets the consumed counters. Within one update splitting never beats
      // the single quote (the preview above), so getting more means an update landed in between.
      const raced =
        sum > preview.single
          ? " · 한 번에 견적보다 많은 건 실행 사이에 가격 갱신이 들어왔기 때문입니다 (소비량 초기화, 가격 이동)"
          : "";
      setSplitMsg(
        `${PARTS}번 실행 합계 ${formatUnits(sum, config.baseDecimals, 6)} SOL (한 번에 견적 ${formatUnits(preview.single, config.baseDecimals, 6)} SOL)${raced}`,
      );
    } catch (e) {
      setSplitMsg(explainError(e));
    } finally {
      setBusy(false);
    }
  };

  const diff = preview?.ok ? preview.split - preview.single : 0n;

  return (
    <div className="scenario">
      <label className="field inline">
        <span>쪼갤 금액 (USDC)</span>
        <input
          inputMode="decimal"
          value={splitText}
          placeholder={defaultText}
          onChange={(e) => setSplitText(e.target.value)}
        />
      </label>
      {preview?.ok ? (
        <dl className="rows">
          <div>
            <dt>한 번에 사기</dt>
            <dd className="num">{formatUnits(preview.single, config.baseDecimals, 6)} SOL</dd>
          </div>
          <div>
            <dt>{PARTS}번 나눠 사기</dt>
            <dd className="num">{formatUnits(preview.split, config.baseDecimals, 6)} SOL</dd>
          </div>
          <div>
            <dt>차이</dt>
            <dd className="num">{diff.toString()} atom</dd>
          </div>
        </dl>
      ) : preview ? (
        <p className="muted small">견적 실패: {preview.error}</p>
      ) : null}
      <div className="btns">
        <button type="button" onClick={executeSplit} disabled={busy || !preview?.ok}>
          주문 쪼개기 시연 ({PARTS}번 실행)
        </button>
      </div>
      <p className="muted small">
        가격 갱신 사이에서는 쪼개도 이득이 없습니다. 먼저 먹은 호가는 소비되어 다음 레벨 가격으로
        넘어가므로, 나눠 사도 같은 호가를 같은 순서로 먹습니다. 차이는 올림 오차뿐이고, 올림은 마켓
        메이커에게 유리합니다. 갱신이 끼어들면 소비량이 초기화되어 결과가 달라질 수 있습니다.
      </p>
      {splitMsg ? <p role="status">{splitMsg}</p> : null}
    </div>
  );
}
