# propamm

Solana PropAMM: on-chain program, market-maker bot, and a web demo.

## Commands
- `make check`: fmt-check + clippy `-D warnings` + sbf build + tests (needs the Solana toolchain on PATH, see `docs/toolchain.md`). This is the gate for every change. Report only failures.
- `make fmt`: format everything.

## Rules
- Tooling enforces style. Reviews cover logic, security, and design only.
- On-chain and pricing math uses integers only, with checked ops and rounding that favors the maker.
- Pin versions, commit `Cargo.lock`, and never run a blanket `cargo update`.
- Write commits as Conventional Commits in English.

## Read when relevant
| When you are... | Read |
|---|---|
| touching the program or the pricing crate | `docs/design-onchain.md` |
| touching the bot or fair-value logic | `docs/design-oracle.md` |
| adding deps, installing tools, or hitting build errors | `docs/toolchain.md` |
| unsure about naming, errors, tests, or git | `docs/conventions.md` |

A crate may have its own short `CLAUDE.md` with crate-specific notes.
