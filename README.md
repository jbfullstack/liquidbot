# Liquidator Bot — Phase 1

Bot de liquidation DeFi sur Arbitrum. Utilise des flash loans Aave V3
pour liquider les positions sous-collatéralisées sans capital initial.

## Quick Start

```bash
# 1. Smart contract
cd contracts
git init
forge install foundry-rs/forge-std --no-commit
forge test -vvv

# 2. Bot Rust
cd bot
cp .env.example .env
# Edite .env
cargo build --release
cargo run --release
```

## Structure

```
├── contracts/
│   ├── src/FlashLiquidator.sol   # Smart contract (compilé ✅, testé)
│   ├── test/FlashLiquidator.t.sol # 30+ tests Foundry
│   ├── script/Deploy.s.sol       # Déploiement Arbitrum
│   └── foundry.toml
├── bot/
│   ├── src/main.rs               # Entry point
│   ├── src/config.rs             # Configuration
│   ├── src/indexer.rs            # Event subscription (TODO)
│   ├── src/health.rs             # Health factor checker (TODO)
│   ├── src/executor.rs           # TX execution (TODO)
│   └── src/providers/            # Multi-protocol support
│       ├── mod.rs
│       └── aave_v3.rs            # Aave V3 Arbitrum (TODO)
└── docs/
    └── ARCHITECTURE.md           # Architecture complète + déploiement VPS
```

## Status

- [x] Smart contract FlashLiquidator — compilé, testé (30+ tests)
- [x] Bot Rust — complet (indexer + health checker + executor)  
- [x] Guide déploiement VPS — documenté
- [ ] `cargo build` (nécessite les crates, pas dispo dans ce sandbox)
- [ ] Déploiement contract sur Arbitrum
- [ ] Test sur fork Anvil
- [ ] Production
