import { useCallback, useEffect, useMemo, useRef } from "react";
import { EventLog } from "./components/EventLog";
import { Header } from "./components/Header";
import { type Band, PricePanel } from "./components/PricePanel";
import { Scenarios } from "./components/Scenarios";
import { SlotTimeline } from "./components/SlotTimeline";
import { SwapBox } from "./components/SwapBox";
import { useBotStream } from "./hooks/useBotStream";
import { useConfig } from "./hooks/useConfig";
import { useEngine } from "./hooks/useEngine";
import { useMarketAccount } from "./hooks/useMarketAccount";
import { useSwapper } from "./hooks/useSwapper";
import { useWallet } from "./hooks/useWallet";
import { BOT_URL } from "./lib/api";
import { createClients } from "./lib/solana";

export function App() {
  const eng = useEngine();
  const { config } = useConfig();
  const stream = useBotStream();
  const clients = useMemo(() => (config ? createClients(config) : null), [config]);
  const market = useMarketAccount(config, clients, eng?.engine ?? null);
  const wallet = useWallet(config, clients?.rpc ?? null);

  const dataRef = useRef<Uint8Array | null>(null);
  useEffect(() => {
    dataRef.current = market.data;
  }, [market.data]);
  const getData = useCallback(() => dataRef.current, []);
  const swapper = useSwapper({
    config,
    engine: eng?.engine ?? null,
    clients,
    wallet,
    getData,
  });

  const band = useMemo<Band | null>(() => {
    if (!eng || !config || !market.data) return null;
    try {
      const ask = eng.engine.ladder(market.data, "buy")[0];
      const bid = eng.engine.ladder(market.data, "sell")[0];
      if (!ask || !bid) return null;
      const ui = (p: bigint) => eng.engine.priceToUi(p, config.baseDecimals, config.quoteDecimals);
      return { ask: ui(ask.price), bid: ui(bid.price) };
    } catch {
      return null; // unpriced market
    }
  }, [eng, config, market.data]);

  const chain = stream.tick?.chain ?? null;
  const ready = config && eng;

  return (
    <div className="page">
      <Header stream={stream} rpcConnected={market.connected} engineKind={eng?.kind ?? null} />
      {!config ? (
        <p className="banner">
          봇 서버({BOT_URL})에 연결하는 중입니다. 봇을 --serve로 실행했는지 확인하세요.
        </p>
      ) : null}
      <div className="grid">
        <div className="col">
          <PricePanel tick={stream.tick} stale={stream.stale} band={band} />
          {config ? (
            <SlotTimeline
              config={config}
              currentSlot={chain?.slot ?? null}
              maxAge={chain?.maxAgeSlots ?? null}
              events={stream.events}
              slotAges={stream.slotAges}
              stale={stream.stale}
            />
          ) : null}
        </div>
        <div className="col">
          {ready ? (
            <>
              <SwapBox
                config={config}
                engine={eng.engine}
                data={market.data}
                tick={stream.tick}
                wallet={wallet}
                swapper={swapper}
              />
              <Scenarios
                config={config}
                engine={eng.engine}
                data={market.data}
                view={market.view}
                tick={stream.tick}
                swapper={swapper}
              />
            </>
          ) : null}
        </div>
      </div>
      <EventLog events={stream.events} config={config} />
    </div>
  );
}
