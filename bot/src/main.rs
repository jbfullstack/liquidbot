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
}

// ═══════════════════════════════════════════════════════════════
// Constants — Aave V3 Arbitrum One
// ═══════════════════════════════════════════════════════════════

const AAVE_POOL: Address          = address!("794a61358D6845594F94dc1DB02A252b5b4814aD");
const AAVE_DATA_PROVIDER: Address = address!("69FA688f1Dc47d4B5d8029D5a35FB7a548310654");
/// Chainlink ETH/USD price feed on Arbitrum One (8 decimals)
const CHAINLINK_ETH_USD: Address  = address!("639Fe6ab55C921f74e7fac1ee960C0B6293ba612");

/// Major Aave V3 reserve tokens on Arbitrum: (address, symbol, decimals)
const TOKENS: &[(Address, &str, u8)] = &[
    (address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"), "WETH",   18),
    (address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), "USDC",    6),
    (address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), "USDCe",   6),
    (address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), "USDT",    6),
    (address!("DA10009cBd5D07dd0CeCc66161FC93D7c9000da1"), "DAI",    18),
    (address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), "WBTC",    8),
    (address!("5979D7b546E38E9Ab8F25A6E70b5CdE5A8C7A1D0"), "wstETH", 18),
    (address!("912CE59144191C1204E64559FE8253a0e49E6548"), "ARB",    18),
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

    // ── Build user index from recent Borrow events ──
    let user_index: UserIndex = Arc::new(RwLock::new(HashMap::new()));
    let current_block: u64 = http_ro.get_block_number().await?;
    let lookback: u64 = 200_000;
    let from = current_block.saturating_sub(lookback);

    tracing::info!("📡 Indexing borrowers from block {from} to {current_block}...");

    // drpc free tier: max 9 999 blocks per eth_getLogs request → chunk it
    const CHUNK: u64 = 9_000;
    let mut borrowers: HashSet<Address> = HashSet::new();
    let mut chunk_start = from;
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
                {
                    let mut due: Vec<(Address, U256)> = {
                        let idx = user_index.read().await;
                        idx.iter()
                            .filter(|(u, p)| p.next_refresh_at <= bn && !at_risk_users.contains(u))
                            .map(|(u, p)| (*u, p.health_factor))
                            .collect()
                    };
                    // Sort by HF ascending → riskiest users processed first within the cap
                    due.sort_unstable_by_key(|(_, hf)| *hf);
                    due.truncate(MAX_REFRESH_PER_BLOCK);

                    for (user, _) in due {
                        if let Ok(d) = pool.getUserAccountData(user).call().await {
                            let interval = refresh_interval_blocks(d.healthFactor);
                            let mut idx = user_index.write().await;
                            remove_if_repaid(&mut idx, &user, d.totalDebtBase);
                            if let Some(pos) = idx.get_mut(&user) {
                                pos.health_factor = d.healthFactor;
                                pos.total_debt_base = d.totalDebtBase;
                                pos.total_collateral_base = d.totalCollateralBase;
                                pos.last_block = bn;
                                pos.next_refresh_at = bn + interval;
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

                    for &(tok, name, decimals) in TOKENS {
                        if let Ok(rd) = data_prov.getUserReserveData(tok, *user).call().await {
                            if rd.currentVariableDebt > best_debt.1 {
                                best_debt = (tok, rd.currentVariableDebt, name, decimals);
                            }
                            if rd.currentATokenBalance > best_coll.1 && rd.usageAsCollateralEnabled {
                                best_coll = (tok, rd.currentATokenBalance, name, decimals);
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

                    // Uniswap V3 fee tier:
                    // 0 when no swap is needed (same asset), otherwise 3000 = 0.30%
                    let fee_tier = if best_debt.0 == best_coll.0 {
                        alloy::primitives::Uint::<24, 1>::from(0u32)
                    } else {
                        alloy::primitives::Uint::<24, 1>::from(3000u32)
                    };

                    // min_profit in debt token native units (correct per-token scaling)
                    let min_profit = min_profit_raw(cfg.min_profit_usd, best_debt.3, eth_price_usd);

                    // minSwapOut = U256::ZERO — no slippage bound on DEX swap.
                    // estimateGas simulation + minProfit are the profitability guards.
                    // TODO: add Uniswap quoter call for proper slippage protection.
                    let tx = liquidator.liquidate(
                        *user,
                        best_coll.0,
                        best_debt.0,
                        debt_to_cover,
                        fee_tier,
                        U256::ZERO,
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

    #[test]
    fn test_tokens_table_has_correct_len() {
        assert_eq!(TOKENS.len(), 8);
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
}
