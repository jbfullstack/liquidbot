//! Liquidator Bot — Phase 1
//! Monitors Aave V3 on Arbitrum for undercollateralized positions
//! and liquidates them using flash loans.

mod config;
mod stats;
mod telegram;

use config::Config;
use stats::StatsStore;
use telegram::{TelegramNotifier, TelegramCommand};
use eyre::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
use alloy::primitives::{address, Address, I256, U256};

use alloy::{
    network::EthereumWallet,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::Filter,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolEvent,
};

// ═══════════════════════════════════════════════════════════════
// ABI bindings via alloy::sol!
// ═══════════════════════════════════════════════════════════════

sol! {
    #[sol(rpc)]
    interface IChainlinkAggregator {
        /// Returns (roundId, answer, startedAt, updatedAt, answeredInRound)
        /// answer has 8 decimal places: e.g. 350000000000 = $3500.00
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
    }

    #[sol(rpc)]
    interface IAavePool {
        function getUserAccountData(address user) external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

        function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);

        event Borrow(address indexed reserve, address onBehalfOf, address indexed user, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode);
    }

    #[sol(rpc)]
    interface IFlashLiquidator {
        function liquidate(
            address user,
            address collateralAsset,
            address debtAsset,
            uint256 debtToCover,
            uint24 swapFeeTier,
            uint256 minSwapOut,
            uint256 minProfit
        ) external;

        function getHealthFactor(address user) external view returns (uint256);
        function getFlashPremiumBps() external view returns (uint128);
        function paused() external view returns (bool);
        function owner() external view returns (address);
        function coldWallet() external view returns (address);
        function setPaused(bool _p) external;
    }

    #[sol(rpc)]
    interface IAaveDataProvider {
        function getUserReserveData(address asset, address user) external view returns (
            uint256 currentATokenBalance,
            uint256 currentStableDebt,
            uint256 currentVariableDebt,
            uint256 principalStableDebt,
            uint256 scaledVariableDebt,
            uint256 stableBorrowRate,
            uint256 liquidityRate,
            uint40 stableRateLastUpdated,
            bool usageAsCollateralEnabled
        );
    }

    /// Uniswap V3 QuoterV2 — returns swap output without reverting (unlike V1).
    /// Used to find the best fee tier for a collateral→debt swap before execution.
    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24  fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32  initializedTicksCrossed,
                uint256 gasEstimate
            );
    }
}

// ═══════════════════════════════════════════════════════════════
// Constants — Aave V3 Arbitrum One
// ═══════════════════════════════════════════════════════════════

const AAVE_POOL: Address          = address!("794a61358D6845594F94dc1DB02A252b5b4814aD");
const AAVE_DATA_PROVIDER: Address = address!("69FA688f1Dc47d4B5d8029D5a35FB7a548310654");
/// Chainlink ETH/USD price feed on Arbitrum One (8 decimals)
const CHAINLINK_ETH_USD: Address  = address!("639Fe6ab55C921f74e7fac1ee960C0B6293ba612");
/// Uniswap V3 QuoterV2 on Arbitrum One
const UNISWAP_QUOTER_V2: Address  = address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");

/// All active Aave V3 reserve tokens on Arbitrum One: (address, symbol, decimals)
const TOKENS: &[(Address, &str, u8)] = &[
    (address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"), "WETH",   18),
    (address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), "USDC",    6),
    (address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), "USDCe",   6),
    (address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), "USDT",    6),
    (address!("DA10009cBd5D07dd0CeCc66161FC93D7c9000da1"), "DAI",    18),
    (address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), "WBTC",    8),
    (address!("5979D7b546E38E9Ab8F25A6E70b5CdE5A8C7A1D0"), "wstETH", 18),
    (address!("912CE59144191C1204E64559FE8253a0e49E6548"), "ARB",    18),
    // Previously missing reserves — added after missed $211k opportunity
    (address!("f97f4df75117a78c1A5a0DBb814Af92458539FB4"), "LINK",   18),
    (address!("EC70Dcb4A1EFa46b8F2D97C310C9c4790ba5ffA4"), "rETH",   18),
    (address!("93b346b6BC2548dA6A1E7d98E9a421B42541425b"), "LUSD",   18),
];

// ═══════════════════════════════════════════════════════════════
// User tracking
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct UserPosition {
    health_factor: U256,
    total_collateral_base: U256,
    total_debt_base: U256,
    last_block: u64,
    /// Block at which this user should next be refreshed (priority-based).
    next_refresh_at: u64,
}

type UserIndex = Arc<RwLock<HashMap<Address, UserPosition>>>;

// ═══════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════

/// Fetch current ETH/USD price from Chainlink on Arbitrum.
/// Returns e.g. 3500.0. Falls back to `fallback` on error.
async fn fetch_eth_price<P>(provider: &P) -> Result<f64>
where
    P: Provider,
{
    let oracle = IChainlinkAggregator::new(CHAINLINK_ETH_USD, provider);

    let d = oracle.latestRoundData().call().await?;
    if d.answer <= I256::ZERO {
        eyre::bail!("Chainlink returned non-positive ETH price");
    }

    Ok(d.answer.to_string().parse::<f64>()? / 1e8)
}

/// Convert min_profit_usd → min_profit in debt token native units.
/// For stablecoins (6 dec, ~$1): multiply by 1e6.
/// For ETH-priced 18-dec tokens: divide by eth_price, multiply by 1e18.
/// For WBTC (8 dec): unknown price — use U256::ZERO to rely on simulation.
fn min_profit_raw(min_profit_usd: f64, debt_decimals: u8, eth_price_usd: f64) -> U256 {
    match debt_decimals {
        6  => U256::from((min_profit_usd * 1_000_000.0) as u128),
        18 => U256::from((min_profit_usd / eth_price_usd * 1e18) as u128),
        _  => U256::ZERO, // Unknown price ratio — estimateGas simulation is the guard
    }
}

/// How many blocks to wait before re-fetching this user's health factor.
///
/// Uses a quadratic curve on the distance above 1.0:
///   interval = (percentage_points_above_1.0)² / 4
///
/// | HF   | interval  | wall-clock (Arbitrum ~0.25s/block) |
/// |------|-----------|------------------------------------|
/// | 1.05 |   6 blocs |  ~1.5s — nearly liquidatable       |
/// | 1.10 |  25 blocs |  ~6s                               |
/// | 1.20 | 100 blocs |  ~25s                              |
/// | 1.50 | 625 blocs |  ~2.5 min                         |
/// | 2.00 |2500 blocs |  ~10 min                          |
/// | MAX  |   0       |  immediate (new borrower)          |
fn refresh_interval_blocks(hf: U256) -> u64 {
    if hf == U256::MAX {
        return 0; // new borrower, never fetched — refresh ASAP
    }
    const ONE_E18: u128 = 1_000_000_000_000_000_000;
    let hf_val = hf.saturating_to::<u128>();
    if hf_val <= ONE_E18 {
        return 1; // already liquidatable
    }
    // percentage points above 1.0 (e.g. HF=1.05 → pct=5, HF=1.20 → pct=20)
    let pct = (hf_val - ONE_E18) / (ONE_E18 / 100);
    (pct * pct / 4).clamp(1, 5_000) as u64
}

/// Add a new borrower to the index if not already present.
/// Called on every Borrow event (real-time WebSocket) and on initial scan.
/// HF is set to MAX (unknown) — will be fetched when the position becomes at-risk.
fn index_borrower(idx: &mut HashMap<Address, UserPosition>, user: Address) {
    idx.entry(user).or_insert(UserPosition {
        health_factor: U256::MAX,
        total_collateral_base: U256::ZERO,
        total_debt_base: U256::from(1u64), // non-zero placeholder until real fetch
        last_block: 0,
        next_refresh_at: 0, // refresh ASAP (HF=MAX triggers interval=0)
    });
}

/// Remove a user from the index if their debt has dropped to zero.
/// Called after every getUserAccountData fetch when we have fresh on-chain data.
fn remove_if_repaid(idx: &mut HashMap<Address, UserPosition>, user: &Address, total_debt_base: U256) {
    if total_debt_base == U256::ZERO {
        idx.remove(user);
        tracing::debug!("🗑️  Removed repaid borrower {user}");
    }
}

// ═══════════════════════════════════════════════════════════════
// User index persistence (BUG FIX: lookback too short on restart)
// ═══════════════════════════════════════════════════════════════

const INDEX_FILE: &str = "user_index.json";
/// Default lookback when no saved index exists: ~14 days on Arbitrum (~0.25s/block)
const DEFAULT_LOOKBACK: u64 = 4_000_000;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SavedIndex {
    last_saved_block: u64,
    addresses: Vec<String>,
}

fn load_saved_index() -> SavedIndex {
    std::fs::read_to_string(INDEX_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_index(idx: &HashMap<Address, UserPosition>, last_block: u64) {
    let count = idx.len();
    let saved = SavedIndex {
        last_saved_block: last_block,
        addresses: idx.keys().map(|a| format!("{a:#x}")).collect(),
    };
    match serde_json::to_string(&saved) {
        Ok(json) => {
            if let Err(e) = std::fs::write(INDEX_FILE, &json) {
                tracing::warn!("Failed to save user index: {e}");
            } else {
                tracing::debug!("💾 Saved {count} addresses to {INDEX_FILE} (block {last_block})");
            }
        }
        Err(e) => tracing::warn!("Failed to serialize user index: {e}"),
    }
}

// ═══════════════════════════════════════════════════════════════
// Uniswap routing — fee tier discovery + cache
// ═══════════════════════════════════════════════════════════════

/// Return 1 unit of `addr` in its native decimals (used as a safe QuoterV2 amount).
/// Dust amounts cause quoter to return 0; overflows cause it to revert.
/// 1 unit is always within the realistic liquidity range of any Uniswap V3 pool.
fn token_unit(addr: Address) -> U256 {
    let dec = TOKENS.iter()
        .find(|(a, _, _)| *a == addr)
        .map(|(_, _, d)| *d)
        .unwrap_or(18);
    U256::from(10u64).pow(U256::from(dec as u64))
}

/// How many blocks a cached fee tier stays valid before QuoterV2 re-check.
/// 10 000 blocks ≈ 40 min on Arbitrum (0.25 s/block).
/// Uniswap pool fee tiers never change; only liquidity distribution does.
/// estimateGas is the real safety net — this cache is a latency optimisation.
const CACHE_TTL_BLOCKS: u64 = 10_000;

/// Cached, symmetric fee-tier lookup.
///
/// Safety contract
/// ───────────────
/// A stale or wrong cached tier can only cause estimateGas to revert (which we
/// already handle with `continue`). We NEVER skip estimateGas. The cache is
/// therefore a pure latency optimisation — zero correctness risk.
///
/// Symmetry
/// ────────
/// A Uniswap V3 pool serves A→B *and* B→A at the same fee tier.
/// When we resolve a new pair we write both directions into the cache.
///
/// Latency saved
/// ─────────────
///   Cache hit  : 0 RPC calls  →   0 ms
///   Cache miss : 4 parallel   → ~50 ms  (vs ~200 ms sequential)
async fn cached_fee_tier<P: Provider>(
    cache: &mut HashMap<(Address, Address), (u32, u64)>,
    provider: &P,
    token_in: Address,
    token_out: Address,
    current_block: u64,
) -> Option<u32> {
    let key = (token_in, token_out);

    // ── Cache hit? ──
    if let Some(&(tier, verified_at)) = cache.get(&key) {
        let age = current_block.saturating_sub(verified_at);
        if age < CACHE_TTL_BLOCKS {
            tracing::debug!("  🗄️  cache hit {tier} bps (age {age} blk)");
            return Some(tier);
        }
        tracing::debug!("  🗄️  cache stale ({age} blk), refreshing QuoterV2");
    }

    // ── Cache miss / stale: 4 parallel QuoterV2 calls ──
    let tier = best_fee_tier(provider, token_in, token_out, token_unit(token_in)).await?;

    // Store both directions — same Uniswap V3 pool serves A→B and B→A
    cache.insert((token_in,  token_out), (tier, current_block));
    cache.insert((token_out, token_in),  (tier, current_block));
    tracing::info!("  🗄️  cache stored {tier} bps for pair (both dirs)");
    Some(tier)
}

/// Query all 4 Uniswap V3 fee tiers via QuoterV2 **in parallel** (tokio::join!)
/// and return the fee tier that produces the most output tokens.
///
/// Parallel = ~50ms vs ~200ms sequential — matters on Arbitrum FCFS.
/// Returns None if no pool has sufficient liquidity for this pair.
async fn best_fee_tier<P: Provider>(
    provider: &P,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Option<u32> {
    let q = IQuoterV2::new(UNISWAP_QUOTER_V2, provider);

    // Helper: build params for a given fee tier
    let p = |fee: u32| IQuoterV2::QuoteExactInputSingleParams {
        tokenIn:           token_in,
        tokenOut:          token_out,
        amountIn:          amount_in,
        fee:               alloy::primitives::Uint::<24, 1>::from(fee),
        sqrtPriceLimitX96: Default::default(), // 0 = no price limit
    };

    // Build CallBuilders first (must be bound to live long enough for join!)
    let cb500   = q.quoteExactInputSingle(p(500));
    let cb3000  = q.quoteExactInputSingle(p(3000));
    let cb100   = q.quoteExactInputSingle(p(100));
    let cb10000 = q.quoteExactInputSingle(p(10000));

    // 4 RPC calls in parallel — resolves in time of the slowest single call
    let (r500, r3000, r100, r10000) = tokio::join!(
        cb500.call(),
        cb3000.call(),
        cb100.call(),
        cb10000.call(),
    );

    let candidates = [
        (500u32,   r500.ok().map(|r| r.amountOut)),
        (3000u32,  r3000.ok().map(|r| r.amountOut)),
        (100u32,   r100.ok().map(|r| r.amountOut)),
        (10000u32, r10000.ok().map(|r| r.amountOut)),
    ];

    let best = candidates.iter()
        .filter_map(|(fee, out)| out.map(|o| (*fee, o)))
        .max_by_key(|(_, out)| *out);

    if let Some((fee, out)) = best {
        tracing::debug!("  QuoterV2 best route: {fee} bps → {out} out");
    } else {
        tracing::warn!("  QuoterV2: no viable route for {token_in} → {token_out}");
    }

    best.map(|(fee, _)| fee)
}

// ═══════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    dotenv::dotenv().ok();

    let cfg = Config::from_env()?;

    tracing::info!("═══════════════════════════════════════");
    tracing::info!("   Liquidator Bot v1.0 — Phase 1");
    tracing::info!("═══════════════════════════════════════");

    // ── Wallet setup ──
    let signer: PrivateKeySigner = cfg.private_key.parse()?;
    let wallet_addr = signer.address();
    let wallet = EthereumWallet::from(signer);
    tracing::info!("Hot wallet: {wallet_addr}");

    // ── Providers ──
    // http_ro: lecture pure (eth_call, eth_getLogs, eth_getBalance...) — sans wallet
    // http:    envoi de transactions signées uniquement
    let http_ro = ProviderBuilder::new()
        .connect_http(cfg.rpc_http_url.parse()?);

    let http = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect_http(cfg.rpc_http_url.parse()?);

    let ws = ProviderBuilder::new()
        .connect_ws(WsConnect::new(&cfg.rpc_ws_url))
        .await?;

    // ── Contracts ──
    let contract_addr: Address = cfg.contract_address.parse()?;
    // liquidator: lié à http (wallet) pour envoyer liquidate()
    let liquidator    = IFlashLiquidator::new(contract_addr, &http);
    // *_ro: liés à http_ro pour les appels view (eth_call sans vérification de solde)
    let liquidator_ro = IFlashLiquidator::new(contract_addr, &http_ro);
    let pool          = IAavePool::new(AAVE_POOL, &http_ro);
    let data_prov     = IAaveDataProvider::new(AAVE_DATA_PROVIDER, &http_ro);

    // ── Verify ──
    let owner = liquidator_ro.owner().call().await?;
    if owner != wallet_addr {
        eyre::bail!("Wallet {wallet_addr} is not owner ({owner})");
    }
    if liquidator_ro.paused().call().await? {
        eyre::bail!("Contract is paused");
    }
    let premium = liquidator_ro.getFlashPremiumBps().call().await?;
    tracing::info!("✅ Connected. Flash premium: {premium} bps");

    // ── ETH price from Chainlink (same oracle Aave uses) ──
    let mut eth_price_usd = fetch_eth_price(&http_ro).await?;
    tracing::info!("💲 ETH price: ${eth_price_usd:.2} (Chainlink)");

    let eth_bal_wei: U256 = http_ro.get_balance(wallet_addr).await?;
    let eth_bal = eth_bal_wei.to::<u128>() as f64 / 1e18;
    tracing::info!("✅ ETH balance: {eth_bal:.6}");

    // Warn immediately if ETH balance is already below threshold
    if eth_bal <= cfg.eth_keep {
        tracing::warn!("⚠️  ETH balance {eth_bal:.6} <= eth_keep {}. Bot cannot send tx!", cfg.eth_keep);
    }

    // ── Telegram ──
    let tg = if !cfg.telegram_token.is_empty() && !cfg.telegram_chat_id.is_empty() {
        tracing::info!("📱 Telegram notifications enabled");
        Some(TelegramNotifier::new(&cfg.telegram_token, &cfg.telegram_chat_id))
    } else {
        tracing::warn!("📱 Telegram not configured (TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID missing)");
        None
    };

    // ── Persistent stats ──
    let mut stats = StatsStore::load();
    tracing::info!(
        "📊 Stats loaded: {} liquidations, ${:.2} profit since {}",
        stats.total_successes(), stats.total_profit(), stats.started_at
    );

    // ── Build user index: load saved addresses + scan only new blocks ──
    let user_index: UserIndex = Arc::new(RwLock::new(HashMap::new()));
    let current_block: u64 = http_ro.get_block_number().await?;

    // Load persisted index from previous run (survives restarts)
    let saved = load_saved_index();
    let saved_count = saved.addresses.len();
    let scan_from = if saved.last_saved_block > 0 {
        // Resume from where we left off — only scan new blocks
        saved.last_saved_block + 1
    } else {
        // First ever run — scan last ~14 days of Borrow events
        current_block.saturating_sub(DEFAULT_LOOKBACK)
    };

    tracing::info!(
        "💾 Loaded {} addresses from saved index (last block: {})",
        saved_count, saved.last_saved_block
    );
    tracing::info!("📡 Scanning Borrow events from block {scan_from} to {current_block}...");

    // drpc free tier: max 9 999 blocks per eth_getLogs request → chunk it
    const CHUNK: u64 = 9_000;
    let mut borrowers: HashSet<Address> = HashSet::new();

    // Seed from saved index first (all previously known borrowers)
    for addr_str in &saved.addresses {
        if let Ok(addr) = addr_str.parse::<Address>() {
            borrowers.insert(addr);
        }
    }

    let mut chunk_start = scan_from;
    while chunk_start <= current_block {
        let chunk_end = (chunk_start + CHUNK - 1).min(current_block);
        let log_filter = Filter::new()
            .address(AAVE_POOL)
            .event_signature(IAavePool::Borrow::SIGNATURE_HASH)
            .from_block(chunk_start)
            .to_block(chunk_end);

        match http_ro.get_logs(&log_filter).await {
            Ok(logs) => {
                for log in &logs {
                    if let Ok(ev) = IAavePool::Borrow::decode_log_data(log.data()) {
                        borrowers.insert(ev.user);
                    }
                }
            }
            Err(e) => tracing::warn!("get_logs {chunk_start}-{chunk_end} failed: {e}"),
        }
        chunk_start = chunk_end + 1;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    tracing::info!("Found {} unique borrowers", borrowers.len());

    // Batch health factor checks
    let hf_thresh = U256::from((cfg.health_factor_threshold * 1e18) as u128);
    let hf_095    = U256::from(95u64) * U256::from(10u64).pow(U256::from(16u64));
    let one_e18   = U256::from(10u64).pow(U256::from(18u64));
    let mut at_risk = 0u32;

    for chunk in borrowers.iter().collect::<Vec<_>>().chunks(20) {
        for &&user in chunk {
            if let Ok(d) = pool.getUserAccountData(user).call().await {
                if d.totalDebtBase > U256::ZERO {
                    if d.healthFactor < hf_thresh { at_risk += 1; }
                    let interval = refresh_interval_blocks(d.healthFactor);
                    user_index.write().await.insert(user, UserPosition {
                        health_factor: d.healthFactor,
                        total_collateral_base: d.totalCollateralBase,
                        total_debt_base: d.totalDebtBase,
                        last_block: current_block,
                        next_refresh_at: current_block + interval,
                    });
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let tracked = user_index.read().await.len();
    tracing::info!("✅ {tracked} users tracked, {at_risk} at risk");

    // Notify startup
    if let Some(ref tg) = tg {
        tg.notify_startup(
            &format!("{wallet_addr}"),
            &cfg.cold_wallet,
            &cfg.contract_address,
            eth_bal,
            tracked, at_risk,
        ).await;
    }

    // ── Shared state for command handler ──
    let shared_tracked   = Arc::new(RwLock::new(tracked));
    let shared_at_risk   = Arc::new(RwLock::new(at_risk));
    let shared_eth_bal   = Arc::new(RwLock::new(eth_bal));
    let shared_eth_price = Arc::new(RwLock::new(eth_price_usd));
    // Snapshot of at-risk positions for /hf command: (address, hf, debt_usd), sorted HF asc
    let shared_hf_list: Arc<RwLock<Vec<(String, f64, f64)>>> = Arc::new(RwLock::new(Vec::new()));

    // ── Bot active flag (toggled by /start_bot and /stop_bot) ──
    let bot_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));

    // ── Channel for Telegram → main loop commands ──
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<TelegramCommand>(8);

    // ── Spawn Telegram command listener (separate task, never blocks bot) ──
    if let Some(ref tg) = tg {
        let tg_cmd    = tg.clone();
        let tracked_ref   = shared_tracked.clone();
        let risk_ref      = shared_at_risk.clone();
        let active_ref    = bot_active.clone();
        let hf_ref        = shared_hf_list.clone();
        let eth_bal_ref   = shared_eth_bal.clone();
        let eth_price_ref = shared_eth_price.clone();
        let hot = format!("{wallet_addr}");
        tokio::spawn(async move {
            tg_cmd.run_command_listener(
                std::time::Instant::now(),
                "stats.json".to_string(),
                hot,
                tracked_ref,
                risk_ref,
                active_ref,
                cmd_tx,
                hf_ref,
                cfg.health_factor_threshold,
                eth_bal_ref,
                eth_price_ref,
            ).await;
        });
        tracing::info!("📱 Telegram commands: /status /stats /json /hf /gas /help /stop_bot /start_bot /pause_contract /resume_contract");
    }

    // ── Fee tier cache + pre-warm ──────────────────────────────────────────────
    // Cache: (token_in, token_out) → (fee_tier_bps, block_verified)
    // Pre-warm all stable↔major and major↔major cross-pairs so the FIRST
    // liquidation attempt has zero QuoterV2 overhead (0 ms vs 50 ms).
    let mut fee_cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
    {
        let stable_addrs: Vec<Address> = TOKENS.iter()
            .filter(|(_, _, d)| *d == 6)
            .map(|(a, _, _)| *a)
            .collect();
        let major_addrs: Vec<Address> = TOKENS.iter()
            .filter(|(_, _, d)| *d != 6)
            .map(|(a, _, _)| *a)
            .collect();

        let mut pairs: Vec<(Address, Address)> = Vec::new();

        // major → stable  (collateral = major, debt = stable — most common case)
        // stable → major  (collateral = stable, debt = major — rarer but exists)
        for s in &stable_addrs {
            for m in &major_addrs {
                pairs.push((*m, *s));
                pairs.push((*s, *m));
            }
        }
        // stable → stable (e.g. USDC debt liquidated with USDT collateral)
        for (i, a) in stable_addrs.iter().enumerate() {
            for b in stable_addrs.iter().skip(i + 1) {
                pairs.push((*a, *b));
                pairs.push((*b, *a));
            }
        }
        // major → major  (e.g. wstETH collateral, WETH debt)
        for (i, a) in major_addrs.iter().enumerate() {
            for b in major_addrs.iter().skip(i + 1) {
                pairs.push((*a, *b));
                pairs.push((*b, *a));
            }
        }

        tracing::info!("🗄️  Pre-warming fee tier cache for {} pairs…", pairs.len());

        // Process in batches of 6 pairs (= 24 concurrent QuoterV2 calls)
        // to stay within DRPC/Alchemy free-tier concurrent request limits.
        let mut resolved = 0usize;
        for batch in pairs.chunks(6) {
            let futs: Vec<_> = batch.iter()
                .map(|(tin, tout)| best_fee_tier(&http_ro, *tin, *tout, token_unit(*tin)))
                .collect();
            let results = futures_util::future::join_all(futs).await;
            for ((tin, tout), maybe_tier) in batch.iter().zip(results.iter()) {
                if let Some(tier) = maybe_tier {
                    fee_cache.insert((*tin,  *tout), (*tier, current_block));
                    fee_cache.insert((*tout, *tin),  (*tier, current_block));
                    resolved += 1;
                }
            }
        }
        tracing::info!("🗄️  Fee cache ready: {resolved}/{} pairs resolved", pairs.len());
    }

    // ── pending_liquidations: prevents double-sending on the same user ──
    let mut pending_liquidations: HashSet<Address> = HashSet::new();

    // ── Priority refresh: users are refreshed based on their HF proximity to 1.0 ──
    // refresh_interval_blocks() returns fewer blocks for riskier positions.
    // Cap: 30 RPC calls/block maximum to avoid rate-limiting.
    const MAX_REFRESH_PER_BLOCK: usize = 30;

    // ── Subscribe to new Borrow events to detect borrowers in real-time ──
    let borrow_filter = Filter::new()
        .address(AAVE_POOL)
        .event_signature(IAavePool::Borrow::SIGNATURE_HASH);
    let borrow_sub = ws.subscribe_logs(&borrow_filter).await?;
    let mut borrow_stream = borrow_sub.into_stream();

    // ── Main loop: new blocks via WebSocket ──
    tracing::info!("🔄 Listening for new blocks + new borrowers...");

    let sub = ws.subscribe_blocks().await?;
    let mut stream = sub.into_stream();

    let mut stats_liq = 0u32;
    let mut stats_profit = 0.0f64;
    // Track price for flash-crash detection
    let mut prev_eth_price = eth_price_usd;
    // Last known block number — used to save index on clean shutdown
    let mut last_block: u64 = current_block;
    // Last block we sent a low-gas alert (avoid spam)
    let mut last_gas_alert_block: u64 = 0;

    use futures_util::StreamExt;

    loop {
        tokio::select! {
            // ── New Borrow event: add borrower to index ──
            Some(log) = borrow_stream.next() => {
                if let Ok(ev) = IAavePool::Borrow::decode_log_data(log.data()) {
                    index_borrower(&mut *user_index.write().await, ev.user);
                    tracing::debug!("📥 New borrower indexed: {}", ev.user);
                }
            }

            // ── Telegram command from listener task ──
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    TelegramCommand::PauseBot => {
                        bot_active.store(false, Ordering::Relaxed);
                        tracing::warn!("⏸️  Bot paused via Telegram /stop_bot");
                        if let Some(ref tg) = tg {
                            tg.send_raw("⚡ LiqBot ⏸️ <b>Bot mis en pause</b>\nLiquidations suspendues. /start_bot pour reprendre.").await;
                        }
                    }
                    TelegramCommand::ResumeBot => {
                        bot_active.store(true, Ordering::Relaxed);
                        tracing::info!("▶️  Bot resumed via Telegram /start_bot");
                        if let Some(ref tg) = tg {
                            tg.send_raw("⚡ LiqBot ▶️ <b>Bot relancé</b>\nLiquidations reprises.").await;
                        }
                    }
                    TelegramCommand::PauseContract => {
                        tracing::warn!("⏸️  Pausing contract via /pause_contract...");
                        match liquidator.setPaused(true).send().await {
                            Ok(pending) => {
                                let hash = format!("{:?}", pending.tx_hash());
                                tracing::info!("  Contract pause TX: {hash}");
                                if let Some(ref tg) = tg {
                                    tg.send_raw(&format!(
                                        "⚡ LiqBot ⏸️ <b>Contrat mis en pause</b>\n🔗 <a href=\"https://arbiscan.io/tx/{hash}\">Arbiscan</a>"
                                    )).await;
                                }
                            }
                            Err(e) => {
                                tracing::error!("  Pause contract failed: {e}");
                                if let Some(ref tg) = tg {
                                    tg.notify_error("pause_contract", &format!("{e}")).await;
                                }
                            }
                        }
                    }
                    TelegramCommand::ResumeContract => {
                        tracing::info!("▶️  Unpausing contract via /resume_contract...");
                        match liquidator.setPaused(false).send().await {
                            Ok(pending) => {
                                let hash = format!("{:?}", pending.tx_hash());
                                tracing::info!("  Contract unpause TX: {hash}");
                                if let Some(ref tg) = tg {
                                    tg.send_raw(&format!(
                                        "⚡ LiqBot ▶️ <b>Contrat réactivé</b>\n🔗 <a href=\"https://arbiscan.io/tx/{hash}\">Arbiscan</a>"
                                    )).await;
                                }
                            }
                            Err(e) => {
                                tracing::error!("  Unpause contract failed: {e}");
                                if let Some(ref tg) = tg {
                                    tg.notify_error("resume_contract", &format!("{e}")).await;
                                }
                            }
                        }
                    }
                }
            }

            block_opt = stream.next() => {
                // None means the WebSocket closed — bail so systemd restarts us
                let block = match block_opt {
                    Some(b) => b,
                    None => eyre::bail!("WebSocket block subscription closed — restarting"),
                };

                let bn = block.number;
                last_block = bn;

                // Collect at-risk users, sorted by debt descending (biggest profit first).
                // On Arbitrum FCFS, we want to attempt the most valuable position first.
                let at_risk_users: Vec<Address> = {
                    let idx = user_index.read().await;
                    let mut v: Vec<(Address, U256)> = idx.iter()
                        .filter(|(_, p)| p.health_factor < hf_thresh && p.total_debt_base > U256::ZERO)
                        .map(|(a, p)| (*a, p.total_debt_base))
                        .collect();
                    v.sort_unstable_by(|a, b| b.1.cmp(&a.1)); // descending debt
                    v.into_iter().map(|(a, _)| a).collect()
                };

                // ── Priority refresh: process users due this block, riskiest first ──
                // All getUserAccountData calls run IN PARALLEL (join_all) then one
                // write lock to update the index — was: N sequential calls × 1 lock each.
                // For 30 users: was ~600ms sequential, now ~20ms parallel.
                {
                    let mut due: Vec<(Address, U256)> = {
                        let idx = user_index.read().await;
                        idx.iter()
                            .filter(|(u, p)| p.next_refresh_at <= bn && !at_risk_users.contains(u))
                            .map(|(u, p)| (*u, p.health_factor))
                            .collect()
                    };
                    due.sort_unstable_by_key(|(_, hf)| *hf);
                    due.truncate(MAX_REFRESH_PER_BLOCK);

                    if !due.is_empty() {
                        // Build all call builders first so they live long enough
                        let cbs: Vec<_> = due.iter()
                            .map(|(user, _)| pool.getUserAccountData(*user))
                            .collect();

                        // Fire all RPC calls in parallel
                        // EthCall implements IntoFuture (not Future), so we must
                        // call .into_future() explicitly for join_all.
                        let results = futures_util::future::join_all(
                            cbs.iter().map(|cb| std::future::IntoFuture::into_future(cb.call()))
                        ).await;

                        // Single write-lock to apply all updates atomically
                        let mut idx = user_index.write().await;
                        for ((user, _), d_result) in due.iter().zip(results.iter()) {
                            let Ok(d) = d_result else { continue; };
                            let interval = refresh_interval_blocks(d.healthFactor);
                            remove_if_repaid(&mut idx, user, d.totalDebtBase);
                            if let Some(pos) = idx.get_mut(user) {
                                pos.health_factor          = d.healthFactor;
                                pos.total_debt_base        = d.totalDebtBase;
                                pos.total_collateral_base  = d.totalCollateralBase;
                                pos.last_block             = bn;
                                pos.next_refresh_at        = bn + interval;
                            }
                        }
                    }
                }

                // Refresh ETH price every 100 blocks (~2 min on Arbitrum)
                if bn % 100 == 0 {
                    match fetch_eth_price(&http_ro).await {
                        Ok(p) => {
                            let drop_pct = (prev_eth_price - p) / prev_eth_price;
                            eth_price_usd = p;
                            tracing::info!("💲 ETH price refreshed: ${eth_price_usd:.2}");

                            // Flash-crash detection: if price dropped > 1%, immediately
                            // re-schedule all users whose stored HF could now be < hf_thresh.
                            // Formula: stored_hf * (new_price/old_price) < hf_thresh
                            // → stored_hf < hf_thresh / (new_price/old_price)
                            // We use a 10% buffer to avoid re-scanning on noise.
                            if drop_pct > 0.01 {
                                let scale = p / prev_eth_price; // < 1.0
                                // HF threshold adjusted for the price move:
                                // any user whose HF × scale < hf_thresh needs immediate check
                                let invalidate_below = U256::from(
                                    (cfg.health_factor_threshold / scale * 1e18) as u128
                                );
                                let mut count = 0usize;
                                for pos in user_index.write().await.values_mut() {
                                    if pos.health_factor < invalidate_below {
                                        pos.next_refresh_at = bn; // refresh this block
                                        count += 1;
                                    }
                                }
                                if count > 0 {
                                    tracing::warn!(
                                        "⚡ Flash drop {:.1}%! Invalidated {} positions for immediate refresh",
                                        drop_pct * 100.0, count
                                    );
                                }
                            }
                            prev_eth_price = p;
                            *shared_eth_price.write().await = p;

                            // Refresh ETH balance + auto low-gas alert
                            let bal_wei = http_ro.get_balance(wallet_addr).await.unwrap_or(U256::ZERO);
                            let bal = bal_wei.to::<u128>() as f64 / 1e18;
                            *shared_eth_bal.write().await = bal;

                            // Compute days remaining from stats and alert if < 7
                            let s = StatsStore::load();
                            let total_ops = s.total_successes() + s.total_failures();
                            if total_ops > 0 {
                                let days_active = {
                                    use chrono::{NaiveDate, Utc};
                                    if let Ok(start) = NaiveDate::parse_from_str(&s.started_at, "%Y-%m-%d") {
                                        (Utc::now().date_naive() - start).num_days().max(1) as f64
                                    } else { 1.0 }
                                };
                                let avg_gas_per_day = s.total_gas() / days_active;
                                let days_remaining = if avg_gas_per_day > 0.0 {
                                    (bal * p) / avg_gas_per_day
                                } else { f64::INFINITY };

                                const ALERT_DAYS: f64 = 7.0;
                                const ALERT_COOLDOWN_BLOCKS: u64 = 2000; // ~8 min
                                if days_remaining < ALERT_DAYS
                                    && bn.saturating_sub(last_gas_alert_block) > ALERT_COOLDOWN_BLOCKS
                                {
                                    last_gas_alert_block = bn;
                                    tracing::warn!("⛽ Gas faible: ~{:.1} jours restants", days_remaining);
                                    if let Some(ref tg) = tg {
                                        let msg = format!(
                                            "⚡ LiqBot ⛽ <b>Gas faible !</b>\n\
                                            Solde: <b>{bal:.5} ETH</b> (~${:.2})\n\
                                            Estimation: <b>{:.1} jours</b> restants\n\
                                            ⚠️ Recharge le hot wallet !",
                                            bal * p, days_remaining
                                        );
                                        tg.send_raw(&msg).await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("ETH price refresh failed: {e}");
                        }
                    }
                }

                // Update shared state for /status and /hf commands
                *shared_at_risk.write().await = at_risk_users.len() as u32;
                *shared_tracked.write().await = user_index.read().await.len();
                {
                    let idx = user_index.read().await;
                    let mut snapshot: Vec<(String, f64, f64)> = at_risk_users.iter()
                        .filter_map(|addr| idx.get(addr).map(|p| {
                            let hf  = p.health_factor.saturating_to::<u128>() as f64 / 1e18;
                            let debt = p.total_debt_base.saturating_to::<u128>() as f64 / 1e8;
                            (format!("{addr}"), hf, debt)
                        }))
                        .collect();
                    snapshot.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap()); // HF asc
                    *shared_hf_list.write().await = snapshot;
                }

                // Persist user index every 500 blocks (~2 min) so restarts are fast
                if bn % 500 == 0 {
                    save_index(&*user_index.read().await, bn);
                }

                // Heartbeat toutes les 100 blocs (~25s sur Arbitrum)
                if bn % 100 == 0 {
                    let liquidatable = at_risk_users.iter().filter(|&&u| {
                        user_index.try_read().ok()
                            .and_then(|idx| idx.get(&u).map(|p| p.health_factor < one_e18))
                            .unwrap_or(false)
                    }).count();
                    tracing::info!(
                        "💓 Block {bn} | {} tracked | {} watching (HF<1.05) | {} liquidatable (HF<1.0) | {stats_liq} liq (${stats_profit:.2} gross) | ETH ${eth_price_usd:.0}",
                        user_index.read().await.len(),
                        at_risk_users.len(),
                        liquidatable,
                    );
                }

                if at_risk_users.is_empty() || !bot_active.load(Ordering::Relaxed) {
                    continue;
                }

                for user in &at_risk_users {
                    // Skip if we already have a pending tx for this user
                    if pending_liquidations.contains(user) {
                        tracing::debug!("  Skip {user}: liquidation already pending");
                        continue;
                    }

                    let Ok(d) = pool.getUserAccountData(*user).call().await else {
                        continue;
                    };

                    let hf = d.healthFactor;
                    let debt_usd = d.totalDebtBase.to::<u128>() as f64 / 1e8;

                    // Update index — or remove if position fully repaid
                    {
                        let interval = refresh_interval_blocks(hf);
                        let mut idx = user_index.write().await;
                        remove_if_repaid(&mut idx, user, d.totalDebtBase);
                        if let Some(pos) = idx.get_mut(user) {
                            pos.health_factor = hf;
                            pos.total_debt_base = d.totalDebtBase;
                            pos.total_collateral_base = d.totalCollateralBase;
                            pos.last_block = bn;
                            pos.next_refresh_at = bn + interval;
                        }
                    }

                    if hf >= one_e18 {
                        continue;
                    } // Not liquidatable

                    tracing::warn!(
                        "🎯 LIQUIDATABLE: {user} HF={:.6} debt=${debt_usd:.2}",
                        hf.to::<u128>() as f64 / 1e18
                    );

                    // Skip tiny positions (not worth the RPC overhead)
                    if debt_usd < cfg.min_profit_usd * 10.0 {
                        continue;
                    }

                    // Find best debt + collateral across all tracked tokens
                    let mut best_debt: (Address, U256, &str, u8) = (Address::ZERO, U256::ZERO, "", 18);
                    let mut best_coll: (Address, U256, &str, u8) = (Address::ZERO, U256::ZERO, "", 18);

                    {
                        let cbs_rd: Vec<_> = TOKENS.iter()
                            .map(|(tok, _, _)| data_prov.getUserReserveData(*tok, *user))
                            .collect();
                        let results = futures_util::future::join_all(
                            cbs_rd.iter().map(|cb| std::future::IntoFuture::into_future(cb.call()))
                        ).await;
                        for ((tok, name, decimals), rd_result) in TOKENS.iter().zip(results.iter()) {
                            let Ok(rd) = rd_result else { continue; };
                            if rd.currentVariableDebt > best_debt.1 {
                                best_debt = (*tok, rd.currentVariableDebt, name, *decimals);
                            }
                            if rd.currentATokenBalance > best_coll.1 && rd.usageAsCollateralEnabled {
                                best_coll = (*tok, rd.currentATokenBalance, name, *decimals);
                            }
                        }
                    }

                    if best_debt.0 == Address::ZERO || best_coll.0 == Address::ZERO {
                        continue;
                    }

                    tracing::info!(
                        "  Debt: {} {} | Coll: {} {}",
                        best_debt.1, best_debt.2, best_coll.1, best_coll.2
                    );

                    // Close factor: 100% when HF < 0.95, 50% otherwise
                    let close_fraction: f64 = if hf < hf_095 { 1.0 } else { 0.5 };
                    let close_pct: u8 = if hf < hf_095 { 100 } else { 50 };

                    let debt_to_cover = if hf < hf_095 {
                        best_debt.1
                    } else {
                        best_debt.1 / U256::from(2u64)
                    };

                    // min_profit in debt token native units (correct per-token scaling)
                    let min_profit = min_profit_raw(cfg.min_profit_usd, best_debt.3, eth_price_usd);

                    // ── Fee tier + minSwapOut ──────────────────────────────────────────
                    // For same-token liquidations (collateral == debt) no swap is needed.
                    // For cross-token: query QuoterV2 across all 4 Uniswap fee tiers to
                    // find the pool with actual liquidity.  Previously hardcoded at 3000
                    // which caused missed liquidations (e.g. WETH/USDC best pool = 500).
                    //
                    // minSwapOut = flash_repay + min_profit (in debt token units):
                    //   guarantees the swap always yields enough to repay the flash loan
                    //   AND produce at least min_profit — the contract checks the same.
                    let (fee_tier, min_swap_out) = if best_debt.0 == best_coll.0 {
                        // Same-token: no swap, no slippage to worry about
                        (alloy::primitives::Uint::<24, 1>::from(0u32), U256::ZERO)
                    } else {
                        // Find the most liquid Uniswap pool for collateral → debt.
                        // Uses the in-memory cache (0 RPC calls if warm, ~50ms if cold).
                        match cached_fee_tier(&mut fee_cache, &http_ro, best_coll.0, best_debt.0, bn).await {
                            Some(f) => {
                                // floor: must recover flash principal + premium + min_profit
                                let flash_repay = debt_to_cover
                                    + debt_to_cover * U256::from(premium as u64)
                                        / U256::from(10_000u64);
                                let min_out = flash_repay + min_profit;
                                tracing::info!(
                                    "  Route: {f} bps | minSwapOut: {min_out} ({}/{})",
                                    best_coll.2, best_debt.2
                                );
                                (alloy::primitives::Uint::<24, 1>::from(f), min_out)
                            }
                            None => {
                                tracing::info!(
                                    "  No Uniswap route for {}/{} → skip {user}",
                                    best_coll.2, best_debt.2
                                );
                                continue;
                            }
                        }
                    };

                    let tx = liquidator.liquidate(
                        *user,
                        best_coll.0,
                        best_debt.0,
                        debt_to_cover,
                        fee_tier,
                        min_swap_out,
                        min_profit,
                    );

                    // Simulate via estimateGas
                    let gas = match tx.estimate_gas().await {
                        Ok(g) => g,
                        Err(e) => {
                            tracing::info!("  Simulation revert → skip {user} (déjà liquidé ou pas profitable): {e}");
                            continue;
                        }
                    };

                    // Check current gas price against our max
                    let gp: u128 = http_ro.get_gas_price().await.unwrap_or(100_000_000u128);
                    let max_gas_wei = (cfg.max_gas_gwei * 1e9) as u128;

                    if gp > max_gas_wei {
                        tracing::info!(
                            "  Skip: gas price {:.3} gwei > max {} gwei",
                            gp as f64 / 1e9,
                            cfg.max_gas_gwei
                        );
                        continue;
                    }

                    let gas_usd = gas as f64 * gp as f64 / 1e18 * eth_price_usd;

                    // Expected GROSS profit: debt liquidated * 5% bonus - 0.3% swap fee
                    // (approximate — actual profit confirmed by LiquidationExecuted event)
                    let expected_profit = debt_usd * close_fraction * 0.047;
                    let hf_display = hf.to::<u128>() as f64 / 1e18;

                    tracing::info!(
                        "  Gas: {gas} units @ {:.3} gwei (~${gas_usd:.4}) | Expected gross: ~${expected_profit:.2}",
                        gp as f64 / 1e9
                    );

                    if gas_usd > debt_usd * 0.05 {
                        tracing::info!("  Skip: gas ${gas_usd:.4} > 5% of debt ${debt_usd:.2}");
                        if let Some(tg) = tg.clone() {
                            let u = format!("{user}");
                            let reason = format!("Gas ${gas_usd:.4} > 5% dette ${debt_usd:.2}");
                            tokio::spawn(async move {
                                tg.notify_simulation_skip(&u, &reason).await;
                            });
                        }
                        continue;
                    }

                    // Check we have enough ETH to send the tx (keep eth_keep in reserve)
                    let current_eth_wei: U256 = http_ro.get_balance(wallet_addr).await.unwrap_or(U256::ZERO);
                    let current_eth = current_eth_wei.to::<u128>() as f64 / 1e18;
                    let eth_keep_wei = U256::from((cfg.eth_keep * 1e18) as u128);

                    if current_eth_wei <= eth_keep_wei {
                        tracing::warn!(
                            "  Skip: ETH {current_eth:.6} <= eth_keep {}. Recharge hot wallet!",
                            cfg.eth_keep
                        );
                        if let Some(tg) = tg.clone() {
                            let b = current_eth;
                            tokio::spawn(async move {
                                tg.notify_low_eth(b).await;
                            });
                        }
                        continue;
                    }

                    // ── CRITICAL PATH: send tx IMMEDIATELY, no Telegram here ──
                    tracing::info!("🚀 EXECUTING {user}!");

                    // Mark as pending before sending to prevent double-execution
                    pending_liquidations.insert(*user);

                    let tx = tx.gas(gas * 13 / 10);
                    let send_result = tx.send().await;

                    // ── TX sent (or failed). Notifications are fire-and-forget ──
                    match send_result {
                        Ok(pending) => {
                            let tx_hash = format!("{:?}", pending.tx_hash());
                            tracing::info!("  TX: {tx_hash}");

                            match pending.get_receipt().await {
                                Ok(receipt) if receipt.status() => {
                                    stats_liq += 1;
                                    stats_profit += expected_profit;

                                    // Record GROSS profit — stats.rs subtracts gas separately
                                    stats.record_liquidation(
                                        &format!("{user}"),
                                        best_debt.2,
                                        best_coll.2,
                                        debt_usd,
                                        expected_profit,
                                        gas_usd,
                                        &tx_hash,
                                        true,
                                    );

                                    let summary = stats.format_summary();

                                    let new_eth = http_ro.get_balance(wallet_addr).await
                                        .unwrap_or(U256::ZERO)
                                        .to::<u128>() as f64 / 1e18;

                                    tracing::info!(
                                        "  ✅ ~${:.2} gross | Total: {stats_liq} (${stats_profit:.2} gross)",
                                        expected_profit
                                    );

                                    if let Some(tg) = tg.clone() {
                                        let u = format!("{user}");
                                        let h = tx_hash.clone();
                                        let dt = best_debt.2.to_string();
                                        let da = format!("{}", best_debt.1);
                                        let ct = best_coll.2.to_string();
                                        let ca = format!("{}", best_coll.1);
                                        let net = expected_profit - gas_usd;

                                        tokio::spawn(async move {
                                            tg.notify_liquidation_complete(
                                                &u,
                                                hf_display,
                                                debt_usd,
                                                &dt,
                                                &da,
                                                &ct,
                                                &ca,
                                                close_pct,
                                                gas,
                                                gas_usd,
                                                expected_profit,
                                                true,
                                                &h,
                                                "",
                                                expected_profit,
                                                gas_usd,
                                                net,
                                                new_eth,
                                                &summary,
                                            ).await;
                                        });
                                    }
                                }
                                Ok(_) => {
                                    tracing::warn!("  ❌ Reverted on-chain");

                                    stats.record_liquidation(
                                        &format!("{user}"),
                                        best_debt.2,
                                        best_coll.2,
                                        debt_usd,
                                        0.0,
                                        gas_usd,
                                        &tx_hash,
                                        false,
                                    );

                                    let summary = stats.format_summary();

                                    let new_eth = http_ro.get_balance(wallet_addr).await
                                        .unwrap_or(U256::ZERO)
                                        .to::<u128>() as f64 / 1e18;

                                    if let Some(tg) = tg.clone() {
                                        let u = format!("{user}");
                                        let h = tx_hash.clone();
                                        let dt = best_debt.2.to_string();
                                        let da = format!("{}", best_debt.1);
                                        let ct = best_coll.2.to_string();
                                        let ca = format!("{}", best_coll.1);

                                        tokio::spawn(async move {
                                            tg.notify_liquidation_complete(
                                                &u,
                                                hf_display,
                                                debt_usd,
                                                &dt,
                                                &da,
                                                &ct,
                                                &ca,
                                                close_pct,
                                                gas,
                                                gas_usd,
                                                expected_profit,
                                                false,
                                                &h,
                                                "Reverted on-chain",
                                                0.0,
                                                gas_usd,
                                                -gas_usd,
                                                new_eth,
                                                &summary,
                                            ).await;
                                        });
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("  Receipt error: {e}");
                                    if let Some(tg) = tg.clone() {
                                        let msg = format!("{e}");
                                        tokio::spawn(async move {
                                            tg.notify_error("Receipt", &msg).await;
                                        });
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("  Send failed: {e}");
                            if let Some(tg) = tg.clone() {
                                let msg = format!("{e}");
                                tokio::spawn(async move {
                                    tg.notify_error("TX send", &msg).await;
                                });
                            }
                        }
                    }

                    // Release the pending lock regardless of tx outcome
                    pending_liquidations.remove(user);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                // Save user index before exiting so next restart is fast
                save_index(&*user_index.read().await, last_block);
                tracing::info!("\n🛑 Session: {stats_liq} liquidations, ~${stats_profit:.2} gross");
                if let Some(ref tg) = tg {
                    let summary = stats.format_summary();
                    let eth: f64 = http_ro.get_balance(wallet_addr).await
                        .unwrap_or(U256::ZERO)
                        .to::<u128>() as f64 / 1e18;

                    let msg = format!(
                        "⚡ LiqBot 🛑 <b>Bot arrêté</b>\n\
                        🔑 Hot: {eth:.6} ETH\n\n\
                        ━━━━━━━━━━━━━━━━━━━━━━━━\n\
                        {summary}",
                    );
                    tg.send_raw(&msg).await;
                }
                break;
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Unit tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_profit_raw_stablecoin_6dec() {
        // $2 min profit for USDC (6 dec) = 2_000_000 units
        let v = min_profit_raw(2.0, 6, 3500.0);
        assert_eq!(v, U256::from(2_000_000u128));
    }

    #[test]
    fn test_min_profit_raw_weth_18dec() {
        // $2 / $3500 ETH = 0.000571... ETH ≈ 571_428_571_428_571 wei
        let v = min_profit_raw(2.0, 18, 3500.0);
        let expected: u128 = (2.0_f64 / 3500.0 * 1e18) as u128;
        assert_eq!(v, U256::from(expected));
        // Sanity: should be in the range ~5e14 wei
        assert!(v > U256::from(5e13 as u128));
        assert!(v < U256::from(6e14 as u128));
    }

    #[test]
    fn test_min_profit_raw_wbtc_8dec_returns_zero() {
        // WBTC (8 dec) — no price known, must return U256::ZERO
        let v = min_profit_raw(2.0, 8, 3500.0);
        assert_eq!(v, U256::ZERO);
    }

    #[test]
    fn test_min_profit_raw_zero_profit() {
        let v = min_profit_raw(0.0, 6, 3500.0);
        assert_eq!(v, U256::ZERO);
    }

    #[test]
    fn test_min_profit_raw_higher_eth_price_means_less_wei() {
        // Higher ETH price → less ETH needed for same USD profit
        let v_cheap_eth = min_profit_raw(10.0, 18, 2000.0); // ETH at $2000 → need more wei
        let v_pricey_eth = min_profit_raw(10.0, 18, 5000.0); // ETH at $5000 → need less wei
        assert!(v_cheap_eth > v_pricey_eth, "cheaper ETH → more wei needed for same USD profit");
    }

    // ── TOKENS table ──────────────────────────────────────────────

    #[test]
    fn test_tokens_table_has_correct_len() {
        // 8 original + 3 added (LINK, rETH, LUSD)
        assert_eq!(TOKENS.len(), 11, "Expected 11 tokens; update this if you add more");
    }

    #[test]
    fn test_tokens_all_have_valid_decimals() {
        for (addr, name, dec) in TOKENS {
            assert!(
                [6u8, 8, 18].contains(dec),
                "Token {name} ({addr}) has unexpected decimals {dec}"
            );
        }
    }

    #[test]
    fn test_tokens_no_duplicate_addresses() {
        let mut seen = HashSet::new();
        for (addr, name, _) in TOKENS {
            assert!(seen.insert(addr), "Duplicate address for token {name}");
        }
    }

    #[test]
    fn test_tokens_no_duplicate_symbols() {
        let mut seen = std::collections::HashSet::new();
        for (_, name, _) in TOKENS {
            assert!(seen.insert(*name), "Duplicate symbol: {name}");
        }
    }

    #[test]
    fn test_tokens_no_zero_address() {
        for (addr, name, _) in TOKENS {
            assert_ne!(*addr, Address::ZERO, "Token {name} has zero address");
        }
    }

    #[test]
    fn test_tokens_contains_original_reserves() {
        let names: Vec<&str> = TOKENS.iter().map(|(_, n, _)| *n).collect();
        for expected in ["WETH", "USDC", "USDCe", "USDT", "DAI", "WBTC", "wstETH", "ARB"] {
            assert!(names.contains(&expected), "Original reserve {expected} missing");
        }
    }

    #[test]
    fn test_tokens_contains_added_reserves() {
        // These were missing and caused the $211k HF=1.03 position to be undetectable
        let addrs: Vec<Address> = TOKENS.iter().map(|(a, _, _)| *a).collect();
        assert!(
            addrs.contains(&address!("f97f4df75117a78c1A5a0DBb814Af92458539FB4")),
            "LINK missing from TOKENS"
        );
        assert!(
            addrs.contains(&address!("EC70Dcb4A1EFa46b8F2D97C310C9c4790ba5ffA4")),
            "rETH missing from TOKENS"
        );
        assert!(
            addrs.contains(&address!("93b346b6BC2548dA6A1E7d98E9a421B42541425b")),
            "LUSD missing from TOKENS"
        );
    }

    // ── index_borrower ──

    fn make_index() -> HashMap<Address, UserPosition> {
        HashMap::new()
    }

    fn make_position(hf: u128, debt: u64) -> UserPosition {
        UserPosition {
            health_factor: U256::from(hf),
            total_collateral_base: U256::ZERO,
            total_debt_base: U256::from(debt),
            last_block: 0,
            next_refresh_at: 0,
        }
    }

    // ── refresh_interval_blocks ──

    #[test]
    fn test_refresh_interval_new_borrower_is_zero() {
        assert_eq!(refresh_interval_blocks(U256::MAX), 0);
    }

    #[test]
    fn test_refresh_interval_liquidatable_is_one() {
        // HF < 1.0 → interval = 1
        let hf_099 = U256::from(990_000_000_000_000_000u128);
        assert_eq!(refresh_interval_blocks(hf_099), 1);
    }

    #[test]
    fn test_refresh_interval_hf_105() {
        // HF = 1.05 → pct=5 → 5*5/4 = 6
        let hf = U256::from(1_050_000_000_000_000_000u128);
        assert_eq!(refresh_interval_blocks(hf), 6);
    }

    #[test]
    fn test_refresh_interval_hf_110() {
        // HF = 1.10 → pct=10 → 10*10/4 = 25
        let hf = U256::from(1_100_000_000_000_000_000u128);
        assert_eq!(refresh_interval_blocks(hf), 25);
    }

    #[test]
    fn test_refresh_interval_hf_120() {
        // HF = 1.20 → pct=20 → 20*20/4 = 100
        let hf = U256::from(1_200_000_000_000_000_000u128);
        assert_eq!(refresh_interval_blocks(hf), 100);
    }

    #[test]
    fn test_refresh_interval_monotone_increasing() {
        // Higher HF → longer interval (refreshed less often)
        let hf_110 = U256::from(1_100_000_000_000_000_000u128);
        let hf_120 = U256::from(1_200_000_000_000_000_000u128);
        let hf_150 = U256::from(1_500_000_000_000_000_000u128);
        let hf_200 = U256::from(2_000_000_000_000_000_000u128);
        assert!(refresh_interval_blocks(hf_110) < refresh_interval_blocks(hf_120));
        assert!(refresh_interval_blocks(hf_120) < refresh_interval_blocks(hf_150));
        assert!(refresh_interval_blocks(hf_150) < refresh_interval_blocks(hf_200));
    }

    #[test]
    fn test_refresh_interval_capped_at_5000() {
        // Very high HF should be capped
        let hf_huge = U256::from(100_000_000_000_000_000_000u128); // HF = 100
        assert_eq!(refresh_interval_blocks(hf_huge), 5000);
    }

    // ── index_borrower ──

    #[test]
    fn test_index_borrower_adds_new_user() {
        let mut idx = make_index();
        let user = Address::repeat_byte(0x01);
        index_borrower(&mut idx, user);
        assert!(idx.contains_key(&user));
        assert_eq!(idx[&user].health_factor, U256::MAX);
        assert_eq!(idx[&user].total_debt_base, U256::from(1u64));
        assert_eq!(idx[&user].next_refresh_at, 0); // refresh ASAP
    }

    #[test]
    fn test_index_borrower_does_not_overwrite_existing() {
        let mut idx = make_index();
        let user = Address::repeat_byte(0x02);
        idx.insert(user, make_position(950_000_000_000_000_000, 500));
        // Calling index_borrower again must not overwrite
        index_borrower(&mut idx, user);
        assert_eq!(idx[&user].health_factor, U256::from(950_000_000_000_000_000u128));
    }

    #[test]
    fn test_index_borrower_multiple_users_independent() {
        let mut idx = make_index();
        let u1 = Address::repeat_byte(0x01);
        let u2 = Address::repeat_byte(0x02);
        index_borrower(&mut idx, u1);
        index_borrower(&mut idx, u2);
        assert_eq!(idx.len(), 2);
    }

    // ── remove_if_repaid ──

    #[test]
    fn test_remove_if_repaid_removes_when_debt_zero() {
        let mut idx = make_index();
        let user = Address::repeat_byte(0x03);
        index_borrower(&mut idx, user);
        assert!(idx.contains_key(&user));
        remove_if_repaid(&mut idx, &user, U256::ZERO);
        assert!(!idx.contains_key(&user), "user with zero debt must be removed");
    }

    #[test]
    fn test_remove_if_repaid_keeps_when_debt_nonzero() {
        let mut idx = make_index();
        let user = Address::repeat_byte(0x04);
        index_borrower(&mut idx, user);
        remove_if_repaid(&mut idx, &user, U256::from(1_000_000u64));
        assert!(idx.contains_key(&user), "user with active debt must stay");
    }

    #[test]
    fn test_remove_if_repaid_noop_on_unknown_user() {
        let mut idx = make_index();
        let unknown = Address::repeat_byte(0xFF);
        // Must not panic on a user that was never indexed
        remove_if_repaid(&mut idx, &unknown, U256::ZERO);
        assert!(idx.is_empty());
    }

    // ── SavedIndex — serialization ─────────────────────────────

    #[test]
    fn test_saved_index_default_is_empty() {
        let s = SavedIndex::default();
        assert_eq!(s.last_saved_block, 0);
        assert!(s.addresses.is_empty());
    }

    #[test]
    fn test_saved_index_json_round_trip() {
        let original = SavedIndex {
            last_saved_block: 123_456_789,
            addresses: vec![
                "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
                "0x1111111111111111111111111111111111111111".to_string(),
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let loaded: SavedIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.last_saved_block, 123_456_789);
        assert_eq!(loaded.addresses.len(), 2);
        assert_eq!(loaded.addresses[0], original.addresses[0]);
        assert_eq!(loaded.addresses[1], original.addresses[1]);
    }

    #[test]
    fn test_saved_index_address_parses_back_correctly() {
        // Verify the `{a:#x}` format used by save_index() round-trips through parse()
        let addr = Address::repeat_byte(0xAB);
        let formatted = format!("{addr:#x}");
        let parsed: Address = formatted.parse().unwrap();
        assert_eq!(parsed, addr);
    }

    #[test]
    fn test_saved_index_empty_addresses_serializes_cleanly() {
        let s = SavedIndex { last_saved_block: 42, addresses: vec![] };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"last_saved_block\":42"));
        assert!(json.contains("\"addresses\":[]"));
    }

    #[test]
    fn test_saved_index_unknown_fields_ignored() {
        // Forward-compatibility: extra JSON fields must not break deserialization
        let json = r#"{"last_saved_block":7,"addresses":[],"future_field":"ignored"}"#;
        // serde default is to error on unknown fields unless #[serde(deny_unknown_fields)]
        // We do NOT use deny_unknown_fields, so this should succeed
        let s: Result<SavedIndex, _> = serde_json::from_str(json);
        assert!(s.is_ok(), "Unknown fields must not break SavedIndex deserialization");
    }

    // ── save_index / load_saved_index (file I/O via tempfile) ──

    #[test]
    fn test_index_file_roundtrip_via_tempdir() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("user_index.json");

        // Build a small in-memory index
        let mut idx: HashMap<Address, UserPosition> = HashMap::new();
        let u1 = Address::repeat_byte(0x11);
        let u2 = Address::repeat_byte(0x22);
        idx.insert(u1, make_position(1_050_000_000_000_000_000, 1_000));
        idx.insert(u2, make_position(1_100_000_000_000_000_000, 2_000));

        // Simulate what save_index() does (without hard-coded path)
        let saved = SavedIndex {
            last_saved_block: 9_999_999,
            addresses: idx.keys().map(|a| format!("{a:#x}")).collect(),
        };
        let json = serde_json::to_string(&saved).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Simulate what load_saved_index() does
        let loaded: SavedIndex = serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap()
        ).unwrap();

        assert_eq!(loaded.last_saved_block, 9_999_999);
        assert_eq!(loaded.addresses.len(), 2);

        let parsed: std::collections::HashSet<Address> = loaded.addresses.iter()
            .map(|s| s.parse::<Address>().unwrap())
            .collect();
        assert!(parsed.contains(&u1), "u1 missing after round-trip");
        assert!(parsed.contains(&u2), "u2 missing after round-trip");
    }

    #[test]
    fn test_load_saved_index_missing_file_returns_default() {
        // load_saved_index() on a non-existent file must return SavedIndex::default()
        // We can't call load_saved_index() directly (hard-coded path), but we can
        // test the underlying logic: read_to_string on missing file → None → default
        let result: Option<SavedIndex> = std::fs::read_to_string("/nonexistent/path/xyz.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        assert!(result.is_none());
        // This is exactly what load_saved_index() falls back to
        let fallback = result.unwrap_or_default();
        assert_eq!(fallback.last_saved_block, 0);
        assert!(fallback.addresses.is_empty());
    }

    // ── DEFAULT_LOOKBACK ───────────────────────────────────────

    #[test]
    fn test_default_lookback_covers_at_least_one_week() {
        // Arbitrum ~0.25s/block → 4M blocks ≈ 11.6 days
        // One week = 604_800s / 0.25s = 2_419_200 blocks
        const ONE_WEEK_BLOCKS: u64 = 2_500_000;
        assert!(
            DEFAULT_LOOKBACK >= ONE_WEEK_BLOCKS,
            "DEFAULT_LOOKBACK {DEFAULT_LOOKBACK} < 1 week ({ONE_WEEK_BLOCKS} blocks)"
        );
    }

    #[test]
    fn test_default_lookback_not_unreasonably_large() {
        // 20M blocks at 0.25s = ~58 days. More than that makes cold-start too slow.
        assert!(
            DEFAULT_LOOKBACK <= 20_000_000,
            "DEFAULT_LOOKBACK {DEFAULT_LOOKBACK} is too large (cold-start would be very slow)"
        );
    }

    // ── QuoterV2 — logic (no RPC needed) ──────────────────────

    #[test]
    fn test_uniswap_quoter_v2_address_is_not_zero() {
        assert_ne!(UNISWAP_QUOTER_V2, Address::ZERO);
    }

    #[test]
    fn test_fee_tier_candidates_logic() {
        // Simulate the candidate-picking logic from best_fee_tier()
        // without making any RPC calls.
        let candidates: [(u32, Option<U256>); 4] = [
            (500,   Some(U256::from(1_050_000u64))), // best output
            (3000,  Some(U256::from(1_030_000u64))),
            (100,   None),                           // pool doesn't exist
            (10000, Some(U256::from(900_000u64))),
        ];

        let best = candidates.iter()
            .filter_map(|(fee, out)| out.map(|o| (*fee, o)))
            .max_by_key(|(_, out)| *out);

        assert_eq!(best, Some((500u32, U256::from(1_050_000u64))));
    }

    #[test]
    fn test_fee_tier_candidates_all_none_returns_none() {
        let candidates: [(u32, Option<U256>); 4] = [
            (500,   None),
            (3000,  None),
            (100,   None),
            (10000, None),
        ];
        let best = candidates.iter()
            .filter_map(|(fee, out)| out.map(|o| (*fee, o)))
            .max_by_key(|(_, out)| *out);
        assert!(best.is_none());
    }

    #[test]
    fn test_min_swap_out_formula() {
        // Verify the minSwapOut = flash_repay + min_profit formula
        // flash premium on Aave V3 = 9 bps
        let premium: u64 = 9;
        let debt_to_cover = U256::from(100_000_000u64); // 100 USDC (6 dec)
        let min_profit    = U256::from(2_000_000u64);   // $2 in USDC units

        let flash_repay = debt_to_cover
            + debt_to_cover * U256::from(premium) / U256::from(10_000u64);
        let min_swap_out = flash_repay + min_profit;

        // 9 bps of 100_000_000 = 90_000
        // flash_repay = 100_000_000 + 90_000 = 100_090_000
        // min_swap_out = 100_090_000 + 2_000_000 = 102_090_000
        assert_eq!(flash_repay, U256::from(100_090_000u64));
        assert_eq!(min_swap_out, U256::from(102_090_000u64));
    }

    // ── token_unit ────────────────────────────────────────────

    #[test]
    fn test_token_unit_usdc_6dec() {
        let usdc = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
        assert_eq!(token_unit(usdc), U256::from(1_000_000u64)); // 10^6
    }

    #[test]
    fn test_token_unit_weth_18dec() {
        let weth = address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1");
        assert_eq!(token_unit(weth), U256::from(1_000_000_000_000_000_000u64)); // 10^18
    }

    #[test]
    fn test_token_unit_wbtc_8dec() {
        let wbtc = address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f");
        assert_eq!(token_unit(wbtc), U256::from(100_000_000u64)); // 10^8
    }

    #[test]
    fn test_token_unit_unknown_defaults_to_1e18() {
        let unknown = Address::repeat_byte(0xAA);
        assert_eq!(token_unit(unknown), U256::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn test_token_unit_all_known_tokens_nonzero() {
        for (addr, name, _) in TOKENS {
            let unit = token_unit(*addr);
            assert!(unit > U256::ZERO, "token_unit for {name} returned 0");
            assert!(unit <= U256::from(10u64).pow(U256::from(18u64)),
                "token_unit for {name} > 1e18 (unexpected decimals)");
        }
    }

    // ── fee cache logic (no RPC) ──────────────────────────────

    #[test]
    fn test_cache_ttl_is_reasonable() {
        // 10 000 blocks at 0.25 s/block = 2 500 s ≈ 42 min
        assert!(CACHE_TTL_BLOCKS >= 1_000, "TTL too short — would thrash QuoterV2");
        assert!(CACHE_TTL_BLOCKS <= 50_000, "TTL too long — stale routes risk");
    }

    #[test]
    fn test_cache_hit_returns_tier() {
        // Simulate the cache-hit branch of cached_fee_tier() without RPC
        let mut cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
        let weth = address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1");
        let usdc = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
        let current_block = 1_000u64;
        let verified_at   =   900u64; // age = 100, well within TTL

        cache.insert((weth, usdc), (500, verified_at));

        // Inline cache-hit logic
        let key = (weth, usdc);
        let result = if let Some(&(tier, va)) = cache.get(&key) {
            if current_block.saturating_sub(va) < CACHE_TTL_BLOCKS {
                Some(tier)
            } else { None }
        } else { None };

        assert_eq!(result, Some(500u32));
    }

    #[test]
    fn test_cache_miss_on_unknown_pair() {
        let cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        assert!(cache.get(&(a, b)).is_none());
    }

    #[test]
    fn test_cache_stale_when_past_ttl() {
        let mut cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let verified_at   =     100u64;
        let current_block = 200_000u64; // age >> TTL

        cache.insert((a, b), (3000, verified_at));

        let age = current_block.saturating_sub(verified_at);
        assert!(age >= CACHE_TTL_BLOCKS, "should be stale");
    }

    #[test]
    fn test_cache_symmetric_insert() {
        // When we store A→B, we also store B→A (same Uniswap V3 pool)
        let mut cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
        let a = Address::repeat_byte(0xAA);
        let b = Address::repeat_byte(0xBB);
        let tier = 500u32;
        let blk  = 5_000u64;

        cache.insert((a, b), (tier, blk));
        cache.insert((b, a), (tier, blk)); // symmetric

        assert_eq!(cache.get(&(a, b)).map(|v| v.0), Some(500));
        assert_eq!(cache.get(&(b, a)).map(|v| v.0), Some(500));
    }

    #[test]
    fn test_cache_newer_entry_overwrites_stale() {
        let mut cache: HashMap<(Address, Address), (u32, u64)> = HashMap::new();
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);

        cache.insert((a, b), (3000, 100));   // old entry
        cache.insert((a, b), (500,  9_999)); // fresher re-check found better pool

        assert_eq!(cache[&(a, b)].0, 500);
        assert_eq!(cache[&(a, b)].1, 9_999);
    }

    #[test]
    fn test_prewarm_pairs_are_non_empty() {
        // Verify the pre-warm logic produces pairs — regression guard
        let stable_addrs: Vec<Address> = TOKENS.iter()
            .filter(|(_, _, d)| *d == 6)
            .map(|(a, _, _)| *a)
            .collect();
        let major_addrs: Vec<Address> = TOKENS.iter()
            .filter(|(_, _, d)| *d != 6)
            .map(|(a, _, _)| *a)
            .collect();

        let mut pairs: Vec<(Address, Address)> = Vec::new();
        for s in &stable_addrs {
            for m in &major_addrs {
                pairs.push((*m, *s));
                pairs.push((*s, *m));
            }
        }

        // 6-dec tokens: USDC, USDCe, USDT = 3  (DAI and LUSD are 18-dec)
        // non-6-dec:    WETH, DAI, WBTC, wstETH, ARB, LINK, rETH, LUSD = 8
        // 3 × 8 × 2 dirs = 48 pairs
        assert_eq!(stable_addrs.len(), 3, "expected 3 six-dec tokens (USDC/USDCe/USDT)");
        assert_eq!(major_addrs.len(),  8, "expected 8 non-six-dec tokens");
        assert_eq!(pairs.len(), 48, "expected 48 directed stable↔major pairs");

        // No pair should have identical token_in and token_out
        for (a, b) in &pairs {
            assert_ne!(a, b, "self-swap pair found");
        }
    }

    // ── Parallel RPC structure — timing proof ─────────────────
    //
    // We can't call real RPC in unit tests, but we can prove the
    // join_all pattern fires N tasks in parallel by timing mock
    // futures.  Each mock sleeps for D ms; if sequential that
    // would take N×D ms, but parallel it takes only ~D ms.

    #[tokio::test]
    async fn test_parallel_join_all_fires_concurrently() {
        use std::time::{Duration, Instant};

        const N: usize = 11; // mirrors TOKENS.len()
        const DELAY_MS: u64 = 20;

        // Build N independent async tasks, each sleeping DELAY_MS
        let futs: Vec<_> = (0..N)
            .map(|_| async move {
                tokio::time::sleep(Duration::from_millis(DELAY_MS)).await;
                1u32
            })
            .collect();

        let t0 = Instant::now();
        let results = futures_util::future::join_all(futs).await;
        let elapsed = t0.elapsed();

        // All tasks produced their value
        assert_eq!(results.len(), N);
        assert!(results.iter().all(|&v| v == 1));

        // Parallel: total time ≈ DELAY_MS, not N × DELAY_MS
        // Allow 2× DELAY_MS for CI jitter
        let max_parallel_ms = DELAY_MS * 2;
        let sequential_ms   = DELAY_MS * N as u64;
        assert!(
            elapsed < Duration::from_millis(max_parallel_ms),
            "join_all took {}ms — expected <{}ms (would be {}ms sequential)",
            elapsed.as_millis(), max_parallel_ms, sequential_ms
        );
    }

    #[tokio::test]
    async fn test_parallel_reserve_data_count_matches_tokens() {
        // Verify that iterating TOKENS with join_all produces exactly
        // TOKENS.len() results — no off-by-one, no dropped entries.
        use std::time::{Duration, Instant};

        let futs: Vec<_> = TOKENS.iter()
            .map(|(tok, name, decimals)| {
                let addr = *tok;
                let sym  = *name;
                let dec  = *decimals;
                async move {
                    // Simulate the getUserReserveData latency
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    (addr, sym, dec, true) // (addr, name, decimals, ok)
                }
            })
            .collect();

        let t0 = Instant::now();
        let results = futures_util::future::join_all(futs).await;
        let elapsed = t0.elapsed();

        assert_eq!(results.len(), TOKENS.len(), "result count must match TOKENS");
        // Each result maps back to the right token
        for ((tok, name, dec, ok), (expected_tok, expected_name, expected_dec)) in
            results.iter().zip(TOKENS.iter())
        {
            assert_eq!(tok,  expected_tok);
            assert_eq!(name, expected_name);
            assert_eq!(dec,  expected_dec);
            assert!(ok);
        }
        // 11 × 5ms sequential = 55ms; parallel should be ~5ms
        assert!(
            elapsed < Duration::from_millis(30),
            "parallel reserve fetch took {}ms — expected <30ms (was ~55ms sequential)",
            elapsed.as_millis()
        );
    }
}
