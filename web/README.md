# PropAMM demo web app

Run order:

1. Validator with the program and market: `scripts/localnet.sh`
2. Bot with the server and faucet: `cargo run -p propamm-bot --release -- run --config data/localnet.json --serve 127.0.0.1:8788 --faucet`
3. Web: `make wasm`, then `cd web && bun install && bun run dev`, open http://127.0.0.1:5173
4. Optional browser E2E (headless system Chrome, screenshots to `target/e2e/`): `make demo-e2e`

Without `make wasm` the app uses a TypeScript mock engine (marked "mock 엔진" in the header).
Set `VITE_BOT_URL` to change the bot address (default `http://127.0.0.1:8788`).
