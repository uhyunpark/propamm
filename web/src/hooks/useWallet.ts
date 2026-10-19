import type { KeyPairSigner } from "@solana/kit";
import { useCallback, useEffect, useState } from "react";
import { type BotConfig, postFaucet } from "../lib/api";
import {
  loadBurner,
  type Rpc,
  solBalance,
  type TraderAccounts,
  tokenBalance,
  traderAccounts,
} from "../lib/solana";

export interface Balances {
  sol: bigint | null;
  base: bigint | null;
  quote: bigint | null;
}

export interface Wallet {
  signer: KeyPairSigner | null;
  ata: TraderAccounts | null;
  balances: Balances;
  refresh: () => Promise<Balances | null>;
  faucet: () => Promise<void>;
}

const EMPTY: Balances = { sol: null, base: null, quote: null };

export function useWallet(config: BotConfig | null, rpc: Rpc | null): Wallet {
  const [signer, setSigner] = useState<KeyPairSigner | null>(null);
  const [ata, setAta] = useState<TraderAccounts | null>(null);
  const [balances, setBalances] = useState<Balances>(EMPTY);

  useEffect(() => {
    loadBurner().then(setSigner);
  }, []);

  useEffect(() => {
    if (!signer || !config) return;
    traderAccounts(signer.address, config).then(setAta);
  }, [signer, config]);

  const refresh = useCallback(async () => {
    if (!signer || !ata || !rpc) return null;
    try {
      const [sol, base, quote] = await Promise.all([
        solBalance(rpc, signer.address),
        tokenBalance(rpc, ata.base),
        tokenBalance(rpc, ata.quote),
      ]);
      const next = { sol, base, quote };
      setBalances(next);
      return next;
    } catch {
      return null;
    }
  }, [signer, ata, rpc]);

  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), 4_000);
    return () => clearInterval(id);
  }, [refresh]);

  const faucet = useCallback(async () => {
    if (!signer) return;
    await postFaucet(signer.address);
    await refresh();
  }, [signer, refresh]);

  return { signer, ata, balances, refresh, faucet };
}
