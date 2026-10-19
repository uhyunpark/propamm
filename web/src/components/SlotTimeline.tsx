import type { BotConfig, BotEvent } from "../lib/api";
import { swapAmounts } from "../lib/format";

const SLOTS = 40;

interface Props {
  config: BotConfig;
  currentSlot: number | null;
  maxAge: number | null;
  events: BotEvent[];
  slotAges: ReadonlyMap<number, number>;
  stale: boolean;
}

export function SlotTimeline({ config, currentSlot, maxAge, events, slotAges, stale }: Props) {
  if (currentSlot === null) {
    return (
      <section className="card" aria-labelledby="timeline-title">
        <h2 id="timeline-title">슬롯 타임라인</h2>
        <p className="muted">봇 서버를 기다리는 중...</p>
      </section>
    );
  }
  const first = currentSlot - SLOTS + 1;
  const cells = Array.from({ length: SLOTS }, (_, i) => first + i);
  const bySlot = new Map<number, BotEvent[]>();
  for (const e of events) {
    const slot = e.type === "update" ? (e.landedSlot ?? e.targetSlot) : e.slot;
    if (slot < first) continue;
    bySlot.set(slot, [...(bySlot.get(slot) ?? []), e]);
  }

  const describe = (e: BotEvent): string => {
    if (e.type === "update") {
      const delay = e.delaySlots === null ? "-" : `${e.delaySlots}슬롯`;
      return `가격 갱신 ${e.status} · 기준 가격 ${e.mid.toFixed(3)} · 지연 ${delay}`;
    }
    return `스왑 ${e.side === "buy" ? "사기" : "팔기"} · ${swapAmounts(e, config)}`;
  };

  return (
    <section className={`card${stale ? " greyed" : ""}`} aria-labelledby="timeline-title">
      <h2 id="timeline-title">슬롯 타임라인</h2>
      <ol className="timeline" aria-label="최근 40개 슬롯">
        {cells.map((slot) => {
          const age = slotAges.get(slot);
          const isStale = maxAge !== null && age !== undefined && age > maxAge;
          const items = bySlot.get(slot) ?? [];
          return (
            <li
              key={slot}
              className={`slot${isStale ? " stale" : ""}${slot === currentSlot ? " now" : ""}`}
              title={`슬롯 ${slot}${age !== undefined ? ` · 가격 나이 ${age}` : ""}${isStale ? " (오래됨)" : ""}`}
            >
              <div className="marks">
                {items.map((e) => (
                  <button
                    type="button"
                    key={`${e.type}-${e.signature}`}
                    className={e.type === "update" ? `mark upd ${e.status}` : `mark swp ${e.side}`}
                    title={describe(e)}
                    aria-label={describe(e)}
                  >
                    {e.type === "swap" ? (e.side === "buy" ? "B" : "S") : ""}
                  </button>
                ))}
              </div>
            </li>
          );
        })}
      </ol>
      <p className="muted small legend-line">
        <span className="mark upd landed" aria-hidden="true" /> 갱신 도착{" "}
        <span className="mark upd sent" aria-hidden="true" /> 전송됨{" "}
        <span className="mark upd failed" aria-hidden="true" /> 실패 · B/S 스왑 · 빗금 칸은 가격이
        오래된 슬롯
      </p>
    </section>
  );
}
