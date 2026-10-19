import { address } from "@solana/kit";
import { useEffect, useMemo, useState } from "react";
import type { Engine, MarketView } from "../engine/engine";
import type { BotConfig } from "../lib/api";
import { backoffMs } from "../lib/backoff";
import { base64ToBytes } from "../lib/format";
import type { Rpc, RpcSubscriptions } from "../lib/solana";

export interface MarketAccount {
  data: Uint8Array | null;
  view: MarketView | null;
  /** True while the RPC subscription is alive. */
  connected: boolean;
}

/** Market account bytes via getAccountInfo plus accountNotifications, with backoff on failure. */
export function useMarketAccount(
  config: BotConfig | null,
  clients: { rpc: Rpc; rpcSubscriptions: RpcSubscriptions } | null,
  engine: Engine | null,
): MarketAccount {
  const [data, setData] = useState<Uint8Array | null>(null);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    if (!config || !clients) return;
    const market = address(config.market);
    const abort = new AbortController();
    let attempt = 0;

    const run = async () => {
      while (!abort.signal.aborted) {
        try {
          const first = await clients.rpc
            .getAccountInfo(market, { encoding: "base64", commitment: "confirmed" })
            .send();
          if (first.value) setData(base64ToBytes(first.value.data[0]));
          const sub = await clients.rpcSubscriptions
            .accountNotifications(market, { encoding: "base64", commitment: "confirmed" })
            .subscribe({ abortSignal: abort.signal });
          setConnected(true);
          attempt = 0;
          for await (const note of sub) setData(base64ToBytes(note.value.data[0]));
        } catch {
          // Fall through to the backoff below (an abort also lands here).
        }
        setConnected(false);
        if (abort.signal.aborted) return;
        await new Promise((r) => setTimeout(r, backoffMs(attempt++)));
      }
    };
    void run();
    return () => abort.abort();
  }, [config, clients]);

  const view = useMemo(() => {
    if (!data || !engine) return null;
    try {
      return engine.decodeMarket(data);
    } catch {
      return null;
    }
  }, [data, engine]);

  return { data, view, connected };
}
