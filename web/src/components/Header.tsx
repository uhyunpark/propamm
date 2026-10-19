import type { EngineKind } from "../engine/engine";
import type { BotStream } from "../hooks/useBotStream";

interface Props {
  stream: BotStream;
  rpcConnected: boolean;
  engineKind: EngineKind | null;
}

function Pill({ ok, label, detail }: { ok: boolean | null; label: string; detail?: string }) {
  const cls = ok === null ? "pill warn" : ok ? "pill ok" : "pill bad";
  return (
    <span className={cls}>
      <span className="dot" aria-hidden="true" />
      {label}
      {detail ? <span className="pill-detail">{detail}</span> : null}
    </span>
  );
}

export function Header({ stream, rpcConnected, engineKind }: Props) {
  const chain = stream.tick?.chain ?? null;
  const near = chain ? chain.ageSlots >= chain.maxAgeSlots * 0.7 : false;
  const over = chain ? chain.ageSlots > chain.maxAgeSlots : false;
  return (
    <header className="header">
      <div>
        <h1>PropAMM 데모</h1>
        <p className="muted">
          마켓 메이커가 슬롯마다 기준 가격을 갱신하면, 호가는 그 가격에서 바로 계산됩니다.
        </p>
      </div>
      <div className="status">
        <div className="pills" role="status" aria-label="연결 상태">
          <Pill ok={stream.connected && !stream.stale} label="봇 서버" />
          <Pill ok={rpcConnected} label="검증기 RPC" />
          <Pill
            ok={engineKind === null ? null : engineKind === "wasm"}
            label={engineKind === "mock" ? "mock 엔진" : "WASM"}
            detail={engineKind === "mock" ? "WASM 미빌드" : undefined}
          />
        </div>
        <div className={`slotinfo${stream.stale ? " greyed" : ""}`}>
          <span>
            슬롯 <b className="num">{chain?.slot.toLocaleString() ?? "-"}</b>
          </span>
          <span className={over ? "age bad" : near ? "age warn" : "age"}>
            가격 나이{" "}
            <b className="num">{chain ? `${chain.ageSlots} / ${chain.maxAgeSlots}` : "-"}</b>
            {over ? " 오래됨" : near ? " 곧 만료" : ""}
          </span>
        </div>
      </div>
    </header>
  );
}
