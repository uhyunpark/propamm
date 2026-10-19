# Conventions

## Workflow
- `make check` = fmt-check + clippy `-D warnings` + sbf build + tests. It is the gate before every commit.
- Only failures go into the conversation. Style is never reviewed by hand.
- Pre-commit hook runs fmt-check + lint. Files in `.githooks`; enable with `git config core.hooksPath .githooks`.
- A Claude Code PostToolUse hook runs rustfmt on edited `.rs` files (`.claude/settings.json`).
- Review gate, in order, before a commit:
  1. `make check` passes.
  2. A review subagent checks the diff for duplication, needless complexity, convention drift, and stale docs, and fixes what it finds.
  3. The lead reviews logic and security (rounding, account checks, signer and authority paths).

## Lint policy
- Workspace `[workspace.lints]`:
  - clippy pedantic (warn), minus `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc`.
  - Plus `dbg_macro`, `todo`, `undocumented_unsafe_blocks`.
- Safety rules are crate-level attributes. Cargo cannot mix `[lints] workspace = true` with per-crate additions.
  - pricing + program: `#![deny(clippy::arithmetic_side_effects, clippy::unwrap_used, clippy::expect_used)]`.
  - pricing also: deny `panic`, `indexing_slicing`; forbid unsafe; warn `missing_docs`.
  - Bot is excluded from `arithmetic_side_effects` (f64 math).
- `clippy.toml` allows unwrap/expect/panic/indexing in tests.
- rustfmt: stable options only.

## Code
- Units in names: `_atoms`, `_bps`, `_slots`, `_ms`.
- On-chain math is integer and checked. No floats on-chain.
- Rounding goes only through `mul_div_floor` / `mul_div_ceil`.
- ExactIn only.
- Errors:
  - pricing: own enum.
  - program: `#[error_code]`.
  - bot: thiserror in libs, anyhow at the binary edge.
- Logging: `tracing` in the bot. No `msg!` in on-chain hot paths.

## Tests
- Unit tests in-module.
- Integration tests in a tests crate with LiteSVM.
- proptest for pricing invariants.
- Scenario test names:
  - `update_first`
  - `update_late`
  - `stale_rejected`
  - `split_order`
  - `compromised_updater`

## Git
- Conventional Commits in English: feat/fix/test/refactor/docs/chore. Scope = crate, e.g. `feat(pricing): ...`.
- Small commits.
- Never commit secrets or keypairs. Keypairs live outside the repo.

## Layout
| Path | Content |
|------|---------|
| `crates/pricing` | pricing core |
| `crates/bot` | feed collector, fair-value, update sender |
| `crates/jupiter-sdk` | Jupiter `Amm` impl |
| `programs/propamm` | Anchor program |
| `crates/propamm-tests` | LiteSVM integration tests |
| `web` | demo page |
| `docs` | documentation |

- Each crate gets its own short `CLAUDE.md` when created (progressive disclosure).

## Web
- bun + Biome (one tool for lint + format).
- TypeScript strict.

## Docs
- Internal docs are in English.
