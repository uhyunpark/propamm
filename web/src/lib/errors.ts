// Order and names follow programs/propamm/src/error.rs (Anchor codes start at 6000).
const PROP_AMM_ERRORS: readonly { name: string; ko: string }[] = [
  { name: "Unauthorized", ko: "권한이 없습니다" },
  { name: "InvalidParams", ko: "마켓 설정이 올바르지 않습니다" },
  { name: "InvalidMint", ko: "마켓의 토큰이 아닙니다" },
  { name: "InvalidVault", ko: "볼트가 마켓과 맞지 않습니다" },
  { name: "MathOverflow", ko: "계산 오버플로" },
  { name: "ZeroInput", ko: "입력 수량이 0입니다" },
  { name: "ZeroOutput", ko: "견적이 0으로 반올림됩니다" },
  { name: "InsufficientDepth", ko: "이 방향의 남은 호가 깊이가 부족합니다" },
  { name: "InvalidUpdater", ko: "갱신 키가 올바르지 않습니다" },
  { name: "NotInitializedPrice", ko: "아직 가격이 없습니다" },
  { name: "PriceMoveTooLarge", ko: "가격 이동이 한도를 넘었습니다" },
  { name: "Paused", ko: "마켓이 일시 중지되었습니다" },
  { name: "SlotRegression", ko: "슬롯이 거꾸로 갔습니다" },
  { name: "AlreadyUpdatedThisSlot", ko: "이 슬롯에서 이미 가격을 갱신했습니다" },
  { name: "StalePrice", ko: "가격이 오래되었습니다 (유효 기간 초과)" },
  { name: "SlippageExceeded", ko: "받는 양이 최소 수령량보다 적습니다" },
  { name: "InsufficientVaultBalance", ko: "볼트 잔고가 부족합니다" },
];

const FIRST_CODE = 6000;

export function programErrorName(code: number): string | null {
  return PROP_AMM_ERRORS[code - FIRST_CODE]?.name ?? null;
}

/** Korean message for a program error name (from the WASM module or the chain). */
export function describeErrorName(name: string): string {
  const hit = PROP_AMM_ERRORS.find((e) => e.name === name);
  return hit ? `${hit.ko} (${hit.name})` : name;
}

/** Finds a custom program error code (6000+) anywhere in an error's cause/context tree or text. */
export function findCustomCode(err: unknown, depth = 0): number | null {
  if (depth > 6 || err === null || typeof err !== "object") return null;
  const obj = err as Record<string, unknown>;
  const custom = obj.Custom;
  if (typeof custom === "number") return custom;
  // Kit's SolanaError context holds the custom code in `code` next to its own `__code`.
  const code = obj.code;
  if (typeof code === "number" && code >= FIRST_CODE && code < FIRST_CODE + 100) return code;
  for (const k of ["cause", "context", "InstructionError"]) {
    const found = findCustomCode(obj[k], depth + 1);
    if (found !== null) return found;
  }
  if (Array.isArray(obj.InstructionError)) {
    for (const item of obj.InstructionError) {
      const found = findCustomCode(item, depth + 1);
      if (found !== null) return found;
    }
  }
  const logs = obj.logs;
  const text = [
    typeof obj.message === "string" ? obj.message : "",
    ...(Array.isArray(logs) ? logs : []),
  ].join("\n");
  const hex = /custom program error: 0x([0-9a-f]+)/i.exec(text);
  if (hex?.[1]) return Number.parseInt(hex[1], 16);
  const named = /Error Number: (\d+)/.exec(text);
  if (named?.[1]) return Number(named[1]);
  return null;
}

/** Message of a thrown value as plain text. */
export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** Turns any thrown value into a short Korean message, naming PropAmm errors. */
export function explainError(err: unknown): string {
  const code = findCustomCode(err);
  if (code !== null) {
    const name = programErrorName(code);
    if (name) return describeErrorName(name);
    return `프로그램 오류 ${code}`;
  }
  return describeErrorName(errorMessage(err));
}
