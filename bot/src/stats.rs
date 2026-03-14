//! Persistent stats tracker
//!
//! Stores every liquidation result in a JSON file on disk.
//! Survives bot restarts, VPS reboots, updates.
//! Computes totals, yearly, monthly, best/worst month on the fly.
//!
//! NOTE: profit_usd stored in each record is GROSS (before gas).
//!       net = total_profit() - total_gas()

use chrono::{Datelike, NaiveDate, Utc};
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
    pub profit_usd: f64,        // GROSS profit (before gas) for successes, 0 for failures
    pub gas_usd: f64,           // gas cost (always positive)
    pub tx_hash: String,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsStore {
    pub started_at: String,      // first ever run
    pub records: Vec<LiqRecord>,
}

impl StatsStore {
    /// Load from default disk location or create fresh
    pub fn load() -> Self {
        Self::load_from(STATS_FILE)
    }

    /// Load from a specific path (used in tests)
    pub fn load_from(path: &str) -> Self {
        let pb = PathBuf::from(path);
        if pb.exists() {
            if let Ok(data) = std::fs::read_to_string(&pb) {
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

    /// Save to default disk location (atomic write — no partial file on crash)
    pub fn save(&self) {
        self.save_to(STATS_FILE);
    }

    /// Save to a specific path (atomic: write .tmp then rename)
    pub fn save_to(&self, path: &str) {
        let tmp = format!("{path}.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, &json) {
                    tracing::error!("stats: failed to write tmp file {tmp}: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::error!("stats: failed to rename {tmp} → {path}: {e}");
                    let _ = std::fs::remove_file(&tmp); // clean up orphan
                }
            }
            Err(e) => tracing::error!("stats: serialization failed: {e}"),
        }
    }

    /// Add a liquidation record and save.
    /// profit_usd = GROSS profit (before gas) for success, 0.0 for failure.
    pub fn record_liquidation(
        &mut self,
        user: &str,
        debt_token: &str,
        collateral_token: &str,
        debt_usd: f64,
        profit_usd: f64,  // gross — caller must NOT subtract gas_usd here
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

    /// Total GROSS profit from successful liquidations
    pub fn total_profit(&self) -> f64 {
        self.records.iter().filter(|r| r.success).map(|r| r.profit_usd).sum()
    }

    /// Total gas burned (all tx, including failures)
    /// Net = total_profit() - total_gas()
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
        let gross = self.total_profit();
        let gas   = self.total_gas();
        let net   = gross - gas;
        let successes = self.total_successes();
        let failures  = self.total_failures();

        let mut lines = Vec::new();

        lines.push(format!(
            "📊 <b>Bilan depuis le {}</b>",
            self.started_at,
        ));
        lines.push(format!(
            "💰 Profit net: <b>${:.2}</b> (brut ${:.2} - gas ${:.2})",
            net, gross, gas,
        ));
        lines.push(format!(
            "✅ {} réussies | ❌ {} échouées",
            successes, failures,
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

            if let Some((&best_m, &best_v)) = months.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()) {
                lines.push(format!("　🏆 Meilleur: {} ${best_v:.2}", month_names[best_m as usize]));
            }
            if months.len() > 1 {
                if let Some((&worst_m, &worst_v)) = months.iter().min_by(|a, b| a.1.partial_cmp(b.1).unwrap()) {
                    lines.push(format!("　📉 Pire: {} ${worst_v:.2}", month_names[worst_m as usize]));
                }
            }

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_record(success: bool, profit: f64, gas: f64) -> LiqRecord {
        LiqRecord {
            timestamp: "2025-01-15T10:00:00+00:00".to_string(),
            user: "0xuser".to_string(),
            debt_token: "USDC".to_string(),
            collateral_token: "WETH".to_string(),
            debt_usd: 1000.0,
            profit_usd: profit,
            gas_usd: gas,
            tx_hash: "0xhash".to_string(),
            success,
        }
    }

    fn store_with(records: Vec<LiqRecord>) -> StatsStore {
        StatsStore {
            started_at: "2025-01-01".to_string(),
            records,
        }
    }

    // ── Aggregation ──

    #[test]
    fn test_total_profit_only_successes() {
        let s = store_with(vec![
            make_record(true,  100.0, 1.0),
            make_record(false,   0.0, 0.5), // failure — profit ignored
            make_record(true,   50.0, 0.8),
        ]);
        assert_eq!(s.total_profit(), 150.0);
    }

    #[test]
    fn test_total_gas_all_records() {
        let s = store_with(vec![
            make_record(true,  100.0, 1.0),
            make_record(false,   0.0, 0.5),
            make_record(true,   50.0, 0.8),
        ]);
        assert!((s.total_gas() - 2.3).abs() < 1e-9);
    }

    #[test]
    fn test_net_profit_correct() {
        // net = gross_success_profit - all_gas
        let s = store_with(vec![
            make_record(true,  100.0, 1.0),  // success: gross $100, gas $1 → net $99
            make_record(false,   0.0, 0.5),  // failure: gas lost $0.50
        ]);
        // net = 100.0 - (1.0 + 0.5) = 98.5
        let net = s.total_profit() - s.total_gas();
        assert!((net - 98.5).abs() < 1e-9);
    }

    #[test]
    fn test_counts() {
        let s = store_with(vec![
            make_record(true, 10.0, 1.0),
            make_record(true, 20.0, 1.0),
            make_record(false, 0.0, 0.5),
        ]);
        assert_eq!(s.total_successes(), 2);
        assert_eq!(s.total_failures(), 1);
    }

    #[test]
    fn test_empty_store() {
        let s = store_with(vec![]);
        assert_eq!(s.total_profit(), 0.0);
        assert_eq!(s.total_gas(), 0.0);
        assert_eq!(s.total_successes(), 0);
        assert_eq!(s.total_failures(), 0);
    }

    #[test]
    fn test_monthly_breakdown_groups_correctly() {
        let mut s = store_with(vec![]);
        // Two successes in Jan 2025, one in Feb 2025
        let mut r1 = make_record(true, 100.0, 1.0);
        r1.timestamp = "2025-01-10T10:00:00+00:00".to_string();
        let mut r2 = make_record(true,  50.0, 0.5);
        r2.timestamp = "2025-01-20T10:00:00+00:00".to_string();
        let mut r3 = make_record(true,  30.0, 0.3);
        r3.timestamp = "2025-02-05T10:00:00+00:00".to_string();
        let mut r4 = make_record(false, 0.0, 0.2); // failure — not in breakdown
        r4.timestamp = "2025-01-15T10:00:00+00:00".to_string();
        s.records = vec![r1, r2, r3, r4];

        let breakdown = s.monthly_breakdown();
        assert_eq!(breakdown[&2025][&1], 150.0);
        assert_eq!(breakdown[&2025][&2],  30.0);
    }

    // ── Save / Load round-trip ──

    #[test]
    fn test_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_stats.json").to_str().unwrap().to_string();

        let original = store_with(vec![make_record(true, 42.5, 0.5)]);
        original.save_to(&path);

        let loaded = StatsStore::load_from(&path);
        assert_eq!(loaded.started_at, original.started_at);
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].profit_usd, 42.5);
        assert!(loaded.records[0].success);
    }

    #[test]
    fn test_load_from_nonexistent() {
        let s = StatsStore::load_from("/tmp/does_not_exist_xyz_12345.json");
        assert!(s.records.is_empty());
        assert!(!s.started_at.is_empty());
    }

    #[test]
    fn test_save_is_atomic_no_partial_file() {
        // If tmp write succeeds and rename succeeds, the original file is always complete.
        // Verify no .tmp file left behind after a successful save.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stats.json").to_str().unwrap().to_string();
        let tmp  = format!("{path}.tmp");

        let s = store_with(vec![make_record(true, 10.0, 0.1)]);
        s.save_to(&path);

        assert!(std::path::Path::new(&path).exists(), "stats file must exist");
        assert!(!std::path::Path::new(&tmp).exists(), ".tmp file must be cleaned up");
    }

    #[test]
    fn test_save_overwrites_previous() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stats.json").to_str().unwrap().to_string();

        let s1 = store_with(vec![make_record(true, 10.0, 1.0)]);
        s1.save_to(&path);

        let mut s2 = StatsStore::load_from(&path);
        s2.records.push(make_record(true, 20.0, 0.5));
        s2.save_to(&path);

        let loaded = StatsStore::load_from(&path);
        assert_eq!(loaded.records.len(), 2);
    }

    // ── format_summary smoke test ──

    #[test]
    fn test_format_summary_no_panic_on_empty() {
        let s = store_with(vec![]);
        let summary = s.format_summary();
        assert!(summary.contains("Bilan"));
    }

    #[test]
    fn test_format_summary_contains_net_profit() {
        let s = store_with(vec![
            make_record(true, 100.0, 2.0),
            make_record(false, 0.0, 0.5),
        ]);
        let summary = s.format_summary();
        // net = 100 - 2.5 = 97.50
        assert!(summary.contains("97.50"), "summary: {summary}");
    }
}
