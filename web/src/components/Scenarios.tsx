import { useState } from "react";
import type { Engine, MarketView } from "../engine/engine";
import type { Swapper } from "../hooks/useSwapper";
import { type BotConfig, postControl, type Tick } from "../lib/api";
import { errorMessage } from "../lib/errors";
import { SplitCard } from "./SplitCard";

interface Props {
  config: BotConfig;
  engine: Engine;
  data: Uint8Array | null;
  view: MarketView | null;
  tick: Tick | null;
  swapper: Swapper;
}

export function Scenarios({ config, engine, data, view, tick, swapper }: Props) {
  const [note, setNote] = useState<string | null>(null);

  const publishing = tick?.publishing ?? true;
  const chain = tick?.chain ?? null;

  const control = async (body: Parameters<typeof postControl>[0], ok: string) => {
    setNote(null);
    try {
      await postControl(body);
      setNote(ok);
    } catch (e) {
      setNote(`봇 서버 요청 실패: ${errorMessage(e)}`);
    }
  };

  return (
    <section className="card" aria-labelledby="scn-title">
      <h2 id="scn-title">시나리오</h2>

      <div className="scenario">
        <div className="btns">
          <button
            type="button"
            disabled={!publishing}
            onClick={() =>
              control({ publishing: false }, "봇이 갱신을 멈췄습니다. 가격 나이가 계속 늘어납니다.")
            }
          >
            봇 멈추기
          </button>
          <button
            type="button"
            disabled={publishing}
            onClick={() => control({ publishing: true }, "봇이 갱신을 다시 시작했습니다.")}
          >
            다시 시작
          </button>
        </div>
        <p className="muted small">
          갱신이 끊기면 가격 나이가 유효 기간을 넘고, 스왑은 StalePrice로 거절됩니다. 지금{" "}
          <b className="num">{chain ? `${chain.ageSlots} / ${chain.maxAgeSlots}` : "-"}</b> 슬롯
          {!publishing ? " (멈춤)" : ""}. 멈춘 채로 스왑을 눌러 보세요.
        </p>
      </div>

      <div className="scenario">
        <div className="btns">
          <button
            type="button"
            onClick={() =>
              control(
                { offsetBps: 50, offsetSeconds: 10 },
                "공정 가격에 +50 bps를 10초 동안 더합니다.",
              )
            }
          >
            가격 급변 +50bps (10초)
          </button>
        </div>
        <p className="muted small">
          공정 가격이 갑자기 뛰어도 기준 가격(온체인)은 슬롯당 이동 한도 안에서만 따라갑니다.
          차트에서 두 선의 차이를 보세요.
        </p>
      </div>

      <SplitCard config={config} engine={engine} data={data} view={view} swapper={swapper} />

      {note ? (
        <p className="small" role="status">
          {note}
        </p>
      ) : null}
    </section>
  );
}
