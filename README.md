# Rock Paper Scissors Dapp

![Verified Build](https://img.shields.io/badge/Solana-Verified%20Build-brightgreen.svg)

Copyright (c) 2025
Nobellium Studio Kft.
Author: Koray Çil (GitHub: leonx99x)
All Rights Reserved.

## Overview

The Solana dapp workspace lives in `rock_paper_scissors_dapp/` and contains the
on-chain program, tests, and supporting scripts for the Rock Paper Scissors
game.

## Structure

- `rock_paper_scissors_dapp/Anchor.toml` - Anchor configuration
- `rock_paper_scissors_dapp/programs/` - Anchor program source
- `rock_paper_scissors_dapp/tests/` - Anchor/TypeScript tests
- `rock_paper_scissors_dapp/scripts/` - helper scripts (e.g., VRF setup)

## Architecture and Patterns

- **Anchor program layout**: Instruction handlers in `rock_paper_scissors_dapp/programs/` follow the standard Anchor module structure with account validation and typed state.
- **PDA-based state**: Program-derived addresses are used to namespace game state and vault accounts deterministically.
- **Commit-reveal flow**: Game logic relies on a commit-then-reveal pattern to prevent move front-running and preserve fairness.
- **Timeout governance**: Slot-based timeouts bound game duration and allow safe cancellation if a player never reveals or joins.
- **Deterministic accounts**: Seeds and bumps ensure the same inputs always map to the same on-chain account addresses.
