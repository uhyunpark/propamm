import { expect, test } from "bun:test";
import {
  getSolanaErrorFromTransactionError,
  SOLANA_ERROR__JSON_RPC__SERVER_ERROR_SEND_TRANSACTION_PREFLIGHT_FAILURE,
  SolanaError,
} from "@solana/kit";
import { explainError, findCustomCode } from "./errors";

// StalePrice is the 15th PropAmm error: 6014 (0x177e).
const STALE = { InstructionError: [0, { Custom: 6014 }] } as const;

test("names a custom error thrown by confirmation (no logs)", () => {
  // sendAndConfirm throws this when preflight passed but the transaction failed on chain.
  const err = getSolanaErrorFromTransactionError(STALE);
  expect(findCustomCode(err)).toBe(6014);
  expect(explainError(err)).toContain("(StalePrice)");
});

test("names a custom error from a preflight failure", () => {
  const err = new SolanaError(
    SOLANA_ERROR__JSON_RPC__SERVER_ERROR_SEND_TRANSACTION_PREFLIGHT_FAILURE,
    {
      accounts: null,
      fee: null,
      innerInstructions: null,
      loadedAccountsDataSize: undefined,
      loadedAddresses: null,
      logs: [],
      postBalances: null,
      postTokenBalances: null,
      preBalances: null,
      preTokenBalances: null,
      replacementBlockhash: null,
      returnData: null,
      unitsConsumed: undefined,
      cause: getSolanaErrorFromTransactionError(STALE),
    } as never,
  );
  expect(explainError(err)).toContain("(StalePrice)");
});

test("falls back to the program log text", () => {
  const err = { message: "x", logs: ["Program X failed: custom program error: 0x177e"] };
  expect(findCustomCode(err)).toBe(6014);
});

test("passes WASM error names through", () => {
  expect(explainError(new Error("InsufficientDepth"))).toContain("(InsufficientDepth)");
});
