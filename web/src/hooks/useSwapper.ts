import { useCallback } from "react";
import type { Engine, Side } from "../engine/engine";
import type { BotConfig } from "../lib/api";
import { minOutFor } from "../lib/quote";
import {
  buildSwapInstruction,
  type Rpc,
  type RpcSubscriptions,
  sendSwap,
  tokenBalance,
} from "../lib/solana";
import type { Wallet } from "./useWallet";

export interface SwapOutcome {
  signature: string;
  amountIn: bigint;
  expectedOut: bigint;
  actualOut: bigint;
}

interface Deps {
  config: BotConfig | null;
  engine: Engine | null;
  clients: { rpc: Rpc; rpcSubscriptions: RpcSubscriptions } | null;
  wallet: Wallet;
  /** Latest Market account bytes, read at call time. */
  getData: () => Uint8Array | null;
}

/** Quotes, sends and confirms one swap; the actual output is the token balance delta. */
export function useSwapper({ config, engine, clients, wallet, getData }: Deps) {
  const { signer, ata } = wallet;
  return useCallback(
    async (side: Side, amountIn: bigint, slippageBps: number): Promise<SwapOutcome> => {
      const data = getData();
      if (!config || !engine || !clients || !signer || !ata || !data) {
        throw new Error("아직 준비되지 않았습니다");
      }
      const expectedOut = engine.quoteExactIn(data, side, amountIn).amountOut;
      const minOut = minOutFor(expectedOut, slippageBps);
      const outAta = side === "buy" ? ata.base : ata.quote;
      const before = (await tokenBalance(clients.rpc, outAta)) ?? 0n;
      const ix = buildSwapInstruction(
        config,
        signer,
        ata,
        engine.swapIxData(side, amountIn, minOut),
      );
      const signature = await sendSwap(clients, signer, ix);
      const after = (await tokenBalance(clients.rpc, outAta)) ?? 0n;
      void wallet.refresh();
      return { signature, amountIn, expectedOut, actualOut: after - before };
    },
    [config, engine, clients, signer, ata, getData, wallet],
  );
}

export type Swapper = ReturnType<typeof useSwapper>;
