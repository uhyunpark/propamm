export const BOT_URL: string = import.meta.env.VITE_BOT_URL ?? "http://127.0.0.1:8788";
export const BOT_WS_URL = `${BOT_URL.replace(/^http/, "ws")}/stream`;

export interface BotConfig {
  programId: string;
  market: string;
  baseMint: string;
  quoteMint: string;
  baseVault: string;
  quoteVault: string;
  baseDecimals: number;
  quoteDecimals: number;
  rpcUrl: string;
  wsUrl: string;
  tokenProgram: string;
}

export interface VenueQuote {
  bid: number;
  ask: number;
  mid: number;
}

export interface Tick {
  type: "tick";
  tsMs: number;
  venues: Record<string, VenueQuote>;
  fair: { price: number; confBps: number; live: number } | null;
  chain: {
    slot: number;
    mid: number;
    lastUpdateSlot: number;
    ageSlots: number;
    maxAgeSlots: number;
    paused: boolean;
  } | null;
  inventory: { base: number; quote: number; skewBps: number } | null;
  pythDevBps: number | null;
  publishing: boolean;
  offsetBps: number;
}

export interface UpdateEvent {
  type: "update";
  signature: string;
  targetSlot: number;
  landedSlot: number | null;
  delaySlots: number | null;
  mid: number;
  status: "sent" | "landed" | "failed";
}

export interface SwapEvent {
  type: "swap";
  signature: string;
  slot: number;
  side: "buy" | "sell";
  amountIn: string;
  amountOut: string;
  trader: string;
}

export type BotMessage = Tick | UpdateEvent | SwapEvent;
export type BotEvent = UpdateEvent | SwapEvent;

export interface ControlBody {
  publishing?: boolean;
  offsetBps?: number;
  offsetSeconds?: number;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BOT_URL}${path}`, init);
  if (!res.ok) throw new Error(`${path} ${res.status}`);
  return (await res.json()) as T;
}

export const fetchConfig = () => request<BotConfig>("/config");

const post = (path: string, body: unknown) =>
  request<unknown>(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });

export const postControl = (body: ControlBody) => post("/control", body);

export const postFaucet = (owner: string) => post("/faucet", { owner });
