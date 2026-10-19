/** Reconnect delay in ms: 0.5 s doubling per failed attempt, capped at 10 s. */
export function backoffMs(attempt: number): number {
  return Math.min(10_000, 500 * 2 ** attempt);
}
