# propamm-client

- Single place that encodes the program's wire format: program id, PDAs (`MarketKeys`), discriminator consts, instruction builders, `decode_market`, `token_amount`, `error_name`, `decode_swap_event`.
- Dependency-light on purpose (bytemuck, pricing, `solana-pubkey`, `solana-instruction`) so bot, Jupiter adapter and the later WASM client share it. Keypairs, RPC and signing stay in the bot.
- `tests/parity.rs` asserts builder output equals Anchor's `InstructionData` and `ToAccountMetas`, the discriminators equal sha256, and error names match `PropAmmError`. A program change (account order, args, errors) fails there first.
- Hand-written Borsh: all args are fixed-size integers, pubkeys and enums, so no borsh dependency.
