# CLAUDE.md — Project Context for AI Agents

## What is this project?

A **DeFi liquidation bot** that monitors Aave V3 on Arbitrum One for undercollateralized lending positions and liquidates them automatically using **flash loans** (zero capital required, only gas in ETH). Profits are swept to a cold wallet (Ledger) after every successful liquidation.

The owner is a solo developer in France with a few hundred euros of capital. The goal is **$10k/year passive income** from liquidation bonuses (5-10% of liquidated debt). The bot runs 24/7 on a cheap VPS ($6/month Vultr, New York).

## How it makes money

```
1. Someone borrows $10,000 USDC against $15,000 WETH on Aave V3
2. ETH price drops → their health factor falls below 1.0
3. Our bot detects this via WebSocket block subscription
4. In ONE atomic transaction:
   a. Flash loan $5,000 USDC from Aave (free, repaid same tx)
   b. Call liquidationCall() → repay their $5,000 debt
   c. Receive $5,250 in WETH (5% liquidation bonus)
   d. Swap WETH → USDC on Uniswap V3
   e. Repay flash loan + 0.05% fee
   f. Net profit ~$245 → sent to cold wallet
5. Cost: ~$0.20 gas on Arbitrum
```

## Project structure

```
├── CLAUDE.md                          ← you are here
├── README.md                          ← user-facing readme
│
├── contracts/                         ← Solidity (Foundry)
│   ├── src/FlashLiquidator.sol        ← main contract (363 lines, COMPILES ✅)
│   ├── test/FlashLiquidator.t.sol     ← 23 tests + fuzz + invariants (397 lines)
│   ├── script/Deploy.s.sol            ← Arbitrum deployment script
│   ├── foundry.toml
│   └── remappings.txt
│
├── bot/                               ← Rust (alloy + tokio)
│   ├── Cargo.toml
│   ├── .env.example                   ← all config vars documented
│   └── src/
│       ├── main.rs                    ← full bot logic (509 lines)
│       ├── config.rs                  ← env config loader (50 lines)
│       ├── stats.rs                   ← persistent JSON stats tracker (205 lines)
│       └── telegram.rs               ← notifications + command handler (439 lines)
│
└── docs/
    └── ARCHITECTURE.md                ← full architecture, VPS guide, deployment steps
```

## Tech stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Smart contract | Solidity 0.8.23, Foundry | Industry standard, well-tested |
| Bot | Rust, alloy crate, tokio | Speed (WebSocket, <50ms reaction), reliability |
| Chain | Arbitrum One (L2) | Cheap gas ($0.05-0.30), FCFS sequencer, less MEV competition |
| Notifications | Telegram Bot API | Free, instant push, command interface |
| Persistence | stats.json on disk | Simple, survives restarts, exportable |
| VPS | Vultr High Frequency, New York | $6/month, ~10ms to Arbitrum sequencer |
| RPC | Alchemy (free tier, WebSocket) | Real-time block subscription |

## Smart contract: FlashLiquidator.sol

### Status: COMPILES ✅ | Tests: NEED `forge test` VERIFICATION

### Key design decisions
- `owner` and `coldWallet` are **immutable** (set at deploy, never changeable)
- Flash loan + liquidation + swap + repay all in one atomic tx
- `forceApprove` pattern for USDT-like tokens (revert if allowance != 0)
- `minProfit` parameter: tx reverts if profit below threshold (lose gas only, never funds)
- `minSwapOut` parameter: slippage protection on Uniswap swap
- Struct `LiqParams` used to avoid stack-too-deep
- `nonReentrant` guard on entry point

### Interfaces used
- **Aave V3 Pool**: `flashLoanSimple()`, `liquidationCall()`, `getUserAccountData()`
- **Uniswap V3 Router**: `exactInputSingle()` (for collateral → debt token swap)
- **ERC20**: standard + `allowance()` for forceApprove pattern

### Arbitrum One addresses (in Deploy.s.sol)
- Aave PoolAddressesProvider: `0xa97684ead0e402dC232d5A977953DF7ECBaB3CDb`
- Aave Pool (derived): `0x794a61358D6845594F94dc1DB02A252b5b4814aD`
- Aave DataProvider: `0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654`
- Uniswap V3 Router: `0xE592427A0AEce92De3Edee1F18E0157C05861564`

### Test coverage (FlashLiquidator.t.sol)
- A: Deployment (owner, coldWallet, defaults, revert on zero)
- B: Access control (onlyOwner, onlyAavePool, paused)
- C: Same-token liquidation (collateral == debt, e.g. USDC/USDC)
- D: Cross-token liquidation (collateral != debt, involves Uniswap swap)
- E: Profitability checks (revert if not profitable, revert if below minProfit)
- F: Cold wallet guarantees (profits only to cold, contract always empty after)
- G: Multiple sequential liquidations
- H: USDT forceApprove edge case (residual allowance)
- I: Admin (rescue tokens, rescue ETH, pause)
- J: View helpers (getHealthFactor, getFlashPremium)
- K: Fuzz tests (onlyOwner, slippage bounds)
- L: Invariants (no residual tokens, immutable cold/owner)

### Known issue in tests
- `console2.log` with 4+ args may fail on some forge-std versions. The 5-arg call was already fixed. Line 317 still has a 4-arg call (`"text", value, "text", value`) — verify it compiles with your forge-std version.

## Rust bot: main.rs

### Status: CODE COMPLETE ✅ | NOT YET COMPILED (needs `cargo build` with network access)

### Architecture
```
tokio::main
├── Setup: wallet, providers (HTTP + WebSocket), contract instances
├── Verify: owner check, pause check, flash premium, ETH balance
├── Index: fetch Borrow events from last 50k blocks → build user HashMap
├── Spawn: Telegram command listener (separate task, long-polling)
└── Main loop (WebSocket block subscription):
    ├── Collect at-risk users (HF < threshold from index)
    ├── Batch getUserAccountData() for each at-risk user
    ├── If HF < 1.0:
    │   ├── Find best debt token (largest variable debt)
    │   ├── Find best collateral (largest aToken balance, enabled as collateral)
    │   ├── Determine close factor (50% or 100% based on HF)
    │   ├── estimateGas() → FREE simulation, reverts = skip
    │   ├── Check gas cost < 5% of debt
    │   └── Send tx → wait receipt → record stats → notify Telegram
    └── Update shared state for /status command
```

### Performance-critical design
- **WebSocket** block subscription (not HTTP polling) — reacts in <100ms to new blocks
- **estimateGas as free simulation** — if it reverts, skip (no gas spent)
- **Telegram notifications are fire-and-forget** via `tokio::spawn` — NEVER block the main loop
- **Telegram command listener** uses long-polling (5min timeout) in separate task — zero CPU cost
- **No Telegram calls before or during tx execution** — all notifications happen AFTER result

### Dependencies (Cargo.toml)
- `alloy 0.15` (full, provider-ws, signer-local) — Ethereum interaction
- `tokio` (full) — async runtime
- `reqwest` (json, multipart) — Telegram API
- `tracing` + `tracing-subscriber` — structured logging
- `serde` + `serde_json` — stats serialization
- `chrono` — timestamps and monthly aggregation
- `dashmap` — concurrent hashmap (declared but currently using std HashMap + RwLock)
- `futures-util` — StreamExt for block subscription
- `eyre` — error handling
- `dotenv` — .env loading

## Stats module: stats.rs

Persists all liquidation records to `stats.json` on disk. Survives bot restarts. Provides:
- `record_liquidation()` — appends record, saves to disk
- `format_summary()` — generates full P&L text with:
  - Total profit since day 1
  - Yearly breakdown with monthly detail
  - Best/worst month per year with bar chart visualization
  - Projection (/day, /month, /year) based on historical daily average
  - Success/failure counts, total gas burned

## Telegram module: telegram.rs

### Notifications (fire-and-forget, non-blocking)
- `notify_startup` — bot started, wallet addresses, ETH balance, users tracked
- `notify_liquidation_complete` — SINGLE message combining attempt details + result + P&L summary
- `notify_simulation_skip` — gas too high, position too small
- `notify_error` — RPC errors, tx failures
- `notify_eth_sweep` — ETH sent to cold wallet
- `notify_low_eth` — balance warning

### Commands (long-polling listener, separate tokio task)
- `/status` — is bot alive, uptime, users tracked, at-risk count, quick P&L
- `/stats` — full historical P&L with yearly/monthly breakdown
- `/json` — download stats.json as Telegram document
- `/help` — list commands

### Security
- Only responds to messages from the configured `TELEGRAM_CHAT_ID`
- Skips old messages on startup (fetches latest update_id first)

## What needs to be done next

### Immediate (before first production run)
1. **`cargo build --release`** — verify Rust compilation (needs network for crate downloads)
2. **`forge test -vvv`** — verify all 23 Solidity tests pass
3. **Fix remaining `console2.log`** if forge-std version doesn't support 4-arg overload (line 317 in test file)
4. **Deploy contract** to Arbitrum One via `forge script`
5. **Create Telegram bot** via @BotFather, get token + chat_id
6. **Setup VPS** (Vultr HF New York, $6/month), install Rust, clone repo
7. **Test on Anvil fork** (`anvil --fork-url $RPC`) before going live

### Phase 2 (after initial production run)
- Add more lending protocols: Compound V3, Dolomite, Silo, Radiant (all on Arbitrum)
- Add multi-DEX support for the collateral swap (Camelot, SushiSwap, not just Uniswap)
- Multicall batching for health factor checks (100 users in 1 RPC call)
- Oracle backrunning: watch Chainlink price update events, liquidate immediately after
- Cross-chain expansion: Base, Optimism, Polygon

### Phase 3 (optimization)
- Arbitrage module: multi-DEX price discrepancies on long-tail tokens
- Rust-native Uniswap quoter (avoid RPC call for swap quote)
- Private mempool / Flashbots-style submission if frontrunning becomes an issue

## Environment variables (.env)

```
ARBITRUM_WS_URL=wss://arb-mainnet.g.alchemy.com/v2/KEY    # WebSocket REQUIRED
ARBITRUM_RPC_URL=https://arb-mainnet.g.alchemy.com/v2/KEY  # HTTP for reads
PRIVATE_KEY=0x...                                           # Hot wallet (gas only)
COLD_WALLET=0x...                                           # Ledger address
CONTRACT_ADDRESS=0x...                                      # After deployment
MIN_PROFIT_USD=2                                            # Min profit to execute ($)
MAX_GAS_GWEI=1                                              # Gas price cap
ETH_KEEP=0.01                                               # ETH to keep in hot wallet
HF_THRESHOLD=1.05                                           # Start watching users below this
TELEGRAM_BOT_TOKEN=123:ABC...                               # From @BotFather
TELEGRAM_CHAT_ID=987654321                                  # Your chat ID
RUST_LOG=info                                               # Logging level
```

## Important constraints and decisions

1. **Zero capital model**: everything uses flash loans. Only ETH for gas (~$30) on the hot wallet.
2. **Cold wallet immutable**: even if hot wallet key is leaked, attacker can only waste gas, never steal profits.
3. **Atomic safety**: if liquidation isn't profitable, the entire tx reverts. You lose only gas (~$0.20).
4. **USDT approve quirk**: USDT reverts if you `approve(X)` when allowance is already non-zero. Contract uses `forceApprove` pattern (approve(0) then approve(X)).
5. **Arbitrum sequencer is FCFS**: no mempool frontrunning, but latency to sequencer matters. VPS in New York is optimal.
6. **`estimateGas` as free simulation**: if the call would revert on-chain (e.g. user already liquidated by someone else), estimateGas fails without costing gas. This is the primary filter.
7. **Telegram never blocks**: all notifications are `tokio::spawn` fire-and-forget. Command listener is a separate long-polling task with 5-minute timeout.
