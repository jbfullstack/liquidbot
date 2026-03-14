# Liquidator Bot — Architecture & Deployment Guide

## Vue d'ensemble

Un bot en Rust qui surveille les positions à risque sur Aave V3 (Arbitrum)
et les liquide automatiquement via flash loans. Zero capital nécessaire
au-delà du gas (~$30 en ETH sur le hot wallet).

## Comment ça gagne de l'argent

```
Exemple concret :
  Un utilisateur a emprunté $10 000 USDC contre $15 000 en WETH.
  ETH chute de 10% → son health factor passe sous 1.0.
  
  Notre bot :
  1. Flash loan $5 000 USDC (gratuit, remboursé dans la même tx)
  2. Rembourse 50% de sa dette ($5 000 USDC)
  3. Reçoit $5 250 en WETH (5% bonus de liquidation)
  4. Swap WETH → USDC sur Uniswap ($5 247 après 0.05% fee)
  5. Rembourse flash loan $5 000 + $2.50 fee
  6. Profit net : ~$244.50 → envoyé au cold wallet
  
  Coût : ~$0.20 de gas sur Arbitrum
  Temps total : ~300ms (1 transaction atomique)
```

## Stack technique

| Composant | Technologie | Raison |
|-----------|-------------|--------|
| Bot | Rust + alloy | Performance, WebSocket natif, fiabilité |
| Smart contract | Solidity + Foundry | Standard, testé, auditable |
| Blockchain | Arbitrum One | Pas de mempool publique (FCFS), gas $0.05-0.30 |
| RPC | Alchemy / QuickNode WebSocket | Blocs reçus en <50ms, events en temps réel |
| VPS | Vultr High Frequency (New York) | $6/mois, proche du sequencer Arbitrum |
| Cold wallet | Ledger | Profits en sécurité, jamais connecté au code |

## Architecture du bot

```
┌─────────────────────────────────────────────────────────┐
│                    Liquidator Bot (Rust)                  │
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │  EventIndexer │  │ HealthChecker│  │   Executor    │  │
│  │              │  │              │  │              │  │
│  │ Subscribe to │  │ Each block:  │  │ When HF < 1: │  │
│  │ Aave events  │──▶ batch check  │──▶ 1. Simulate  │  │
│  │ Build user   │  │ at-risk users│  │ 2. Profit ok? │  │
│  │ index        │  │ via multicall│  │ 3. Send tx    │  │
│  └──────────────┘  └──────────────┘  └──────┬───────┘  │
│                                              │          │
└──────────────────────────────────────────────┼──────────┘
                                               │
                              ┌────────────────▼──────────┐
                              │   FlashLiquidator.sol      │
                              │   (on-chain, Arbitrum)     │
                              │                            │
                              │   Flash loan debt token    │
                              │   → liquidationCall()      │
                              │   → swap if needed         │
                              │   → repay flash loan       │
                              │   → sweep to cold wallet   │
                              └────────────────────────────┘
```

### Module 1: EventIndexer

Souscrit via WebSocket aux événements Aave V3 :
- `Supply`, `Borrow`, `Repay`, `Withdraw`, `LiquidationCall`

Maintient un index en mémoire (DashMap) de tous les emprunteurs actifs
avec leurs positions. Cet index permet de savoir instantanément qui est
à risque quand les prix bougent.

### Module 2: HealthChecker

À chaque nouveau bloc :
1. Identifie les users avec HF estimé < 1.05 (zone de danger)
2. Batch-call `getUserAccountData()` via multicall (100 users en 1 RPC call)
3. Si HF < 1.0 → push vers le module Executor

Optimisation clé : on ne check pas les 10 000+ users à chaque bloc.
On ne check que ceux dont le HF est proche du seuil, basé sur les
prix courants.

### Module 3: Executor

Quand un user liquidatable est détecté :
1. Détermine le meilleur collateral à saisir (plus gros bonus)
2. Calcule le montant optimal de dette à couvrir
3. `eth_estimateGas` sur la tx de liquidation (simulation gratuite)
4. Si ça revert → skip (pas profitable ou déjà liquidé)
5. Si ça passe → calcule profit net (bonus - flash fee - gas - swap fee)
6. Si profit > min → envoie la tx
7. Log le résultat

## Déploiement VPS — Recommandations

### Le sequencer Arbitrum

Le sequencer Arbitrum tourne sur AWS. Les transactions arrivent en FCFS
(first-come, first-served). La latence entre ton VPS et le sequencer
est critique.

### Meilleur setup prix/perf

**Option 1 — Vultr High Frequency (recommandé pour commencer)**
- Localisation : **New York (NJ)** — le plus proche d'AWS us-east-1
- Plan : 1 vCPU, 1GB RAM, 32GB NVMe — **$6/mois**
- Latence estimée au sequencer : 5-15ms
- Suffisant pour le bot Rust (très léger en mémoire)

**Option 2 — Hetzner Cloud (alternative EU, moins cher)**
- Localisation : Ashburn, VA (USA) ou Falkenstein
- Plan : CX22, 2 vCPU, 4GB RAM — **€4.50/mois**
- Latence : 10-30ms depuis US, 80-100ms depuis EU
- Moins bon mais beaucoup moins cher

**Option 3 — AWS EC2 t3.micro (free tier, pour tester)**
- Localisation : us-east-1 (Virginia)
- Plan : Free tier 12 mois, puis ~$8/mois
- Latence : 3-10ms (même datacenter que le sequencer)
- Meilleure latence possible

### RPC Provider

Tu as besoin d'un WebSocket RPC pour recevoir les blocs en temps réel.

| Provider | Gratuit | Payant | WebSocket |
|----------|---------|--------|-----------|
| Alchemy | 300M compute units/mois | $49/mois Growth | ✅ |
| QuickNode | 10M credits/mois | $49/mois | ✅ |
| Infura | 100k req/jour | $50/mois | ✅ |
| Public RPC | illimité | gratuit | ❌ (HTTP only) |
| Ankr | 30 req/sec | $0 premium tier | ✅ |

**Recommandation** : Commence avec **Alchemy free tier** (300M CU/mois).
C'est suffisant pour un bot qui fait ~1000 RPC calls/minute.
Upgrade à $49/mois seulement si tu dépasses les limites.

### Budget mensuel minimal

| Poste | Coût |
|-------|------|
| VPS Vultr HF | $6 |
| RPC Alchemy free | $0 |
| Gas ETH hot wallet | ~$5-10 (rechargé) |
| **Total** | **~$11-16/mois** |

## Plan d'action — étapes concrètes

### Semaine 1 : Fondations

```bash
# 1. Crée ton hot wallet (MetaMask ou via CLI)
#    ⚠️ Ce wallet ne contiendra que ~$30 en ETH pour gas

# 2. Crée un compte Alchemy
#    → https://www.alchemy.com
#    → Crée une app "Arbitrum Mainnet"
#    → Note l'URL HTTPS et WebSocket

# 3. Déploie le smart contract
cd contracts
forge install foundry-rs/forge-std --no-commit
forge test -vvv  # vérifie que tout passe

# Crée .env
echo 'PRIVATE_KEY=0x...' >> .env
echo 'COLD_WALLET=0x...' >> .env
echo 'ARBITRUM_RPC_URL=https://arb-mainnet.g.alchemy.com/v2/...' >> .env

# Déploie
forge script script/Deploy.s.sol \
  --rpc-url $ARBITRUM_RPC_URL \
  --broadcast --verify

# 4. Crée le VPS
#    → https://www.vultr.com
#    → New York (NJ), High Frequency, 1CPU/1GB, Ubuntu 24
#    → SSH key auth (pas de password !)
```

### Semaine 2 : Bot Rust — implémentation core

```bash
# Sur le VPS :
# Installe Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone et build
git clone <ton-repo> liquidator
cd liquidator/bot
cp .env.example .env
# Edite .env avec tes clés

cargo build --release
```

Implémente dans cet ordre :
1. `config.rs` — chargement env ✅ (fait)
2. `providers/aave_v3.rs` — connexion Aave, getUserAccountData
3. `indexer.rs` — subscription WebSocket aux events
4. `health.rs` — batch health check avec multicall
5. `executor.rs` — simulation + envoi tx

### Semaine 3 : Test sur fork + production

```bash
# Test sur fork Arbitrum local
anvil --fork-url $ARBITRUM_RPC_URL --fork-block-number latest

# Le bot se connecte au fork, simule des liquidations
# Pas de risque réel

# Quand stable → passer en production
cargo run --release
```

### Semaine 4+ : Optimisation et expansion

- Ajouter Compound V3, Dolomite, Silo comme providers
- Ajouter multicall pour batch les health checks
- Monitorer les oracle updates Chainlink pour backrunning
- Ajouter des métriques (Prometheus/Grafana sur le VPS)

## Sécurité

- **Hot wallet** : ne contient que ~$30 ETH. Rechargé manuellement.
- **Cold wallet** : Ledger hardware. Tous les profits y vont automatiquement.
- **Cold wallet immutable** : impossible à changer dans le smart contract.
- **Pas de transferOwnership** : personne ne peut prendre le contrôle.
- **Le contrat ne garde jamais de tokens** : sweep après chaque liquidation.
- **VPS** : SSH key only, fail2ban, firewall UFW, pas de password login.

## Monitoring

```bash
# Sur le VPS, un simple screen/tmux suffit pour commencer
screen -S bot
cd /root/liquidator/bot
RUST_LOG=info cargo run --release

# Ctrl+A, D pour détacher
# screen -r bot pour rattacher

# Plus tard : systemd service
sudo cat > /etc/systemd/system/liquidator.service << EOF
[Unit]
Description=Liquidator Bot
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=/root/liquidator/bot
ExecStart=/root/liquidator/bot/target/release/liquidator-bot
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable liquidator
sudo systemctl start liquidator
sudo journalctl -u liquidator -f  # voir les logs
```

## Risques et mitigations

| Risque | Impact | Mitigation |
|--------|--------|------------|
| TX échoue (déjà liquidé par un autre bot) | Perte gas (~$0.20) | Simulation via estimateGas avant envoi |
| Smart contract bug | Perte des tokens en transit | Pas de tokens stockés, flash loan atomique |
| VPS down | Manque des opportunités | systemd auto-restart, monitoring |
| Clé privée volée | Perte du gas ETH (~$30) | Cold wallet protège les profits |
| RPC rate limited | Bot ralenti | Upgrade Alchemy si nécessaire |
| Compétition avec d'autres bots | TX arrive en retard | VPS proche du sequencer, WebSocket |
