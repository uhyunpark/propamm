#!/bin/sh
# Stop the validator started by scripts/localnet.sh.
set -eu

cd "$(dirname "$0")/.."
PIDFILE=data/validator.pid

if [ ! -f "$PIDFILE" ]; then
    echo "no $PIDFILE; nothing to stop"
    exit 0
fi
pid=$(cat "$PIDFILE")
# The pid may be stale and reused by another program: only kill a validator.
case $(ps -p "$pid" -o comm= 2>/dev/null) in
    *solana-test-val*) kill "$pid" 2>/dev/null || true ;;
    *) echo "pid $pid is not a validator; removing the stale pid file" ;;
esac
i=0
while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 20 ]; do
    i=$((i + 1))
    sleep 0.5
done
rm -f "$PIDFILE"
echo "validator stopped"
