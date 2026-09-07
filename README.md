# Kairos
A Rust-based DeFi arbitrage system: mempool ingestion, pool state, opportunity detection, simulation, and execution.

## Setup

After cloning, enable the repo's git hooks (pre-commit runs `forge fmt --check` + `forge build`; pre-push runs `forge test`):

```
git config core.hooksPath .githooks
```
