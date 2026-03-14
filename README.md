# Liquidator Bot — Phase 1

Bot de liquidation DeFi sur Arbitrum. Utilise des flash loans Aave V3
pour liquider les positions sous-collatéralisées sans capital initial.

## Quick Start

```bash
# 1. Smart contract
cd contracts
git init
forge install foundry-rs/forge-std
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


# Est-ce qu'on peut stopper ?

  Oui — setPaused(true) est déjà dans le contrat

  ./contracts/script/stop-smart-contract.sh

  # Mettre en pause immédiatement
  cast send 0xCONTRACT_ADDRESS \
    "setPaused(bool)" true \
    --private-key 0xTA_CLÉ \
    --rpc-url https://TON_QUICKNODE

  Quand paused = true → toute tentative de liquidation revert. Le bot est neutralisé instantanément.

  Ce qu'on ne peut PAS faire :
  - Supprimer le contrat (pas de selfdestruct) — il reste sur la blockchain pour toujours mais inactif si paused
  - Changer coldWallet ou owner — immutables par design, c'est une feature de sécurité

# VPS

ssh root@108.61.159.79


# guide VPS complet. Remplace <VPS_IP> par l'IP de ton Vultr :

  ---
  1. Depuis ta machine Windows — copier les fichiers

  # Dans WSL (Ubuntu)
  rsync -avz --exclude 'target/' \
    /mnt/d/dev-web3/liquidator-bot/ \
    root@<VPS_IP>:/root/liquidator-bot/

  ---
  2. Sur le VPS — installation

  ssh root@<VPS_IP>

  # Mise à jour OS
  apt update && apt upgrade -y

  # Rust
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source $HOME/.cargo/env

  # Vérifie
  rustc --version

  ---
  3. Build du bot

  cd /root/liquidator-bot/bot

  # Vérifie le .env (déjà copié par rsync)
  cat .env

  cargo build --release
  # ~5-10 min la première fois

  ---
  4. Service systemd (redémarre auto si crash ou reboot)

  cat > /etc/systemd/system/liquidator.service << 'EOF'
  [Unit]
  Description=Aave V3 Liquidator Bot
  After=network-online.target
  Wants=network-online.target

  [Service]
  Type=simple
  User=root
  WorkingDirectory=/root/liquidator-bot/bot
  ExecStart=/root/liquidator-bot/bot/target/release/liquidator-bot
  Restart=always
  RestartSec=10
  Environment=RUST_LOG=info

  [Install]
  WantedBy=multi-user.target
  EOF

  systemctl daemon-reload
  systemctl enable liquidator
  systemctl start liquidator

  ---
  5. Voir les logs en direct

  journalctl -u liquidator -f

  Tu devrais voir en quelques secondes :
  ✅ Connected. Flash premium: 5 bps
  💲 ETH price: $3487.23 (Chainlink)
  ✅ ETH balance: 0.002800
  📡 Indexing borrowers from block ...
  Found 847 unique borrowers
  ✅ 312 users tracked, 2 at risk
  🔄 Listening for new blocks...
  💓 Block 441300000 | 312 tracked | 2 at-risk | 0 liq ($0.00 gross) | ETH $3487

  ---
  6. Commandes utiles

  systemctl stop liquidator      # arrêter
  systemctl start liquidator     # démarrer
  systemctl restart liquidator   # redémarrer (après update)
  journalctl -u liquidator -n 100  # 100 dernières lignes
  journalctl -u liquidator --since "1 hour ago"  # dernière heure

  ---
  7. Mise à jour future

  # Depuis Windows/WSL :
  rsync -avz --exclude 'target/' \
    /mnt/d/dev-web3/liquidator-bot/ \
    root@<VPS_IP>:/root/liquidator-bot/

  # Sur le VPS :
  cd /root/liquidator-bot/bot
  cargo build --release
  systemctl restart liquidator

  # Arrêt immédiat du bot (sur le VPS)
  systemctl stop liquidator

  # OU si tu veux garder le bot actif mais bloquer toute exécution de liquidation,
  # pause le smart contract depuis n'importe où avec le script :
  cd /root/liquidator-bot/contracts
  ./stop-smart-contract.sh

# La stratégie complète

  Ce que le bot cherche

  Santé financière d'un emprunteur = Health Factor (HF)

  HF = (collateral × liquidation_threshold) / dette

  HF > 1.0  → position saine, intouchable
  HF < 1.0  → position liquidable → TON BOT AGIT

  Exemple concret :
  Alice dépose 1 ETH ($2000) comme collateral
  Alice emprunte $1400 USDC  (threshold 80% → HF = 1600/1400 = 1.14)

  ETH tombe à $1600
  → HF = (1600 × 0.80) / 1400 = 1280/1400 = 0.91  ← LIQUIDABLE ✅

  Ce que le smart contract fait (atomique, une seule TX)

  ┌─────────────────────────────────────────────────────┐
  │  1. Flash loan : emprunte $700 USDC à Aave           │
  │     (gratuit, remboursé dans la même TX)             │
  │                                                      │
  │  2. liquidationCall() sur Aave :                     │
  │     → Repaye $700 de la dette d'Alice                │
  │     → Reçoit $735 en WETH (5% bonus liquidation)    │
  │                                                      │
  │  3. Swap WETH → USDC sur Uniswap V3 :               │
  │     → $735 WETH → ~$733 USDC (0.3% frais swap)     │
  │                                                      │
  │  4. Rembourse le flash loan : $700 + $0.35 (0.05%) │
  │                                                      │
  │  5. Profit net = $733 - $700.35 = ~$32.65           │
  │     → envoyé automatiquement à ton cold wallet      │
  └─────────────────────────────────────────────────────┘

  Coût total pour toi : ~$0.20 de gas sur Arbitrum
  Si non profitable → TX revert → tu perds 0

  Le rôle du bot vs le contrat

  BOT (Rust)                          CONTRAT (Solidity)
  ──────────────────────────────────  ──────────────────────────
  Surveille les blocs WebSocket       Exécute l'atomique
  Calcule qui est liquidable          Flash loan Aave
  Choisit debt + collateral           liquidationCall
  Estime le profit                    Swap Uniswap V3
  Filtre si pas rentable              Vérifie minProfit
  Envoie la TX                        Envoie profit au cold wallet
  Notifie Telegram                    Revert si pas rentable