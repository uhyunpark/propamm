import {
  ColorType,
  createChart,
  type IChartApi,
  type ISeriesApi,
  LineSeries,
  LineStyle,
  LineType,
  type Time,
  type UTCTimestamp,
} from "lightweight-charts";
import { useEffect, useRef, useState } from "react";
import type { Tick } from "../lib/api";
import { formatBps, formatUsd, gapBps } from "../lib/format";

const WINDOW_SECONDS = 180;

interface SeriesDef {
  id: string;
  label: string;
  color: string; // CSS variable name
  width: 1 | 2 | 3;
  style?: LineStyle;
  step?: boolean;
}

const VENUES: SeriesDef[] = [
  { id: "binance_perp", label: "Binance 선물", color: "--c1", width: 1 },
  { id: "binance_spot", label: "Binance 현물", color: "--c2", width: 1 },
  { id: "okx_perp", label: "OKX 선물", color: "--c3", width: 1 },
  { id: "bybit_perp", label: "Bybit 선물", color: "--c4", width: 1 },
  { id: "hyperliquid", label: "Hyperliquid", color: "--c5", width: 1 },
  {
    id: "coinbase",
    label: "Coinbase 참고(평가용)",
    color: "--muted",
    width: 1,
    style: LineStyle.Dotted,
  },
];

const DEFS: SeriesDef[] = [
  ...VENUES,
  { id: "fair", label: "공정 가격", color: "--fair", width: 3 },
  { id: "chain", label: "기준 가격 (온체인)", color: "--chain", width: 2, step: true },
  {
    id: "ask0",
    label: "호가 상단 (레벨 0 ask)",
    color: "--band",
    width: 1,
    style: LineStyle.Dashed,
  },
  {
    id: "bid0",
    label: "호가 하단 (레벨 0 bid)",
    color: "--band",
    width: 1,
    style: LineStyle.Dashed,
  },
];

function css(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
}

function timeLabel(t: Time): string {
  return typeof t === "number"
    ? new Date(t * 1000).toLocaleTimeString("en-GB", { hour12: false }) // compact HH:MM:SS
    : String(t);
}

/** Fair value the bot is targeting: the demo offset (`POST /control`) is not part of `fair.price`. */
function shownFair(tick: Tick): number | null {
  return tick.fair ? tick.fair.price * (1 + tick.offsetBps / 10_000) : null;
}

export interface Band {
  bid: number;
  ask: number;
}

interface Props {
  tick: Tick | null;
  stale: boolean;
  band: Band | null;
}

export function PricePanel({ tick, stale, band }: Props) {
  const host = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const series = useRef(new Map<string, ISeriesApi<"Line">>());
  const lastTime = useRef(new Map<string, number>());
  const lastRange = useRef(0);
  const [on, setOn] = useState<Record<string, boolean>>({});

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const chart = createChart(el, {
      autoSize: true,
      localization: { timeFormatter: timeLabel },
      timeScale: { timeVisible: true, secondsVisible: true, tickMarkFormatter: timeLabel },
      layout: { background: { type: ColorType.Solid, color: "transparent" } },
    });
    chartRef.current = chart;
    const map = series.current;
    for (const d of DEFS) {
      map.set(
        d.id,
        chart.addSeries(LineSeries, {
          lineWidth: d.width,
          lineStyle: d.style ?? LineStyle.Solid,
          lineType: d.step ? LineType.WithSteps : LineType.Simple,
          priceLineVisible: false,
          lastValueVisible: d.id === "fair" || d.id === "chain",
          crosshairMarkerVisible: false,
        }),
      );
    }
    const paint = () => {
      chart.applyOptions({
        layout: { textColor: css("--muted") },
        grid: { vertLines: { color: css("--grid") }, horzLines: { color: css("--grid") } },
        rightPriceScale: { borderColor: css("--border") },
        timeScale: { borderColor: css("--border") },
      });
      for (const d of DEFS) map.get(d.id)?.applyOptions({ color: css(d.color) });
    };
    paint();
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    mq.addEventListener("change", paint);
    const times = lastTime.current;
    return () => {
      mq.removeEventListener("change", paint);
      chart.remove();
      chartRef.current = null;
      map.clear();
      times.clear();
    };
  }, []);

  useEffect(() => {
    for (const d of DEFS) series.current.get(d.id)?.applyOptions({ visible: on[d.id] ?? true });
  }, [on]);

  useEffect(() => {
    if (!tick) return;
    const t = Math.floor(tick.tsMs / 1000);
    const push = (id: string, value: number | null | undefined) => {
      if (value === null || value === undefined || !Number.isFinite(value)) return;
      const last = lastTime.current.get(id) ?? 0;
      if (t < last) return;
      lastTime.current.set(id, t);
      series.current.get(id)?.update({ time: t as UTCTimestamp, value });
      // Number of series with data, read by the browser E2E (e2e/demo.ts).
      if (host.current) host.current.dataset.series = String(lastTime.current.size);
    };
    for (const v of VENUES) push(v.id, tick.venues[v.id]?.mid);
    push("fair", shownFair(tick));
    push("chain", tick.chain?.mid);
    push("ask0", band?.ask);
    push("bid0", band?.bid);
    if (t - lastRange.current >= 1) {
      lastRange.current = t;
      chartRef.current?.timeScale().setVisibleRange({
        from: (t - WINDOW_SECONDS) as UTCTimestamp,
        to: (t + 5) as UTCTimestamp,
      });
    }
  }, [tick, band]);

  const fair = tick ? shownFair(tick) : null;
  const offset = tick?.offsetBps ?? 0;
  const chainMid = tick?.chain?.mid ?? null;
  const gap = fair !== null && chainMid !== null ? gapBps(chainMid, fair) : null;

  return (
    <section className={`card${stale ? " greyed" : ""}`} aria-labelledby="price-title">
      <h2 id="price-title">가격</h2>
      <dl className="stats">
        <div>
          <dt>공정 가격{offset !== 0 ? ` (시연 ${offset > 0 ? "+" : ""}${offset} bps)` : ""}</dt>
          <dd className="num">{formatUsd(fair)}</dd>
        </div>
        <div>
          <dt>기준 가격 (온체인)</dt>
          <dd className="num">{formatUsd(chainMid)}</dd>
        </div>
        <div>
          <dt>차이</dt>
          <dd className="num">{gap === null ? "-" : formatBps(gap)}</dd>
        </div>
        <div>
          <dt>신뢰 구간</dt>
          <dd className="num">{tick?.fair ? formatBps(tick.fair.confBps) : "-"}</dd>
        </div>
      </dl>
      <div
        className="chart"
        ref={host}
        role="img"
        aria-label="거래소별 가격, 공정 가격, 기준 가격(온체인) 차트"
      />
      <fieldset className="legend">
        <legend className="sr-only">표시할 선</legend>
        {DEFS.map((d) => (
          <label key={d.id} className="legend-item">
            <input
              type="checkbox"
              checked={on[d.id] ?? true}
              onChange={(e) => setOn((prev) => ({ ...prev, [d.id]: e.target.checked }))}
            />
            <span className="swatch" style={{ background: `var(${d.color})` }} aria-hidden="true" />
            {d.label}
          </label>
        ))}
      </fieldset>
    </section>
  );
}
