#!/bin/sh
# Start a local validator with the propamm program, then create a funded, priced market.
# Needs the Solana toolchain on PATH and target/deploy/propamm.so (run `make build-sbf`).
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
RPC=${RPC:-http://127.0.0.1:8899}
PROGRAM_ID=2PuFKsYDoYaVWFnSxVGgbD2Xvsz3nymLu2WuctvF6V3F
SO=target/deploy/propamm.so
PIDFILE=data/validator.pid
LOG=data/validator.log

[ -f "$SO" ] || { echo "missing $SO: run 'make build-sbf' first" >&2; exit 1; }
command -v solana-test-validator >/dev/null || { echo "solana-test-validator not on PATH (see docs/toolchain.md)" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl not found" >&2; exit 1; }
if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    echo "validator already running (pid $(cat "$PIDFILE")); run scripts/localnet-stop.sh first" >&2
    exit 1
fi

mkdir -p data
# Stop the validator again if anything below fails.
ok=0
cleanup() {
    [ "$ok" -eq 1 ] && return
    if [ -f "$PIDFILE" ]; then
        kill "$(cat "$PIDFILE")" 2>/dev/null || true
        rm -f "$PIDFILE"
    fi
}
trap cleanup EXIT
trap 'exit 1' INT TERM

solana-test-validator --reset --quiet --bpf-program "$PROGRAM_ID" "$SO" --ledger data/test-ledger >"$LOG" 2>&1 &
echo $! >"$PIDFILE"

echo "waiting for the validator (log: $LOG)"
i=0
until curl -s -m 2 -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$RPC" | grep -q '"ok"'; do
    i=$((i + 1))
    kill -0 "$(cat "$PIDFILE")" 2>/dev/null || { echo "validator exited; see $LOG" >&2; exit 1; }
    [ "$i" -lt 60 ] || { echo "validator did not become healthy; see $LOG" >&2; exit 1; }
    sleep 1
done

BOT="cargo run -q -p propamm-bot --release --"
# SETUP_ARGS is split into words on purpose (e.g. SETUP_ARGS="--fair-usd 200").
# shellcheck disable=SC2086
$BOT setup-local --rpc "$RPC" ${SETUP_ARGS:-} --out data/localnet.json
ok=1

cat <<MSG

Local market is ready (config: data/localnet.json, validator pid: $(cat "$PIDFILE")).
  bot:    $BOT run --rpc $RPC --ws ws://127.0.0.1:8900 --config data/localnet.json
  demo:   $BOT run --config data/localnet.json --serve 127.0.0.1:8788 --faucet   (then: cd web && bun run dev)
  buy:    $BOT swap --config data/localnet.json --side buy --amount 1000
  sell:   $BOT swap --config data/localnet.json --side sell --amount 5
  admin:  $BOT force-price --config data/localnet.json --mid 200
          $BOT pause --config data/localnet.json   (or: unpause)
  stop:   scripts/localnet-stop.sh
MSG
