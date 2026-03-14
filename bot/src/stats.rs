//! Persistent stats tracker
//!
//! Stores every liquidation result in a JSON file on disk.
//! Survives bot restarts, VPS reboots, updates.
//! Computes totals, yearly, monthly, best/worst month on the fly.

use chrono::{Datelike, NaiveDate, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const STATS_FILE: &str = "stats.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiqRecord {
    pub timestamp: String,       // ISO 8601
    pub user: String,
    pub debt_token: String,
    pub collateral_token: String,
    pub debt_usd: f64,
    pub profit_usd: f64,        // net (after gas)
    pub gas_usd: f64,
    pub tx_hash: String,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsStore {
    pub started_at: String,      // first ever run
    pub records: Vec<LiqRecord>,
}

impl StatsStore {
    /// Load from disk or create fresh
    pub fn load() -> Self {
        let path = PathBuf::from(STATS_FILE);
        if path.exists() {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str::<StatsStore>(&data) {
                    return store;
                }
            }
        }
        StatsStore {
            started_at: Utc::now().format("%Y-%m-%d").to_string(),
            records: Vec::new(),
        }
    }

    /// Save to disk (call after each record)
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(STATS_FILE, json);
        }
    }

    /// Add a liquidation record and save
    pub fn record_liquidation(
        &mut self,
        user: &str,
        debt_token: &str,
        collateral_token: &str,
        debt_usd: f64,
        profit_usd: f64,
        gas_usd: f64,
        tx_hash: &str,
        success: bool,
    ) {
        self.records.push(LiqRecord {
            timestamp: Utc::now().to_rfc3339(),
            user: user.to_string(),
            debt_token: debt_token.to_string(),
            collateral_token: collateral_token.to_string(),
            debt_usd,
            profit_usd,
            gas_usd,
            tx_hash: tx_hash.to_string(),
            success,
        });
        self.save();
    }

    // ─────────────────────────────────────────────────────────
    // Aggregation
    // ─────────────────────────────────────────────────────────

    /// Total profit since inception (only successful)
    pub fn total_profit(&self) -> f64 {
        self.records.iter().filter(|r| r.success).map(|r| r.profit_usd).sum()
    }

    /// Total gas burned (all tx, including failures)
    pub fn total_gas(&self) -> f64 {
        self.records.iter().map(|r| r.gas_usd).sum()
    }

    /// Count of successful liquidations
    pub fn total_successes(&self) -> usize {
        self.records.iter().filter(|r| r.success).count()
    }

    /// Count of failed tx (gas lost)
    pub fn total_failures(&self) -> usize {
        self.records.iter().filter(|r| !r.success).count()
    }

    /// Aggregate profit by year → month
    /// Returns BTreeMap<year, BTreeMap<month, profit>>
    pub fn monthly_breakdown(&self) -> BTreeMap<i32, BTreeMap<u32, f64>> {
        let mut result: BTreeMap<i32, BTreeMap<u32, f64>> = BTreeMap::new();
        for r in self.records.iter().filter(|r| r.success) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&r.timestamp) {
                let year = dt.year();
                let month = dt.month();
                *result.entry(year).or_default().entry(month).or_default() += r.profit_usd;
            }
        }
        result
    }

    /// Format a complete stats summary for Telegram
    pub fn format_summary(&self) -> String {
        let total = self.total_profit();
        let gas = self.total_gas();
        let net = total - gas;
        let successes = self.total_successes();
        let failures = self.total_failures();

        let mut lines = Vec::new();

        lines.push(format!(
            "📊 <b>Bilan depuis le {}</b>",
            self.started_at,
        ));
        lines.push(format!(
            "💰 Profit total: <b>${:.2}</b>",
            net,
        ));
        lines.push(format!(
            "✅ {} réussies | ❌ {} échouées | ⛽ ${:.2} gas",
            successes, failures, gas,
        ));

        let breakdown = self.monthly_breakdown();
        let month_names = [
            "", "Jan", "Fév", "Mar", "Avr", "Mai", "Jun",
            "Jul", "Aoû", "Sep", "Oct", "Nov", "Déc",
        ];

        for (&year, months) in &breakdown {
            let year_total: f64 = months.values().sum();
            let months_active = months.len() as f64;
            let avg = if months_active > 0.0 { year_total / months_active } else { 0.0 };

            lines.push(String::new());
            lines.push(format!("📅 <b>{year}</b>: ${year_total:.2}"));
            lines.push(format!("　Moyenne/mois: ${avg:.2}"));

            // Best and worst month
            if let Some((&best_m, &best_v)) = months.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()) {
                lines.push(format!("　🏆 Meilleur: {} ${best_v:.2}", month_names[best_m as usize]));
            }
            if months.len() > 1 {
                if let Some((&worst_m, &worst_v)) = months.iter().min_by(|a, b| a.1.partial_cmp(b.1).unwrap()) {
                    lines.push(format!("　📉 Pire: {} ${worst_v:.2}", month_names[worst_m as usize]));
                }
            }

            // Month-by-month detail
            for (&m, &v) in months {
                let bar_len = (v / year_total * 20.0).max(1.0) as usize;
                let bar: String = "█".repeat(bar_len.min(20));
                lines.push(format!(
                    "　{} {} ${:.2}",
                    month_names[m as usize], bar, v,
                ));
            }
        }

        // Projection if we have enough data
        let days_active = {
            if let Ok(start) = NaiveDate::parse_from_str(&self.started_at, "%Y-%m-%d") {
                let today = Utc::now().date_naive();
                (today - start).num_days().max(1)
            } else {
                1
            }
        };

        if days_active > 7 {
            let daily_avg = net / days_active as f64;
            let monthly_proj = daily_avg * 30.0;
            let yearly_proj = daily_avg * 365.0;

            lines.push(String::new());
            lines.push(format!("📈 <b>Projection</b> ({days_active} jours de données)"));
            lines.push(format!("　/jour: ${daily_avg:.2}"));
            lines.push(format!("　/mois: ${monthly_proj:.2}"));
            lines.push(format!("　/an: ${yearly_proj:.2}"));
        }

        lines.join("\n")
    }
}
