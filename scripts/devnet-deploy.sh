#!/bin/sh
# Deploy the propamm program to devnet with the devnet admin key as upgrade authority, then
# create the demo market (`propamm-bot setup --cluster devnet`, config: data/devnet.json).
# Needs the Solana toolchain on PATH and target/deploy/propamm.so (run `make build-sbf`).
# Stops with instructions when the admin key holds too little SOL. It never airdrops.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
ADMIN=${ADMIN_KEYPAIR:-$HOME/.config/solana/propamm-devnet-admin.json}
PROGRAM_KEYPAIR=${PROGRAM_KEYPAIR:-$HOME/.config/solana/propamm-program-keypair.json}
PROGRAM_ID=2PuFKsYDoYaVWFnSxVGgbD2Xvsz3nymLu2WuctvF6V3F
SO=target/deploy/propamm.so
URL=devnet
FAUCET_URL=https://faucet.solana.com
# SOL that `setup --cluster devnet` needs after the deploy (rent, updater and trader top-ups, fees).
SETUP_RESERVE_LAMPORTS=650000000
FEE_MARGIN_LAMPORTS=20000000

die() { echo "error: $*" >&2; exit 1; }
command -v solana >/dev/null || die "solana not on PATH (see docs/toolchain.md)"
command -v solana-keygen >/dev/null || die "solana-keygen not on PATH (see docs/toolchain.md)"
[ -f "$SO" ] || die "missing $SO: run 'make build-sbf' first"
[ -f "$ADMIN" ] || die "missing admin keypair $ADMIN"
[ -f "$PROGRAM_KEYPAIR" ] || die "missing program keypair $PROGRAM_KEYPAIR"

ADMIN_PUBKEY=$(solana-keygen pubkey "$ADMIN")
[ "$(solana-keygen pubkey "$PROGRAM_KEYPAIR")" = "$PROGRAM_ID" ] ||
    die "$PROGRAM_KEYPAIR is not the keypair of program id $PROGRAM_ID (declare_id! in programs/propamm)"

# Rent of an account of $1 bytes, in lamports.
rent() { solana rent "$1" --lamports --url "$URL" | awk '/Rent-exempt minimum/ { print $3 }'; }
sol() { awk -v l="$1" 'BEGIN { printf "%.4f", l / 1e9 }'; }

SIZE=$(wc -c <"$SO" | tr -d ' ')
# Headroom for upgrades: the default would be twice the program size (about 1.6 SOL more rent).
MAX_LEN=$((SIZE + SIZE / 4))
BUFFER_RENT=$(rent $((SIZE + 37)))
if solana program show "$PROGRAM_ID" --url "$URL" --keypair "$ADMIN" >/dev/null 2>&1; then
    MODE=upgrade
    # An upgrade only needs the temporary buffer (refunded afterwards).
    NEEDED=$((BUFFER_RENT + FEE_MARGIN_LAMPORTS))
else
    MODE=deploy
    # Peak: program data account + the buffer it is copied from + the program account.
    NEEDED=$(($(rent $((MAX_LEN + 45))) + BUFFER_RENT + $(rent 36) + FEE_MARGIN_LAMPORTS + SETUP_RESERVE_LAMPORTS))
fi
BALANCE=$(solana balance "$ADMIN_PUBKEY" --lamports --url "$URL" | awk '{ print $1 }')

echo "mode:      $MODE ($SO, $SIZE bytes, max length $MAX_LEN)"
echo "admin:     $ADMIN_PUBKEY (upgrade authority)"
echo "balance:   $(sol "$BALANCE") SOL on devnet"
echo "needed:    about $(sol "$NEEDED") SOL (program rent, deploy buffer, setup reserve, fees)"

if [ "$BALANCE" -lt "$NEEDED" ]; then
    cat >&2 <<MSG

Not enough devnet SOL: short by about $(sol $((NEEDED - BALANCE))) SOL.
  1. Open $FAUCET_URL
  2. Request devnet SOL for: $ADMIN_PUBKEY
     (the web faucet gives a few SOL per request; repeat later if you need more)
  3. Run scripts/devnet-deploy.sh again.
MSG
    exit 2
fi

solana program deploy "$SO" --url "$URL" --keypair "$ADMIN" \
    --program-id "$PROGRAM_KEYPAIR" --upgrade-authority "$ADMIN" --max-len "$MAX_LEN"

cargo run -q -p propamm-bot --release -- setup --cluster devnet \
    --admin-keypair "$ADMIN" --out data/devnet.json

cat <<MSG

Devnet market is ready (config: data/devnet.json).
  bot:   cargo run -q -p propamm-bot --release -- run --config data/devnet.json --serve 127.0.0.1:8788
  swap:  cargo run -q -p propamm-bot --release -- swap --config data/devnet.json --side buy --amount 100
MSG
