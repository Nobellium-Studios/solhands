# Rock Paper Scissors Dapp

![Verified Build](https://img.shields.io/badge/Solana-Verified%20Build-brightgreen.svg)

Copyright (c) 2025
Nobellium Studio Kft.
Author: Koray Çil (GitHub: leonx99x)
All Rights Reserved.

> On-chain Rock Paper Scissors powered by Anchor and Solana PDAs.

---

## Overview

The Solana dapp workspace lives in `rock_paper_scissors_dapp/` and contains the
on-chain program, tests, and supporting scripts for the Rock Paper Scissors
game.

## Quick Facts

| Area | Detail |
| --- | --- |
| Workspace | `rock_paper_scissors_dapp/` |
| Program Framework | Anchor (Rust) |
| Testing | Anchor + TypeScript |
| Tooling | Scripts for setup and utilities |

## Repository Layout

| Path | Purpose |
| --- | --- |
| `rock_paper_scissors_dapp/Anchor.toml` | Anchor configuration |
| `rock_paper_scissors_dapp/programs/` | Anchor program source |
| `rock_paper_scissors_dapp/tests/` | Anchor/TypeScript tests |
| `rock_paper_scissors_dapp/scripts/` | Helper scripts (e.g., VRF setup) |

## Architecture and Patterns

| Pattern | How it is used |
| --- | --- |
| Anchor program layout | Instruction handlers and typed accounts with validation |
| PDA-based state | Deterministic account derivation for games and vaults |
| Commit-reveal | Prevents move front-running and preserves fairness |
| Slot-based timeouts | Bounds game duration and supports safe cancellation |
| Deterministic accounts | Seeds and bumps map the same inputs to the same accounts |
