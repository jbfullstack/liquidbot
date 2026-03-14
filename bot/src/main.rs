//! Liquidator Bot — Phase 1
//! Monitors Aave V3 on Arbitrum for undercollateralized positions
//! and liquidates them using flash loans.

mod config;
mod stats;
mod telegram;

use config::Config;
use stats::StatsStore;
use telegram::TelegramNotifier;
use eyre::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use alloy::{
    network::EthereumWallet,
    primitives::{address, Address, U256},
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

const AAVE_POOL: Address = address!("794a61358D6845594F94dc1DB02A252b5b4814aD");
const AAVE_DATA_PROVIDER: Address = address!("69FA688f1Dc47d4B5d8029D5a35FB7a548310654");

/// Major Aave V3 reserve tokens on Arbitrum
const TOKENS: &[(Address, &str)] = &[
    (address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"), "WETH"),
    (address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), "USDC"),
    (address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), "USDCe"),
    (address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), "USDT"),
    (address!("DA10009cBd5D07dd0CeCc66161FC93D7c9000da1"), "DAI"),
    (address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), "WBTC"),
    (address!("5979D7b546E38E9Ab8F25A6E70b5CdE5A8C7A1D0"), "wstETH"),
    (address!("912CE59144191C1204E64559FE8253a0e49E6548"), "ARB"),
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
}

type UserIndex = Arc<RwLock<HashMap<Address, UserPosition>>>;

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
    let http = ProviderBuilder::new()
        .wallet(wallet.clone())
        .on_http(cfg.rpc_http_url.parse()?);

    let ws = ProviderBuilder::new()
        .on_ws(WsConnect::new(&cfg.rpc_ws_url))
        .await?;

    // ── Contracts ──
    let contract_addr: Address = cfg.contract_address.parse()?;
    let liquidator = IFlashLiquidator::new(contract_addr, &http);
    let pool = IAavePool::new(AAVE_POOL, &http);
    let data_prov = IAaveDataProvider::new(AAVE_DATA_PROVIDER, &http);

    // ── Verify ──
    let owner = liquidator.owner().call().await?._0;
    if owner != wallet_addr {
        eyre::bail!("Wallet {wallet_addr} is not owner ({owner})");
    }
    if liquidator.paused().call().await?._0 {
        eyre::bail!("Contract is paused");
    }
    let premium = liquidator.getFlashPremiumBps().call().await?._0;
    tracing::info!("✅ Connected. Flash premium: {premium} bps");

    let eth_bal = http.get_balance(wallet_addr).await?;
    tracing::info!("✅ ETH balance: {:.6}", eth_bal.to::<u128>() as f64 / 1e18);

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
    let current_block = http.get_block_number().await?;
    let lookback: u64 = 50_000;
    let from = current_block.saturating_sub(lookback);

    tracing::info!("📡 Indexing borrowers from block {from} to {current_block}...");

    let log_filter = Filter::new()
        .address(AAVE_POOL)
        .event_signature(IAavePool::Borrow::SIGNATURE_HASH)
        .from_block(from)
        .to_block(current_block);

    let logs = http.get_logs(&log_filter).await?;
    let mut borrowers: Vec<Address> = Vec::new();
    for log in &logs {
        if let Ok(ev) = IAavePool::Borrow::decode_log_data(log.data(), true) {
            if !borrowers.contains(&ev.user) {
                borrowers.push(ev.user);
            }
        }
    }
    tracing::info!("Found {} unique borrowers", borrowers.len());

    // Batch health factor checks
    let hf_thresh = U256::from((cfg.health_factor_threshold * 1e18) as u128);
    let mut at_risk = 0u32;

    for chunk in borrowers.chunks(20) {
        for &user in chunk {
            if let Ok(d) = pool.getUserAccountData(user).call().await {
                if d.totalDebtBase > U256::ZERO {
                    if d.healthFactor < hf_thresh { at_risk += 1; }
                    user_index.write().await.insert(user, UserPosition {
                        health_factor: d.healthFactor,
                        total_collateral_base: d.totalCollateralBase,
                        total_debt_base: d.totalDebtBase,
                        last_block: current_block,
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
            eth_bal.to::<u128>() as f64 / 1e18,
            tracked, at_risk,
        ).await;
    }

    // ── Shared state for command handler ──
    let shared_tracked = Arc::new(RwLock::new(tracked));
    let shared_at_risk = Arc::new(RwLock::new(at_risk));

    // ── Spawn Telegram command listener (separate task, never blocks bot) ──
    if let Some(ref tg) = tg {
        let tg_cmd = tg.clone();
        let tracked_ref = shared_tracked.clone();
        let risk_ref = shared_at_risk.clone();
        let hot = format!("{wallet_addr}");
        tokio::spawn(async move {
            tg_cmd.run_command_listener(
                std::time::Instant::now(),
                "stats.json".to_string(),
                hot,
                tracked_ref,
                risk_ref,
            ).await;
        });
        tracing::info!("📱 Telegram commands: /status /stats /json /help");
    }

    // ── Main loop: new blocks via WebSocket ──
    tracing::info!("🔄 Listening for new blocks...");

    let sub = ws.subscribe_blocks().await?;
    let mut stream = sub.into_stream();

    let mut stats_liq = 0u32;
    let mut stats_profit = 0.0f64;
    let one_e18 = U256::from(10u64).pow(U256::from(18u64));

    use futures_util::StreamExt;

    loop {
        tokio::select! {
            Some(block) = stream.next() => {
                let bn = block.header.number;

                // Collect at-risk users
                let at_risk_users: Vec<Address> = {
                    let idx = user_index.read().await;
                    idx.iter()
                        .filter(|(_, p)| p.health_factor < hf_thresh && p.total_debt_base > U256::ZERO)
                        .map(|(a, _)| *a)
                        .collect()
                };

                // Update shared state for /status command
                *shared_at_risk.write().await = at_risk_users.len() as u32;
                *shared_tracked.write().await = user_index.read().await.len();

                if at_risk_users.is_empty() {
                    if bn % 200 == 0 {
                        tracing::info!("Block {bn} | {tracked} tracked | 0 at-risk | {stats_liq} liq (${stats_profit:.2})");
                    }
                    continue;
                }

                for user in &at_risk_users {
                    let Ok(d) = pool.getUserAccountData(*user).call().await else { continue };
                    let hf = d.healthFactor;
                    let debt_usd = d.totalDebtBase.to::<u128>() as f64 / 1e8;

                    // Update index
                    if let Some(pos) = user_index.write().await.get_mut(user) {
                        pos.health_factor = hf;
                        pos.total_debt_base = d.totalDebtBase;
                        pos.total_collateral_base = d.totalCollateralBase;
                        pos.last_block = bn;
                    }

                    if hf >= one_e18 { continue; } // Not liquidatable

                    tracing::warn!("🎯 LIQUIDATABLE: {user} HF={:.6} debt=${debt_usd:.2}",
                        hf.to::<u128>() as f64 / 1e18);

                    // Skip tiny positions
                    if debt_usd < cfg.min_profit_usd * 10.0 { continue; }

                    // Find best debt + collateral
                    let mut best_debt = (Address::ZERO, U256::ZERO, "");
                    let mut best_coll = (Address::ZERO, U256::ZERO, "");

                    for &(tok, name) in TOKENS {
                        if let Ok(rd) = data_prov.getUserReserveData(tok, *user).call().await {
                            if rd.currentVariableDebt > best_debt.1 {
                                best_debt = (tok, rd.currentVariableDebt, name);
                            }
                            if rd.currentATokenBalance > best_coll.1 && rd.usageAsCollateralEnabled {
                                best_coll = (tok, rd.currentATokenBalance, name);
                            }
                        }
                    }

                    if best_debt.0 == Address::ZERO || best_coll.0 == Address::ZERO { continue; }

                    tracing::info!("  Debt: {} {} | Coll: {} {}", best_debt.1, best_debt.2, best_coll.1, best_coll.2);

                    // Close factor: 50% default, 100% if HF < 0.95
                    let hf_095 = U256::from(95u64) * U256::from(10u64).pow(U256::from(16u64));
                    let debt_to_cover = if hf < hf_095 {
                        best_debt.1
                    } else {
                        best_debt.1 / U256::from(2u64)
                    };

                    let fee_tier: u32 = if best_debt.0 == best_coll.0 { 0 } else { 3000 };
                    let min_profit_raw = U256::from((cfg.min_profit_usd * 1e6) as u128);

                    // Simulate via estimateGas
                    let tx = liquidator.liquidate(
                        *user, best_coll.0, best_debt.0,
                        debt_to_cover, fee_tier.try_into().unwrap_or(3000),
                        U256::ZERO, min_profit_raw,
                    );

                    match tx.estimate_gas().await {
                        Ok(gas) => {
                            let gp = http.get_gas_price().await.unwrap_or(100_000_000);
                            let gas_usd = gas as f64 * gp as f64 / 1e18 * 3500.0;
                            let expected_profit = debt_usd * 0.04;
                            let close_pct: u8 = if hf < hf_095 { 100 } else { 50 };
                            let hf_display = hf.to::<u128>() as f64 / 1e18;

                            tracing::info!("  Gas: {gas} (~${gas_usd:.4})");

                            if gas_usd > debt_usd * 0.05 {
                                tracing::info!("  Skip: gas too high");
                                // Notification fire-and-forget (non-blocking)
                                if let Some(tg) = tg.clone() {
                                    let u = format!("{user}");
                                    let reason = format!("Gas ${:.4} > 5% dette ${:.2}", gas_usd, debt_usd);
                                    tokio::spawn(async move { tg.notify_simulation_skip(&u, &reason).await; });
                                }
                                continue;
                            }

                            // ── CRITICAL PATH: send tx IMMEDIATELY, no Telegram here ──
                            tracing::info!("🚀 EXECUTING {user}!");

                            let send_result = tx.gas(gas * 13 / 10).send().await;

                            // ── TX sent (or failed). Now we have time for notifications ──
                            match send_result {
                                Ok(pending) => {
                                    let tx_hash = format!("{:?}", pending.tx_hash());
                                    tracing::info!("  TX: {tx_hash}");

                                    match pending.get_receipt().await {
                                        Ok(receipt) if receipt.status() => {
                                            stats_liq += 1;
                                            stats_profit += expected_profit;

                                            // Record in persistent stats
                                            stats.record_liquidation(
                                                &format!("{user}"), best_debt.2, best_coll.2,
                                                debt_usd, expected_profit - gas_usd, gas_usd,
                                                &tx_hash, true,
                                            );
                                            let summary = stats.format_summary();

                                            let new_eth = http.get_balance(wallet_addr).await
                                                .unwrap_or(U256::ZERO).to::<u128>() as f64 / 1e18;

                                            tracing::info!("  ✅ ~${expected_profit:.2} | Total: {stats_liq} (${stats_profit:.2})");

                                            // Fire-and-forget combined notification
                                            if let Some(tg) = tg.clone() {
                                                let u = format!("{user}");
                                                let h = tx_hash.clone();
                                                let dt = best_debt.2.to_string();
                                                let da = format!("{}", best_debt.1);
                                                let ct = best_coll.2.to_string();
                                                let ca = format!("{}", best_coll.1);
                                                tokio::spawn(async move {
                                                    tg.notify_liquidation_complete(
                                                        &u, hf_display, debt_usd,
                                                        &dt, &da, &ct, &ca,
                                                        close_pct, gas, gas_usd, expected_profit,
                                                        true, &h, "",
                                                        expected_profit, gas_usd, expected_profit - gas_usd,
                                                        new_eth, &summary,
                                                    ).await;
                                                });
                                            }
                                        }
                                        Ok(_) => {
                                            tracing::warn!("  ❌ Reverted on-chain");

                                            stats.record_liquidation(
                                                &format!("{user}"), best_debt.2, best_coll.2,
                                                debt_usd, 0.0, gas_usd,
                                                &tx_hash, false,
                                            );
                                            let summary = stats.format_summary();

                                            let new_eth = http.get_balance(wallet_addr).await
                                                .unwrap_or(U256::ZERO).to::<u128>() as f64 / 1e18;

                                            if let Some(tg) = tg.clone() {
                                                let u = format!("{user}");
                                                let h = tx_hash.clone();
                                                let dt = best_debt.2.to_string();
                                                let da = format!("{}", best_debt.1);
                                                let ct = best_coll.2.to_string();
                                                let ca = format!("{}", best_coll.1);
                                                tokio::spawn(async move {
                                                    tg.notify_liquidation_complete(
                                                        &u, hf_display, debt_usd,
                                                        &dt, &da, &ct, &ca,
                                                        close_pct, gas, gas_usd, expected_profit,
                                                        false, &h,
                                                        "Reverted on-chain",
                                                        0.0, gas_usd, -gas_usd,
                                                        new_eth, &summary,
                                                    ).await;
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("  Receipt error: {e}");
                                            if let Some(tg) = tg.clone() {
                                                let msg = format!("{e}");
                                                tokio::spawn(async move { tg.notify_error("Receipt", &msg).await; });
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("  Send failed: {e}");
                                    if let Some(tg) = tg.clone() {
                                        let msg = format!("{e}");
                                        tokio::spawn(async move { tg.notify_error("TX send", &msg).await; });
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            tracing::debug!("  Simulation reverted (expected)");
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("\n🛑 Session: {stats_liq} liquidations, ~${stats_profit:.2}");
                if let Some(ref tg) = tg {
                    let summary = stats.format_summary();
                    let eth = http.get_balance(wallet_addr).await
                        .unwrap_or(U256::ZERO).to::<u128>() as f64 / 1e18;
                    let msg = format!(
                        "⚡ LiqBot 🛑 <b>Bot arrêté</b>\n\
                        🔑 Hot: {:.6} ETH\n\n\
                        ━━━━━━━━━━━━━━━━━━━━━━━━\n\
                        {}", eth, summary,
                    );
                    tg.send_raw(&msg).await;
                }
                break;
            }
        }
    }

    Ok(())
}
