import { useEffect, useRef, useState } from "react";
import { BOT_WS_URL, type BotEvent, type BotMessage, type Tick } from "../lib/api";
import { backoffMs } from "../lib/backoff";

const MAX_EVENTS = 200;
const MAX_SLOT_AGES = 300;
const STALE_MS = 2_000;

export interface BotStream {
  connected: boolean;
  tick: Tick | null;
  /** True when the last tick is older than a couple of seconds (shown greyed out). */
  stale: boolean;
  events: BotEvent[];
  /** Price age observed per slot (from ticks), for the timeline's stale highlight. */
  slotAges: ReadonlyMap<number, number>;
}

export function useBotStream(): BotStream {
  const [connected, setConnected] = useState(false);
  const [tick, setTick] = useState<Tick | null>(null);
  const [events, setEvents] = useState<BotEvent[]>([]);
  const [now, setNow] = useState(() => Date.now());
  const lastTickAt = useRef(0);
  const slotAges = useRef(new Map<number, number>());

  useEffect(() => {
    let ws: WebSocket | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    let closed = false;

    const connect = () => {
      ws = new WebSocket(BOT_WS_URL);
      ws.onopen = () => {
        attempt = 0;
        setConnected(true);
      };
      ws.onmessage = (ev) => {
        let msg: BotMessage;
        try {
          msg = JSON.parse(String(ev.data)) as BotMessage;
        } catch {
          return;
        }
        if (msg.type === "tick") {
          lastTickAt.current = Date.now();
          if (msg.chain) {
            const ages = slotAges.current;
            ages.set(msg.chain.slot, msg.chain.ageSlots);
            if (ages.size > MAX_SLOT_AGES) {
              const oldest = ages.keys().next().value;
              if (oldest !== undefined) ages.delete(oldest);
            }
          }
          setTick(msg);
        } else {
          setEvents((prev) => mergeEvent(prev, msg));
        }
      };
      ws.onclose = () => {
        setConnected(false);
        if (closed) return;
        const delay = backoffMs(attempt++);
        timer = setTimeout(connect, delay);
      };
      ws.onerror = () => ws?.close();
    };
    connect();
    const clock = setInterval(() => setNow(Date.now()), 1_000);
    return () => {
      closed = true;
      clearTimeout(timer);
      clearInterval(clock);
      ws?.close();
    };
  }, []);

  const stale = !connected || now - lastTickAt.current > STALE_MS;
  return { connected, tick, stale, events, slotAges: slotAges.current };
}

/** Updates replace the earlier status of the same signature; swaps are added once. */
export function mergeEvent(prev: BotEvent[], msg: BotEvent): BotEvent[] {
  const i = prev.findIndex((e) => e.type === msg.type && e.signature === msg.signature);
  if (i >= 0) {
    const next = prev.slice();
    next[i] = msg;
    return next;
  }
  const next = [...prev, msg];
  return next.length > MAX_EVENTS ? next.slice(next.length - MAX_EVENTS) : next;
}
