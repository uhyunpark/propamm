import {
  AccountRole,
  type Address,
  address,
  appendTransactionMessageInstruction,
  assertIsTransactionWithBlockhashLifetime,
  createKeyPairSignerFromPrivateKeyBytes,
  createSolanaRpc,
  createSolanaRpcSubscriptions,
  createTransactionMessage,
  getAddressEncoder,
  getProgramDerivedAddress,
  getSignatureFromTransaction,
  type Instruction,
  type KeyPairSigner,
  type Rpc as KitRpc,
  type RpcSubscriptions as KitRpcSubscriptions,
  pipe,
  type SolanaRpcApi,
  type SolanaRpcSubscriptionsApi,
  sendAndConfirmTransactionFactory,
  setTransactionMessageFeePayerSigner,
  setTransactionMessageLifetimeUsingBlockhash,
  signTransactionMessageWithSigners,
} from "@solana/kit";
import type { BotConfig } from "./api";

const ATA_PROGRAM = address("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SESSION_KEY = "propamm-burner";

// Cluster-agnostic types: the validator may be localnet or devnet.
export type Rpc = KitRpc<SolanaRpcApi>;
export type RpcSubscriptions = KitRpcSubscriptions<SolanaRpcSubscriptionsApi>;

export function createClients(cfg: BotConfig): { rpc: Rpc; rpcSubscriptions: RpcSubscriptions } {
  return {
    rpc: createSolanaRpc(cfg.rpcUrl) as Rpc,
    rpcSubscriptions: createSolanaRpcSubscriptions(cfg.wsUrl) as RpcSubscriptions,
  };
}

/** Burner key for this browser session: 32 random bytes kept in sessionStorage so reloads keep the ATAs. */
export async function loadBurner(): Promise<KeyPairSigner> {
  let bytes: Uint8Array | null = null;
  try {
    const saved = sessionStorage.getItem(SESSION_KEY);
    if (saved) bytes = Uint8Array.from(JSON.parse(saved) as number[]);
  } catch {
    bytes = null;
  }
  if (bytes?.length !== 32) {
    bytes = crypto.getRandomValues(new Uint8Array(32));
    try {
      sessionStorage.setItem(SESSION_KEY, JSON.stringify(Array.from(bytes)));
    } catch {
      // Storage may be blocked; the key then lives only until reload.
    }
  }
  return createKeyPairSignerFromPrivateKeyBytes(bytes);
}

export async function findAta(
  owner: Address,
  mint: Address,
  tokenProgram: Address,
): Promise<Address> {
  const enc = getAddressEncoder();
  const [ata] = await getProgramDerivedAddress({
    programAddress: ATA_PROGRAM,
    seeds: [enc.encode(owner), enc.encode(tokenProgram), enc.encode(mint)],
  });
  return ata;
}

export interface TraderAccounts {
  base: Address;
  quote: Address;
}

export async function traderAccounts(owner: Address, cfg: BotConfig): Promise<TraderAccounts> {
  const tokenProgram = address(cfg.tokenProgram);
  return {
    base: await findAta(owner, address(cfg.baseMint), tokenProgram),
    quote: await findAta(owner, address(cfg.quoteMint), tokenProgram),
  };
}

/** Swap instruction with the account order from docs/demo-contract.md. */
export function buildSwapInstruction(
  cfg: BotConfig,
  trader: KeyPairSigner,
  ata: TraderAccounts,
  data: Uint8Array,
): Instruction {
  const W = AccountRole.WRITABLE;
  const R = AccountRole.READONLY;
  return {
    programAddress: address(cfg.programId),
    accounts: [
      { address: trader.address, role: AccountRole.WRITABLE_SIGNER },
      { address: address(cfg.market), role: W },
      { address: address(cfg.baseMint), role: R },
      { address: address(cfg.quoteMint), role: R },
      { address: address(cfg.baseVault), role: W },
      { address: address(cfg.quoteVault), role: W },
      { address: ata.base, role: W },
      { address: ata.quote, role: W },
      { address: address(cfg.tokenProgram), role: R },
    ],
    data,
  };
}

export async function sendSwap(
  clients: { rpc: Rpc; rpcSubscriptions: RpcSubscriptions },
  trader: KeyPairSigner,
  ix: Instruction,
): Promise<string> {
  const { value: blockhash } = await clients.rpc
    .getLatestBlockhash({ commitment: "confirmed" })
    .send();
  const message = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(trader, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(blockhash, m),
    (m) => appendTransactionMessageInstruction(ix, m),
  );
  const signed = await signTransactionMessageWithSigners(message);
  assertIsTransactionWithBlockhashLifetime(signed);
  const send = sendAndConfirmTransactionFactory(clients);
  await send(signed, { commitment: "confirmed" });
  return getSignatureFromTransaction(signed);
}

export async function tokenBalance(rpc: Rpc, ata: Address): Promise<bigint | null> {
  try {
    const res = await rpc.getTokenAccountBalance(ata, { commitment: "confirmed" }).send();
    return BigInt(res.value.amount);
  } catch {
    return null; // the account does not exist yet
  }
}

export async function solBalance(rpc: Rpc, owner: Address): Promise<bigint> {
  const res = await rpc.getBalance(owner, { commitment: "confirmed" }).send();
  return res.value;
}
