//! Telegram notification module
//!
//! Sends structured messages to your Telegram chat for every bot action.
//! Setup: message @BotFather on Telegram → /newbot → get token
//! Then message your bot, visit https://api.telegram.org/bot<TOKEN>/getUpdates
//! to find your chat_id.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Commands the Telegram listener sends back to the main loop.
#[derive(Debug)]
pub enum TelegramCommand {
    PauseBot,
    ResumeBot,
    PauseContract,
    ResumeContract,
}

#[derive(Clone)]
pub struct TelegramNotifier {
    token: String,
    chat_id: String,
    client: reqwest::Client,
    bot_name: String,
}

impl TelegramNotifier {
    pub fn new(token: &str, chat_id: &str) -> Self {
        Self {
            token: token.to_string(),
            chat_id: chat_id.to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
            bot_name: "⚡ LiqBot".to_string(),
        }
    }

    /// Send a raw HTML message (public for special cases like shutdown)
    pub async fn send_raw(&self, text: &str) {
        self.send(text).await;
    }

    /// Send a raw HTML message (internal)
    async fn send(&self, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let _ = self.client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            }))
            .send()
            .await;
        // Fire-and-forget: don't let Telegram errors crash the bot
    }

    // ─────────────────────────────────────────────────────────
    // STARTUP
    // ─────────────────────────────────────────────────────────

    pub async fn notify_startup(
        &self,
        hot_wallet: &str,
        cold_wallet: &str,
        contract: &str,
        eth_balance: f64,
        users_tracked: usize,
        at_risk: u32,
    ) {
        let msg = format!(
            "{} <b>Bot démarré</b>\n\
            \n\
            🔑 Hot: <code>{}</code>\n\
            🏦 Cold: <code>{}</code>\n\
            📄 Contract: <code>{}</code>\n\
            ⛽ ETH: <b>{:.6} ETH</b>\n\
            👥 Users suivis: <b>{}</b>\n\
            ⚠️ À risque: <b>{}</b>",
            self.bot_name,
            &hot_wallet[..hot_wallet.len().min(8)],
            &cold_wallet[..cold_wallet.len().min(8)],
            &contract[..contract.len().min(8)],
            eth_balance, users_tracked, at_risk
        );
        self.send(&msg).await;
    }

    // ─────────────────────────────────────────────────────────
    // LIQUIDATION COMPLETE — single combined message (post-tx)
    // Called AFTER the tx result is known. Zero latency impact.
    // ─────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn notify_liquidation_complete(
        &self,
        // Context
        user: &str,
        health_factor: f64,
        debt_usd: f64,
        debt_token: &str,
        debt_amount: &str,
        collateral_token: &str,
        collateral_amount: &str,
        close_factor_pct: u8,
        gas_estimate: u64,
        gas_cost_usd: f64,
        _expected_profit_usd: f64,
        // Result
        success: bool,
        tx_hash: &str,
        failure_reason: &str,
        // Financials
        profit_usd: f64,
        gas_paid_usd: f64,
        net_profit_usd: f64,
        // Wallet
        hot_eth: f64,
        // Full stats summary (pre-formatted)
        stats_summary: &str,
    ) {
        let status = if success { "✅ Réussie" } else { "❌ Échouée" };
        let emoji = if success { "💰" } else { "💸" };

        let mut msg = format!(
            "{} <b>{} Liquidation {}</b>\n\
            \n\
            👤 <code>{}</code> | HF: <b>{:.4}</b>\n\
            💸 {} {} (~${:.2})\n\
            🛡️ {} {}\n\
            📊 Close {}% | Gas {} (~${:.4})\n",
            self.bot_name, emoji, status,
            &user[..user.len().min(10)],
            health_factor,
            debt_amount, debt_token, debt_usd,
            collateral_amount, collateral_token,
            close_factor_pct, gas_estimate, gas_cost_usd,
        );

        if !tx_hash.is_empty() {
            msg.push_str(&format!(
                "🔗 <a href=\"https://arbiscan.io/tx/{}\">Arbiscan</a>\n",
                tx_hash,
            ));
        }

        msg.push('\n');

        if success {
            msg.push_str(&format!(
                "✨ <b>+${:.4}</b> (brut ${:.4} - gas ${:.4})\n",
                net_profit_usd, profit_usd, gas_paid_usd,
            ));
        } else {
            msg.push_str(&format!(
                "❓ {}\n⛽ Gas perdu: -${:.4}\n",
                failure_reason, gas_paid_usd,
            ));
        }

        msg.push_str(&format!("🔑 Hot: {:.6} ETH\n", hot_eth));

        // Append full historical stats
        msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
        msg.push_str(stats_summary);

        self.send(&msg).await;
    }

    pub async fn notify_simulation_skip(
        &self,
        user: &str,
        reason: &str,
    ) {
        let msg = format!(
            "{} 🔍 Simulation skip\n\
            👤 <code>{}</code>\n\
            💭 {}",
            self.bot_name, &user[..user.len().min(10)], reason,
        );
        self.send(&msg).await;
    }

    // ─────────────────────────────────────────────────────────
    // COLD WALLET SWEEP (standalone — for ETH sweeps)
    // ─────────────────────────────────────────────────────────

    pub async fn notify_eth_sweep(
        &self,
        amount_eth: f64,
        hot_remaining_eth: f64,
    ) {
        let msg = format!(
            "{} 💸 <b>ETH envoyé au cold wallet</b>\n\
            \n\
              Envoyé: <b>{:.6} ETH</b>\n\
              Hot restant: {:.6} ETH",
            self.bot_name, amount_eth, hot_remaining_eth,
        );
        self.send(&msg).await;
    }

    // ─────────────────────────────────────────────────────────
    // ERRORS / ALERTS
    // ─────────────────────────────────────────────────────────

    pub async fn notify_error(&self, context: &str, error: &str) {
        let msg = format!(
            "{} 🚨 <b>Erreur</b>\n\
            \n\
            📍 {}\n\
            ❗ <code>{}</code>",
            self.bot_name, context, &error[..error.len().min(500)],
        );
        self.send(&msg).await;
    }

    /// Send a grouped summary of missed liquidations (buffered over 8s window).
    /// `labels` maps full competitor address → label (e.g. "pb_01").
    pub async fn notify_missed_batch(
        &self,
        events: &[crate::MissedEvent],
        labels: &std::collections::HashMap<String, String>,
    ) {
        if events.is_empty() { return; }

        let n = events.len();
        let title = if n == 1 {
            format!("{} 😢 <b>Opportunité manquée</b>", self.bot_name)
        } else {
            format!("{} 😢 <b>{n} opportunités manquées</b>", self.bot_name)
        };

        // Count by protocol
        let av3 = events.iter().filter(|e| e.protocol == "AV3").count();
        let rdt = events.iter().filter(|e| e.protocol == "RDT").count();
        let proto_line = match (av3, rdt) {
            (a, 0) => format!("[{a} Aave V3]"),
            (0, r) => format!("[{r} Radiant V2]"),
            (a, r) => format!("[{a} Aave V3 + {r} Radiant V2]"),
        };

        // Detect recurring competitor within this batch
        let mut liq_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in events { *liq_counts.entry(e.liquidator.as_str()).or_insert(0) += 1; }
        let top_liq = liq_counts.iter().max_by_key(|(_, c)| *c);
        let recurring = top_liq
            .filter(|(_, &c)| c > 1)
            .map(|(addr, c)| {
                let lbl = labels.get(*addr).map(|l| format!(" <b>{l}</b>")).unwrap_or_default();
                format!("\n⚔️  <b>Concurrent récurrent:</b>{lbl} <code>{addr}</code> ({c}/{n})")
            })
            .unwrap_or_default();

        // Total estimated profit missed
        let total_est: f64 = events.iter().map(|e| e.est_profit).sum();
        let profit_line = format!("\n💸 Profit estimé manqué: ~<b>${total_est:.2}</b>");

        // Per-event lines
        let mut lines = String::new();
        for (i, e) in events.iter().enumerate() {
            let user_s = &e.user[..e.user.len().min(12)];
            let hf_str = if e.last_hf > 0.0 { format!("HF {:.4}", e.last_hf) } else { "HF ?".into() };
            let tx_link = if e.tx_hash.is_empty() {
                String::new()
            } else {
                format!(" <a href=\"https://arbiscan.io/tx/{}\">tx</a>", e.tx_hash)
            };
            if e.untracked {
                // Position non-indexée : on n'était même pas en compétition
                lines.push_str(&format!(
                    "\n{}. 🔍 [{}] <code>{user_s}…</code> | {:.4} {} → {} | <i>non-indexé</i> | ~${:.2}{tx_link}",
                    i + 1, e.protocol, e.debt_amt, e.debt_sym, e.col_sym, e.est_profit,
                ));
                lines.push_str(&format!("\n   🤖 <code>{}</code>", e.liquidator));
            } else {
                lines.push_str(&format!(
                    "\n{}. [{}] <code>{user_s}…</code> | {:.4} {} → {} | {hf_str} | ~${:.2}{tx_link}",
                    i + 1, e.protocol, e.debt_amt, e.debt_sym, e.col_sym, e.est_profit,
                ));
                // Always show full address + label for competitor analysis
                let lbl = labels.get(&e.liquidator).map(|l| format!(" <b>{l}</b>")).unwrap_or_default();
                let is_top = top_liq.map(|(a, _)| *a == e.liquidator.as_str()).unwrap_or(false);
                if !is_top || n == 1 {
                    lines.push_str(&format!("\n   ⚔️ {lbl} <code>{}</code>", e.liquidator));
                }
            }
        }

        let msg = format!("{title} {proto_line}{profit_line}\n{lines}{recurring}");
        self.send(&msg).await;
    }

    pub async fn notify_low_eth(&self, balance_eth: f64) {
        let msg = format!(
            "{} ⛽ <b>ETH bas !</b>\n\
            \n Balance: <b>{:.6} ETH</b>\n ⚠️ Recharge le hot wallet !",
            self.bot_name, balance_eth,
        );
        self.send(&msg).await;
    }

    // ─────────────────────────────────────────────────────────
    // DAILY SUMMARY (called by a scheduled task)
    // ─────────────────────────────────────────────────────────

    pub async fn notify_daily_summary(
        &self,
        uptime_hours: f64,
        liquidations_today: u32,
        profit_today: f64,
        gas_today: f64,
        total_liquidations: u32,
        total_profit: f64,
        users_tracked: usize,
        at_risk: u32,
        hot_eth: f64,
    ) {
        let msg = format!(
            "{} 📋 <b>Résumé quotidien</b>\n\
            \n\
            ⏱️ Uptime: {:.1}h\n\
            \n\
            📊 <b>Aujourd'hui:</b>\n\
              Liquidations: {}\n\
              Profit: ${:.2}\n\
              Gas: -${:.4}\n\
              Net: <b>${:.2}</b>\n\
            \n\
            📊 <b>Total (session):</b>\n\
              Liquidations: {}\n\
              Profit: <b>${:.2}</b>\n\
            \n\
            👥 Users suivis: {}\n\
            ⚠️ À risque: {}\n\
            ⛽ Hot wallet: {:.6} ETH",
            self.bot_name,
            uptime_hours,
            liquidations_today, profit_today, gas_today,
            profit_today - gas_today,
            total_liquidations, total_profit,
            users_tracked, at_risk, hot_eth,
        );
        self.send(&msg).await;
    }

    // ─────────────────────────────────────────────────────────
    // COMMAND HANDLER — polls for incoming /commands
    // ─────────────────────────────────────────────────────────

    /// Poll for incoming Telegram messages and respond to commands.
    /// Runs in a separate tokio task, never blocks the main bot.
    ///
    /// Commands:
    ///   /status [s]         — bot alive, uptime, current state
    ///   /stats [protocol]   — P&L global ou filtré par protocole
    ///   /hf [protocol]      — positions à risque (filtre protocole optionnel)
    ///   /protocols          — liste tous les protocoles + statut
    ///   /enable <id>        — activer un protocole
    ///   /disable <id>       — désactiver un protocole
    ///   /gas                — solde ETH + estimation jours restants
    ///   /stats_json         — télécharger stats.json
    ///   /missed_json        — télécharger missed.json
    ///   /blind_spot [N]     — résumé des N derniers blind spots (défaut 10)
    ///   /blind_spot_json    — télécharger untracked.json
    ///   /stop_bot           — suspendre liquidations
    ///   /start_bot          — reprendre liquidations
    ///   /pause_contract     — setPaused(true) on-chain
    ///   /resume_contract    — setPaused(false) on-chain
    ///   /logs [N]           — dernières N lignes journalctl
    ///   /restart            — redémarrer le processus (systemd le relance)
    ///   /help               — liste des commandes
    #[allow(clippy::too_many_arguments)]
    pub async fn run_command_listener(
        self,
        started_at: std::time::Instant,
        stats_path: String,
        // Shared state for live info
        hot_wallet: String,
        users_tracked: Arc<tokio::sync::RwLock<usize>>,
        at_risk_count: Arc<tokio::sync::RwLock<u32>>,
        bot_active: Arc<AtomicBool>,
        cmd_sender: tokio::sync::mpsc::Sender<TelegramCommand>,
        hf_list: Arc<tokio::sync::RwLock<Vec<(String, f64, f64, String)>>>,
        hf_threshold: f64,
        min_profit_usd: f64,
        max_gas_gwei: f64,
        dust_excluded: Arc<std::sync::atomic::AtomicU32>,
        eth_balance: Arc<tokio::sync::RwLock<f64>>,
        eth_price: Arc<tokio::sync::RwLock<f64>>,
        // Protocol registry for /protocols, /enable, /disable
        protocol_registry: Arc<tokio::sync::RwLock<crate::protocols::ProtocolRegistry>>,
        // Count of positions liquidated by competitors while we were tracking them
        missed_count: Arc<std::sync::atomic::AtomicU32>,
        // Competitor label registry (pb_01, pb_02…)
        competitor_registry: Arc<tokio::sync::RwLock<crate::competitors::CompetitorStore>>,
    ) {
        // Separate client with long timeout for Telegram long-polling
        let poll_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(330)) // 300s poll + 30s margin
            .build()
            .expect("poll client");

        let mut last_update_id: i64 = 0;

        // Get initial offset (skip old messages on startup)
        match self.get_latest_update_id(&poll_client).await {
            Some(id) => {
                last_update_id = id + 1;
                tracing::info!("📱 Telegram listener prêt (update_id offset={})", id);
            }
            None => {
                tracing::info!("📱 Telegram listener prêt (aucun message précédent)");
            }
        }

        loop {
            let updates = match self.get_updates(&poll_client, last_update_id).await {
                Some(u) => u,
                None => {
                    // Erreur réseau ou API — déjà loggué dans get_updates
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    continue;
                }
            };

            for update in updates {
                let update_id = update["update_id"].as_i64().unwrap_or(0);
                if update_id >= last_update_id {
                    last_update_id = update_id + 1;
                }

                // Only respond to messages from our chat_id
                let chat_id = update["message"]["chat"]["id"].to_string();
                if chat_id.trim_matches('"') != self.chat_id {
                    tracing::debug!("Telegram: message ignoré (chat_id={} != attendu={})", chat_id, self.chat_id);
                    continue;
                }

                let text = update["message"]["text"].as_str().unwrap_or("");
                let cmd = text.split_whitespace().next().unwrap_or("");
                tracing::info!("📱 Telegram commande reçue: '{cmd}' (update_id={update_id})");

                match cmd {
                    "/status" | "/s" => {
                        let uptime = started_at.elapsed();
                        let hours = uptime.as_secs() / 3600;
                        let mins = (uptime.as_secs() % 3600) / 60;
                        let tracked = *users_tracked.read().await;
                        let risk = *at_risk_count.read().await;
                        let active = bot_active.load(Ordering::Relaxed);

                        // Load stats for quick numbers
                        let stats = crate::stats::StatsStore::load();

                        let status_icon = if active { "🟢 <b>Bot is UP</b>" } else { "🟡 <b>Bot PAUSÉ</b>" };
                        let msg = format!(
                            "{} {}\n\
                            \n\
                            ⏱️ Uptime: {}h {}min\n\
                            🔑 Hot: <code>{}</code>\n\
                            👥 Users suivis: {}\n\
                            ⚠️ À risque: {}\n\
                            ✅ Liquidations: {}\n\
                            😢 Ratées (concurrence): {}\n\
                            💰 Profit total: <b>${:.2}</b>",
                            self.bot_name, status_icon,
                            hours, mins,
                            &hot_wallet[..hot_wallet.len().min(10)],
                            tracked, risk,
                            stats.total_successes(),
                            missed_count.load(std::sync::atomic::Ordering::Relaxed),
                            stats.total_profit() - stats.total_gas(),
                        );
                        self.send(&msg).await;
                    }

                    "/stats" | "/bilan" => {
                        // Optional: "/stats aave_v3" or "/stats radiant_v2"
                        let proto_arg = text.split_whitespace().nth(1);
                        let stats = crate::stats::StatsStore::load();
                        let summary = if let Some(proto_id) = proto_arg {
                            // Convert machine ID → display name for record lookup
                            let proto_name = crate::protocols::ALL_PROTOCOLS.iter()
                                .find(|p| p.id == proto_id)
                                .map(|p| p.name)
                                .unwrap_or(proto_id);
                            stats.format_protocol_summary(proto_name)
                        } else {
                            stats.format_summary()
                        };
                        self.send(&summary).await;
                    }

                    "/stats_json" | "/export" => {
                        self.send_document(&stats_path).await;
                    }

                    "/missed_json" => {
                        self.send_document("missed.json").await;
                    }

                    "/blind_spot_json" => {
                        self.send_document("untracked.json").await;
                    }

                    "/blind_spot" => {
                        // Optional: /blind_spot 20 → show last 20 (default 10, max 30)
                        let n: usize = text.split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(10)
                            .min(30);

                        let log = crate::competitors::UntrackedLog::load();

                        if log.records.is_empty() {
                            self.send(&format!(
                                "{} 🔍 <b>Blind spots</b>\n\nAucun blind spot enregistré.",
                                self.bot_name
                            )).await;
                        } else {
                            let total      = log.records.len();
                            let total_profit: f64 = log.records.iter().map(|r| r.est_profit).sum();
                            // Most recent first
                            let shown: Vec<_> = log.records.iter().rev().take(n).collect();

                            let mut lines = vec![
                                format!(
                                    "{} 🔍 <b>Blind spots</b> — {} total · ~<b>${total_profit:.2}</b> manqués",
                                    self.bot_name, total
                                ),
                                format!("Derniers <b>{}</b> enregistrements :\n", shown.len()),
                            ];

                            for (i, r) in shown.iter().enumerate() {
                                let date = &r.timestamp[..r.timestamp.len().min(10)];
                                let liq_s = &r.liquidator[..r.liquidator.len().min(12)];
                                let tx_link = if r.tx_hash.is_empty() {
                                    String::new()
                                } else {
                                    format!(" <a href=\"https://arbiscan.io/tx/{}\">tx</a>", r.tx_hash)
                                };
                                lines.push(format!(
                                    "{}. [{}] {} | {:.2} {} → {} | ~${:.2}{}",
                                    i + 1, r.protocol, date,
                                    r.debt_amt, r.debt_sym, r.col_sym,
                                    r.est_profit, tx_link,
                                ));
                                lines.push(format!(
                                    "   bloc <code>{}</code> · liq <code>{liq_s}…</code>",
                                    r.block,
                                ));
                            }

                            if total > n {
                                lines.push(format!("\n… et {} autres — /blind_spot_json pour tout", total - n));
                            }

                            self.send(&lines.join("\n")).await;
                        }
                    }

                    "/gas" => {
                        let bal   = *eth_balance.read().await;
                        let price = *eth_price.read().await;
                        let bal_usd = bal * price;
                        let stats = crate::stats::StatsStore::load();
                        let total_ops = stats.total_successes() + stats.total_failures();

                        let msg = if total_ops == 0 {
                            // Pas encore de données réelles — estimation théorique
                            let est_ops = (bal_usd / 0.20).floor() as u64;
                            format!(
                                "{} ⛽ <b>Gas &amp; estimations</b>\n\
                                \n\
                                Solde: <b>{:.5} ETH</b> (~${:.2})\n\
                                \n\
                                📊 Pas encore de données (0 opérations)\n\
                                   Estimation théorique à $0.20/op:\n\
                                   ~<b>{est_ops} opérations</b> restantes\n\
                                \n\
                                ⚠️ Alerte auto si &lt; 7 jours estimés",
                                self.bot_name, bal, bal_usd,
                            )
                        } else {
                            let days_active = {
                                use chrono::{NaiveDate, Utc};
                                if let Ok(start) = NaiveDate::parse_from_str(&stats.started_at, "%Y-%m-%d") {
                                    (Utc::now().date_naive() - start).num_days().max(1) as f64
                                } else { 1.0 }
                            };
                            let total_gas  = stats.total_gas();
                            let avg_per_op  = total_gas / total_ops as f64;
                            let avg_per_day = total_gas / days_active;
                            let ops_left  = if avg_per_op  > 0.0 { (bal_usd / avg_per_op).floor()  as i64 } else { 9999 };
                            let days_left = if avg_per_day > 0.0 { (bal_usd / avg_per_day).floor() as i64 } else { 9999 };
                            let alert_icon = if days_left < 7 { "🔴" } else if days_left < 14 { "🟡" } else { "🟢" };

                            format!(
                                "{} ⛽ <b>Gas &amp; estimations</b>\n\
                                \n\
                                Solde: <b>{:.5} ETH</b> (~${:.2})\n\
                                \n\
                                📊 Base ({} ops sur {:.0} jours):\n\
                                   Gas total brûlé: ${:.4}\n\
                                   Coût moyen/op:   ${:.4}\n\
                                   Coût moyen/jour: ${:.4}\n\
                                \n\
                                ⏱️ Estimations (solde ÷ coût moy):\n\
                                   ~<b>{ops_left} opérations</b> restantes\n\
                                   ~<b>{days_left} jours</b> restants\n\
                                \n\
                                {} Alerte auto si &lt; 7 jours estimés",
                                self.bot_name,
                                bal, bal_usd,
                                total_ops, days_active,
                                total_gas, avg_per_op, avg_per_day,
                                alert_icon,
                            )
                        };
                        self.send(&msg).await;
                    }

                    "/hf" => {
                        // Optional protocol filter: "/hf aave_v3" or "/hf radiant_v2"
                        let proto_filter = text.split_whitespace().nth(1);

                        // Clone+filter from the shared list so we release the lock immediately
                        let list: Vec<(String, f64, f64, String)> = {
                            let guard = hf_list.read().await;
                            if let Some(filter) = proto_filter {
                                // Convert machine ID to display name for matching
                                let display_name = crate::protocols::ALL_PROTOCOLS.iter()
                                    .find(|p| p.id == filter)
                                    .map(|p| p.name);
                                guard.iter()
                                    .filter(|(_, _, _, proto)| match display_name {
                                        Some(n) => proto.as_str() == n,
                                        // Fallback: substring match in case user typed display name
                                        None => proto.to_lowercase().contains(&filter.to_lowercase()),
                                    })
                                    .cloned()
                                    .collect()
                            } else {
                                guard.clone()
                            }
                        };

                        let dust  = dust_excluded.load(std::sync::atomic::Ordering::Relaxed);
                        let eth_p = *eth_price.read().await;
                        let min_debt_usd = min_profit_usd * 20.0;
                        let filter_label = proto_filter
                            .map(|f| format!(" [{}]", f))
                            .unwrap_or_default();

                        if list.is_empty() && dust == 0 {
                            self.send(&format!(
                                "{} 🟢 Aucune position{filter_label} à risque actuellement.",
                                self.bot_name
                            )).await;
                        } else {
                            let red:    Vec<_> = list.iter().filter(|(_, hf, _, _)| *hf < 1.0).collect();
                            let yellow: Vec<_> = list.iter().filter(|(_, hf, _, _)| *hf >= 1.0).collect();

                            let gas_est_usd = 300_000.0 * max_gas_gwei * 1e-9 * eth_p;

                            let mut msg = format!(
                                "{} 🔍 <b>{} positions à risque{filter_label}</b> (HF &lt; {:.2})\n",
                                self.bot_name, list.len(), hf_threshold,
                            );

                            // ── 🔴 Section: liquidatables avec profit estimé ──
                            if !red.is_empty() {
                                msg.push_str(&format!("\n🔴 <b>{} liquidatable{}</b> maintenant\n",
                                    red.len(), if red.len() > 1 { "s" } else { "" }));
                                msg.push_str("<pre>");
                                msg.push_str("   HF      Dette    Profit est.   Adresse    Proto\n");
                                for (addr, hf, debt, proto) in red.iter().take(10) {
                                    let short = format!("{}…{}", &addr[..6], &addr[addr.len()-4..]);
                                    let close = if *hf < 0.95 { 1.0_f64 } else { 0.5_f64 };
                                    let profit = debt * close * 0.05 - gas_est_usd;
                                    let proto_short = proto.replace("Aave ", "A").replace(' ', "");
                                    msg.push_str(&format!(
                                        "{:.4}  {:>8.0} $  {:>+8.0} $  {}  {}\n",
                                        hf, debt, profit, short, proto_short,
                                    ));
                                }
                                msg.push_str("</pre>");
                            }

                            // ── 🟡 Section: surveillance ──
                            if !yellow.is_empty() {
                                msg.push_str(&format!("\n🟡 <b>{} en surveillance</b>\n", yellow.len()));
                                msg.push_str("<pre>");
                                msg.push_str("      HF       Dette      Adresse    Proto\n");
                                for (addr, hf, debt, proto) in yellow.iter().take(20) {
                                    let short = format!("{}…{}", &addr[..6], &addr[addr.len()-4..]);
                                    let proto_short = proto.replace("Aave ", "A").replace(' ', "");
                                    msg.push_str(&format!(
                                        "{:.4}  {:>10.2} $  {}  {}\n",
                                        hf, debt, short, proto_short,
                                    ));
                                }
                                if yellow.len() > 20 {
                                    msg.push_str(&format!("+{} autres…", yellow.len() - 20));
                                }
                                msg.push_str("</pre>");
                            }

                            if dust > 0 && proto_filter.is_none() {
                                msg.push_str(&format!(
                                    "\n⚫ <i>{dust} ignorée(s) — dette &lt; ${min_debt_usd:.0} (non rentable)</i>"
                                ));
                            }

                            self.send(&msg).await;
                        }
                    }

                    "/protocols" => {
                        // Build live stats from available shared state
                        let users_total = *users_tracked.read().await;
                        let stats = crate::stats::StatsStore::load();

                        // Per-protocol at-risk count from hf_list
                        let mut at_risk_by_proto: std::collections::HashMap<&'static str, u32> =
                            std::collections::HashMap::new();
                        {
                            let list = hf_list.read().await;
                            for (_, _, _, proto) in list.iter() {
                                if let Some(meta) = crate::protocols::ALL_PROTOCOLS.iter()
                                    .find(|p| p.name == proto.as_str())
                                {
                                    *at_risk_by_proto.entry(meta.id).or_insert(0) += 1;
                                }
                            }
                        }

                        // Build live map: protocol_id → (users, at_risk, net_profit)
                        let mut live: std::collections::HashMap<&'static str, (usize, u32, f64)> =
                            std::collections::HashMap::new();
                        for meta in crate::protocols::ALL_PROTOCOLS {
                            let (_ok, _fail, net) = stats.protocol_quick_stats(meta.name);
                            let at_risk = at_risk_by_proto.get(meta.id).copied().unwrap_or(0);
                            // Phase 1: all users attributed to aave_v3; future protocols get 0
                            let users = if meta.id == "aave_v3" { users_total } else { 0 };
                            live.insert(meta.id, (users, at_risk, net));
                        }

                        let reg = protocol_registry.read().await;
                        let msg = reg.format_for_telegram(&live);
                        self.send(&msg).await;
                    }

                    "/enable" => {
                        let id = text.split_whitespace().nth(1).unwrap_or("");
                        if id.is_empty() {
                            self.send(&format!(
                                "{} ❌ Usage: <code>/enable &lt;id&gt;</code>\n\
                                Exemple: <code>/enable radiant_v2</code>\n\
                                Voir /protocols pour les IDs disponibles.",
                                self.bot_name
                            )).await;
                        } else {
                            let result = protocol_registry.write().await.enable(id);
                            match result {
                                Ok(name) => {
                                    self.send(&format!(
                                        "{} ✅ <b>{name}</b> activé.\n\
                                        Les liquidations reprennent immédiatement.",
                                        self.bot_name
                                    )).await;
                                }
                                Err(msg) => {
                                    self.send(&format!("{} ❌ {msg}", self.bot_name)).await;
                                }
                            }
                        }
                    }

                    "/disable" => {
                        let id = text.split_whitespace().nth(1).unwrap_or("");
                        if id.is_empty() {
                            self.send(&format!(
                                "{} ❌ Usage: <code>/disable &lt;id&gt;</code>\n\
                                Exemple: <code>/disable radiant_v2</code>\n\
                                Voir /protocols pour les IDs disponibles.",
                                self.bot_name
                            )).await;
                        } else {
                            let result = protocol_registry.write().await.disable(id);
                            match result {
                                Ok(name) => {
                                    self.send(&format!(
                                        "{} ⏸ <b>{name}</b> désactivé.\n\
                                        Les liquidations sur ce protocole sont suspendues.",
                                        self.bot_name
                                    )).await;
                                }
                                Err(msg) => {
                                    self.send(&format!("{} ❌ {msg}", self.bot_name)).await;
                                }
                            }
                        }
                    }

                    "/stop_bot" => {
                        let _ = cmd_sender.send(TelegramCommand::PauseBot).await;
                    }

                    "/start_bot" => {
                        let _ = cmd_sender.send(TelegramCommand::ResumeBot).await;
                    }

                    "/pause_contract" => {
                        self.send(&format!("{} ⏳ Envoi de setPaused(true)...", self.bot_name)).await;
                        let _ = cmd_sender.send(TelegramCommand::PauseContract).await;
                    }

                    "/resume_contract" => {
                        self.send(&format!("{} ⏳ Envoi de setPaused(false)...", self.bot_name)).await;
                        let _ = cmd_sender.send(TelegramCommand::ResumeContract).await;
                    }

                    "/logs" => {
                        let n: usize = text.split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(30)
                            .min(1000); // cap raisonnable
                        let service = std::env::var("SERVICE_NAME")
                            .unwrap_or_else(|_| "liquidator".to_string());
                        match tokio::process::Command::new("journalctl")
                            .args(["-u", &service, "-n", &n.to_string(), "--no-pager", "--output=short-precise"])
                            .output()
                            .await
                        {
                            Ok(out) if !out.stdout.is_empty() => {
                                self.send_document_bytes(
                                    &out.stdout,
                                    &format!("liquidator-logs-{n}.txt"),
                                    &format!("📋 Dernières {n} lignes — {service}"),
                                ).await;
                            }
                            Ok(_) => {
                                self.send(&format!(
                                    "{} ❌ Aucun log trouvé pour <code>{service}</code>.\n\
                                    Vérifie que <code>SERVICE_NAME</code> est correct dans .env.",
                                    self.bot_name
                                )).await;
                            }
                            Err(e) => {
                                self.send(&format!(
                                    "{} ❌ Erreur journalctl: <code>{}</code>",
                                    self.bot_name, e
                                )).await;
                            }
                        }
                    }

                    "/restart" => {
                        self.send(&format!(
                            "{} 🔄 <b>Redémarrage en cours...</b>\n\
                            ⏳ Le bot sera de retour dans ~5 secondes.",
                            self.bot_name
                        )).await;
                        // Le bot se suicide proprement — systemd (Restart=always) le relance.
                        // Aucun besoin de sudo ni de systemctl depuis le process.
                        std::process::exit(0);
                    }

                    "/competitors" | "/concurrents" => {
                        let reg = competitor_registry.read().await;
                        let msg = reg.format_for_telegram(&self.bot_name);
                        drop(reg);
                        self.send(&msg).await;
                    }

                    "/help" | "/start" => {
                        let msg = format!(
                            "{} 📖 <b>Commandes</b>\n\
                            \n\
                            /status — état du bot (up/down, uptime)\n\
                            /stats [id] — bilan P&amp;L (global ou par protocole)\n\
                            /hf [id] — positions à risque (filtre protocole optionnel)\n\
                            /competitors — bots concurrents identifiés (labels pb_01…)\n\
                            /protocols — liste des protocoles + statut\n\
                            /gas — solde ETH + estimation ops/jours restants\n\
                            /stats_json — télécharger stats.json\n\
                            /missed_json — télécharger missed.json\n\
                            /blind_spot [N] — derniers N blind spots (défaut 10)\n\
                            /blind_spot_json — télécharger untracked.json\n\
                            \n\
                            <b>Protocoles:</b>\n\
                            /enable &lt;id&gt; — activer un protocole\n\
                            /disable &lt;id&gt; — désactiver un protocole\n\
                            Exemples: <code>/stats aave_v3</code>  <code>/hf radiant_v2</code>\n\
                            \n\
                            <b>Contrôle bot:</b>\n\
                            /stop_bot — suspendre les liquidations\n\
                            /start_bot — reprendre les liquidations\n\
                            /restart — redémarrer le service systemd\n\
                            /logs [N] — dernières N lignes (défaut 30)\n\
                            \n\
                            <b>Contrôle contrat:</b>\n\
                            /pause_contract — mettre le contrat en pause (on-chain)\n\
                            /resume_contract — réactiver le contrat (on-chain)\n\
                            \n\
                            /help — cette aide",
                            self.bot_name,
                        );
                        self.send(&msg).await;
                    }

                    _ => {} // Ignore unknown
                }
            }
        }
    }

    /// Get updates from Telegram using long-polling
    /// One request hangs for up to 30s, returns instantly when a message arrives.
    /// Result: ~1 HTTP request per 30s idle, instant response when you type a command.
    async fn get_updates(&self, client: &reqwest::Client, offset: i64) -> Option<Vec<serde_json::Value>> {
        // POST avec JSON body — plus fiable que GET avec paramètres encodés à la main
        let url = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
        let body = serde_json::json!({
            "offset": offset,
            "timeout": 300,
            "allowed_updates": ["message"]
        });
        let resp = match client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Telegram getUpdates réseau: {e}");
                return None;
            }
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Telegram getUpdates parse: {e}");
                return None;
            }
        };
        if !json["ok"].as_bool().unwrap_or(false) {
            tracing::warn!("Telegram API erreur: {}", json["description"].as_str().unwrap_or("?"));
            return None;
        }
        json["result"].as_array().cloned()
    }

    /// Get the latest update_id to skip old messages on startup
    async fn get_latest_update_id(&self, client: &reqwest::Client) -> Option<i64> {
        let url = format!(
            "https://api.telegram.org/bot{}/getUpdates?offset=-1",
            self.token
        );
        let resp = client.get(&url).send().await.ok()?;
        let json: serde_json::Value = resp.json().await.ok()?;
        json["result"].as_array()
            .and_then(|arr| arr.last())
            .and_then(|u| u["update_id"].as_i64())
    }

    /// Send a file as a Telegram document, using the actual filename from the path.
    async fn send_document(&self, file_path: &str) {
        let path = std::path::Path::new(file_path);
        let filename = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file_path)
            .to_string();

        if !path.exists() {
            self.send(&format!("❌ <code>{filename}</code> introuvable (aucune donnée encore)")).await;
            return;
        }

        let file_bytes = match tokio::fs::read(file_path).await {
            Ok(b) => b,
            Err(e) => {
                self.send(&format!("❌ Erreur lecture <code>{filename}</code>: {e}")).await;
                return;
            }
        };

        let url = format!("https://api.telegram.org/bot{}/sendDocument", self.token);
        let caption = format!("📁 {filename}");
        let form = reqwest::multipart::Form::new()
            .text("chat_id", self.chat_id.clone())
            .text("caption", caption)
            .part("document", reqwest::multipart::Part::bytes(file_bytes)
                .file_name(filename)
                .mime_str("application/json")
                .unwrap()
            );

        let _ = self.client.post(&url).multipart(form).send().await;
    }

    /// Send raw bytes as a Telegram document (used for logs, etc.)
    async fn send_document_bytes(&self, bytes: &[u8], filename: &str, caption: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendDocument", self.token);
        let form = reqwest::multipart::Form::new()
            .text("chat_id", self.chat_id.clone())
            .text("caption", caption.to_string())
            .part("document", reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(filename.to_string())
                .mime_str("text/plain")
                .unwrap()
            );
        let _ = self.client.post(&url).multipart(form).send().await;
    }
}
