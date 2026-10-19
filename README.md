# propamm

A Solana PropAMM (proprietary AMM). A market-maker bot publishes a fair-value mid price every slot, and the program quotes swaps from a price ladder around that mid.

- Demo interface (WASM, bot server, web): [`docs/demo-contract.md`](docs/demo-contract.md)

## Layout

| Path | What |
|---|---|
| `programs/propamm` | Anchor program (market, `update_price`, `swap`) |
| `programs/propamm-pinocchio` | Pinocchio port for the CU comparison |
| `crates/pricing` | `no_std` pricing core shared by the program, bot, WASM and Jupiter adapter |
| `crates/client` | Instruction encoding and account decoding |
| `crates/bot` | Feed recorder, fair-value engine, update sender, demo server |
| `crates/wasm` | WASM bindings for the web app |
| `crates/jupiter-sdk` | Jupiter `Amm` adapter |
| `web/` | Demo page (Vite, React, TypeScript) |

## Demo on localnet

Needs the Solana toolchain on `PATH` (`docs/toolchain.md`), bun, and `wasm-bindgen` matching the pinned version.

```sh
make build-sbf                     # program binaries
scripts/localnet.sh                # 1. validator + funded, priced market (data/localnet.json)
cargo run -p propamm-bot --release -- run --config data/localnet.json \
  --serve 127.0.0.1:8788 --faucet  # 2. bot: price updates, demo server, local faucet
make wasm                          # 3. web: WASM pricing module, then
cd web && bun install && bun run dev   #    open http://127.0.0.1:5173
make demo-e2e                      # optional: browser E2E against the running stack
scripts/localnet-stop.sh           # stop the validator
```

The bot needs outbound access to the exchange feeds (Binance, OKX, Bybit, Hyperliquid, Coinbase). `VITE_BOT_URL` overrides the bot address the page uses.

## Make targets

| Target | What |
|---|---|
| `make check` | fmt check, clippy `-D warnings`, SBF build, tests |
| `make fmt` | format everything |
| `make deny` | supply-chain audit (`cargo deny`) |
| `make wasm` | build the WASM module into `web/src/wasm/` |
| `make wasm-test` | load the WASM module in bun and compare with the Rust fixtures |
| `make demo-e2e` | headless Chrome E2E of the demo; writes `target/e2e/*.png` |
| `make mutants` | mutation testing for the pricing crate (slow) |

## Devnet

`scripts/devnet-deploy.sh` deploys the program and creates the demo market (`data/devnet.json`). It needs a funded admin key: when the key holds too little SOL, the script prints the address and the amount to request from https://faucet.solana.com, then stops.
