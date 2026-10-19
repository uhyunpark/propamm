import type { BotConfig, BotEvent } from "../lib/api";
import { shortKey, swapAmounts } from "../lib/format";

export function EventLog({ events, config }: { events: BotEvent[]; config: BotConfig | null }) {
  const last = events.slice(-50).reverse();
  return (
    <details className="card">
      <summary>이벤트 로그 ({last.length})</summary>
      {last.length === 0 ? (
        <p className="muted">아직 이벤트가 없습니다.</p>
      ) : (
        <ol className="log">
          {last.map((e) => (
            <li key={`${e.type}-${e.signature}`}>
              <span className="num">
                {e.type === "update" ? (e.landedSlot ?? e.targetSlot) : e.slot}
              </span>{" "}
              {e.type === "update" ? (
                <>
                  갱신 <span className={`badge ${e.status}`}>{e.status}</span> 기준 가격{" "}
                  {e.mid.toFixed(3)}
                  {e.delaySlots !== null ? ` · 지연 ${e.delaySlots}슬롯` : ""}
                </>
              ) : (
                <>
                  스왑 {e.side === "buy" ? "사기" : "팔기"}{" "}
                  {config ? swapAmounts(e, config) : `${e.amountIn} → ${e.amountOut}`} (
                  {shortKey(e.trader)})
                </>
              )}{" "}
              <code>{shortKey(e.signature)}</code>
            </li>
          ))}
        </ol>
      )}
    </details>
  );
}
