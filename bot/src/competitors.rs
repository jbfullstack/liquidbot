//! Competitor bot tracker
//!
//! Assigns stable labels (pb_01, pb_02…) to competitor addresses that beat us
//! on liquidations. Persists to competitors.json between restarts.
//!
//! A competitor only gets a label after appearing at least once in a missed event.
//! Labels are assigned in order of first appearance and never change.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const COMPETITORS_FILE: &str = "competitors.json";
const MISSED_FILE:      &str = "missed.json";

// ─────────────────────────────────────────────────────────
// Structs
// ─────────────────────────────────────────────────────────

/// Everything we know about one competitor bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompetitorRecord {
    /// Stable human label: "pb_01", "pb_02"…
    pub label:        String,
    /// Full lowercase 0x address (alloy format)
    pub address:      String,
    /// ISO 8601 timestamp of first observed miss
    pub first_seen:   String,
    /// ISO 8601 timestamp of most recent miss
    pub last_seen:    String,
    /// Total liquidations where this bot beat us
    pub liq_count:    u32,
    /// Sum of our estimated profit on those positions (~5% bonus)
    pub total_profit: f64,
    /// Tokens this bot tends to target: Vec<(symbol, count)> sorted desc by count
    pub tokens:       Vec<(String, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompetitorStore {
    pub competitors: Vec<CompetitorRecord>,
}

// ─────────────────────────────────────────────────────────
// Core logic
// ─────────────────────────────────────────────────────────

impl CompetitorStore {
    /// Load from default location or start empty.
    pub fn load() -> Self {
        Self::load_from(COMPETITORS_FILE)
    }

    /// Load from a specific path (used in tests).
    pub fn load_from(path: &str) -> Self {
        let pb = PathBuf::from(path);
        if pb.exists() {
            if let Ok(data) = std::fs::read_to_string(&pb) {
                if let Ok(store) = serde_json::from_str::<CompetitorStore>(&data) {
                    return store;
                }
            }
        }
        CompetitorStore::default()
    }

    /// Atomic save (write .tmp then rename — no partial file on crash).
    pub fn save(&self) {
        self.save_to(COMPETITORS_FILE);
    }

    pub fn save_to(&self, path: &str) {
        let tmp = format!("{path}.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, &json) {
                    tracing::error!("competitors: failed to write {tmp}: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::error!("competitors: failed to rename {tmp} → {path}: {e}");
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(e) => tracing::error!("competitors: serialization failed: {e}"),
        }
    }

    /// Record one missed liquidation by `address`.
    /// Assigns a new label if this is the first time we see this address.
    /// Returns the label (e.g. "pb_01").
    pub fn record_miss(&mut self, address: &str, debt_sym: &str, est_profit: f64) -> String {
        let now = Utc::now().to_rfc3339();
        if let Some(rec) = self.competitors.iter_mut().find(|r| r.address == address) {
            rec.last_seen     = now;
            rec.liq_count    += 1;
            rec.total_profit += est_profit;
            upsert_token(&mut rec.tokens, debt_sym);
            rec.label.clone()
        } else {
            let idx   = self.competitors.len() + 1;
            let label = format!("pb_{idx:02}");
            self.competitors.push(CompetitorRecord {
                label:        label.clone(),
                address:      address.to_string(),
                first_seen:   now.clone(),
                last_seen:    now,
                liq_count:    1,
                total_profit: est_profit,
                tokens:       vec![(debt_sym.to_string(), 1)],
            });
            label
        }
    }

    /// Look up the label for a full address string, if known.
    #[allow(dead_code)]
    pub fn label_for(&self, address: &str) -> Option<&str> {
        self.competitors
            .iter()
            .find(|r| r.address == address)
            .map(|r| r.label.as_str())
    }

    /// Returns competitors sorted by total_profit descending.
    pub fn sorted_by_profit(&self) -> Vec<&CompetitorRecord> {
        let mut v: Vec<&CompetitorRecord> = self.competitors.iter().collect();
        v.sort_by(|a, b| b.total_profit.partial_cmp(&a.total_profit)
            .unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    /// Format the /competitors Telegram response (HTML).
    pub fn format_for_telegram(&self, bot_name: &str) -> String {
        if self.competitors.is_empty() {
            return format!(
                "{bot_name} ⚔️ <b>Concurrents connus</b>\n\nAucun concurrent identifié pour l'instant."
            );
        }

        let sorted = self.sorted_by_profit();
        let total_stolen: f64 = self.competitors.iter().map(|r| r.total_profit).sum();
        let total_liq: u32    = self.competitors.iter().map(|r| r.liq_count).sum();

        let mut lines = vec![
            format!(
                "{bot_name} ⚔️ <b>{} concurrent(s) identifié(s)</b>",
                sorted.len()
            ),
            format!(
                "💸 Total volé estimé: ~<b>${total_stolen:.2}</b> sur <b>{total_liq}</b> liquidations\n"
            ),
        ];

        for (i, rec) in sorted.iter().enumerate() {
            let first = &rec.first_seen[..rec.first_seen.len().min(10)]; // YYYY-MM-DD
            let last  = &rec.last_seen[..rec.last_seen.len().min(10)];

            // Top 3 tokens
            let top_tokens: String = rec.tokens.iter().take(3)
                .map(|(sym, n)| format!("{sym}({n})"))
                .collect::<Vec<_>>()
                .join(" ");

            lines.push(format!(
                "{}. <b>{}</b>  ~${:.2}  ·  {} liq",
                i + 1, rec.label, rec.total_profit, rec.liq_count,
            ));
            lines.push(format!("   <code>{}</code>", rec.address));
            if !top_tokens.is_empty() {
                lines.push(format!("   🎯 {top_tokens}"));
            }
            lines.push(format!("   📅 {first} → {last}"));
            if i + 1 < sorted.len() { lines.push(String::new()); }
        }

        lines.join("\n")
    }
}

// ─────────────────────────────────────────────────────────
// Missed liquidation log (missed.json)
// ─────────────────────────────────────────────────────────

/// One missed liquidation, as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissedRecord {
    pub timestamp:   String,   // ISO 8601
    pub user:        String,   // liquidated user address
    pub liquidator:  String,   // who liquidated (competitor or unknown bot)
    pub label:       String,   // pb_01, pb_02… (empty if unknown at save time)
    pub protocol:    String,   // "AV3" or "RDT"
    pub debt_sym:    String,
    pub debt_amt:    f64,
    pub col_sym:     String,
    pub last_hf:     f64,      // our last recorded HF (0.0 if never tracked)
    pub est_profit:  f64,      // ~5% of debt_amt
    pub tx_hash:     String,
    /// true = we never tracked this user (indexing blind spot, not a race loss)
    #[serde(default)]
    pub untracked:    bool,
    /// Block where the liquidation landed (0 if unavailable)
    #[serde(default)]
    pub block_number: u64,
}

/// Append-only log of every missed liquidation event.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MissedLog {
    pub records: Vec<MissedRecord>,
}

impl MissedLog {
    pub fn load() -> Self {
        Self::load_from(MISSED_FILE)
    }

    pub fn load_from(path: &str) -> Self {
        let pb = PathBuf::from(path);
        if pb.exists() {
            if let Ok(data) = std::fs::read_to_string(&pb) {
                if let Ok(log) = serde_json::from_str::<MissedLog>(&data) {
                    return log;
                }
            }
        }
        MissedLog::default()
    }

    /// Append events in memory only (no disk write). Used in tests.
    pub fn append_in_memory(
        &mut self,
        events: &[crate::MissedEvent],
        labels: &std::collections::HashMap<String, String>,
    ) {
        let now = Utc::now().to_rfc3339();
        for ev in events {
            let label = labels.get(&ev.liquidator).cloned().unwrap_or_default();
            self.records.push(MissedRecord {
                timestamp:  now.clone(),
                user:       ev.user.clone(),
                liquidator: ev.liquidator.clone(),
                label,
                protocol:   ev.protocol.clone(),
                debt_sym:   ev.debt_sym.clone(),
                debt_amt:   ev.debt_amt,
                col_sym:    ev.col_sym.clone(),
                last_hf:      ev.last_hf,
                est_profit:   ev.est_profit,
                tx_hash:      ev.tx_hash.clone(),
                untracked:    ev.untracked,
                block_number: ev.block_number,
            });
        }
    }

    /// Append all events from a batch and save atomically to disk.
    pub fn append_batch(
        &mut self,
        events: &[crate::MissedEvent],
        labels: &std::collections::HashMap<String, String>,
    ) {
        self.append_in_memory(events, labels);
        self.save_to(MISSED_FILE);
    }

    fn save_to(&self, path: &str) {
        let tmp = format!("{path}.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, &json) {
                    tracing::error!("missed: failed to write {tmp}: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::error!("missed: failed to rename {tmp} → {path}: {e}");
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(e) => tracing::error!("missed: serialization failed: {e}"),
        }
    }

    #[allow(dead_code)]
    pub fn total_est_profit(&self) -> f64 {
        self.records.iter().map(|r| r.est_profit).sum()
    }
}

// ─────────────────────────────────────────────────────────
// Untracked liquidation log (untracked.json)
// ─────────────────────────────────────────────────────────

const UNTRACKED_FILE: &str = "untracked.json";

/// Rich record for a liquidation where the user was never in our index.
/// Kept separate from missed.json to allow focused offline analysis.
///
/// To diagnose WHY this was a blind spot, look up Borrow events for `user`
/// on `protocol` before `block` — if the Borrow predates your 50k-block
/// indexing window that's the root cause.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UntrackedRecord {
    pub timestamp:   String,   // ISO 8601 — wall time of detection
    /// Block where the liquidation tx landed — query Borrow events before this
    pub block:       u64,
    pub protocol:    String,   // "AV3" | "RDT"
    pub user:        String,   // borrower (was never in our user_index)
    pub liquidator:  String,   // who executed the liquidation
    /// Full 0x token address — for offline Borrow event lookup
    pub debt_asset:  String,
    pub debt_sym:    String,
    pub debt_amt:    f64,
    /// Full 0x token address
    pub col_asset:   String,
    pub col_sym:     String,
    pub est_profit:  f64,      // ~5% liquidation bonus estimate
    pub tx_hash:     String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UntrackedLog {
    pub records: Vec<UntrackedRecord>,
}

impl UntrackedLog {
    pub fn load() -> Self {
        Self::load_from(UNTRACKED_FILE)
    }

    pub fn load_from(path: &str) -> Self {
        let pb = std::path::PathBuf::from(path);
        if pb.exists() {
            if let Ok(data) = std::fs::read_to_string(&pb) {
                if let Ok(log) = serde_json::from_str::<UntrackedLog>(&data) {
                    return log;
                }
            }
        }
        UntrackedLog::default()
    }

    /// Append all untracked events from a batch and save atomically.
    pub fn append_batch(&mut self, events: &[crate::MissedEvent]) {
        let now = Utc::now().to_rfc3339();
        for ev in events.iter().filter(|e| e.untracked) {
            self.records.push(UntrackedRecord {
                timestamp:  now.clone(),
                block:      ev.block_number,
                protocol:   ev.protocol.clone(),
                user:       ev.user.clone(),
                liquidator: ev.liquidator.clone(),
                debt_asset: ev.debt_asset.clone(),
                debt_sym:   ev.debt_sym.clone(),
                debt_amt:   ev.debt_amt,
                col_asset:  ev.col_asset.clone(),
                col_sym:    ev.col_sym.clone(),
                est_profit: ev.est_profit,
                tx_hash:    ev.tx_hash.clone(),
            });
        }
        if !events.iter().any(|e| e.untracked) { return; }
        let tmp = format!("{UNTRACKED_FILE}.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, &json) {
                    tracing::error!("untracked: failed to write {tmp}: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, UNTRACKED_FILE) {
                    tracing::error!("untracked: failed to rename {tmp} → {UNTRACKED_FILE}: {e}");
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(e) => tracing::error!("untracked: serialization failed: {e}"),
        }
    }
}

// ─────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────

/// Insert or increment a token entry, then re-sort descending by count.
fn upsert_token(tokens: &mut Vec<(String, u32)>, sym: &str) {
    if let Some(entry) = tokens.iter_mut().find(|(s, _)| s == sym) {
        entry.1 += 1;
    } else {
        tokens.push((sym.to_string(), 1));
    }
    tokens.sort_by(|a, b| b.1.cmp(&a.1));
}

// ─────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_new_competitor_gets_label_pb_01() {
        let mut s = CompetitorStore::default();
        let label = s.record_miss("0xabc", "USDC", 50.0);
        assert_eq!(label, "pb_01");
        assert_eq!(s.competitors.len(), 1);
    }

    #[test]
    fn test_second_competitor_gets_pb_02() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xaaa", "USDC", 10.0);
        let label = s.record_miss("0xbbb", "WETH", 20.0);
        assert_eq!(label, "pb_02");
    }

    #[test]
    fn test_same_address_increments_count() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xabc", "USDC", 50.0);
        s.record_miss("0xabc", "USDT", 30.0);
        let rec = &s.competitors[0];
        assert_eq!(rec.liq_count, 2);
        assert!((rec.total_profit - 80.0).abs() < 1e-9);
    }

    #[test]
    fn test_same_address_keeps_same_label() {
        let mut s = CompetitorStore::default();
        let l1 = s.record_miss("0xabc", "USDC", 10.0);
        let l2 = s.record_miss("0xabc", "USDC", 10.0);
        assert_eq!(l1, l2);
        assert_eq!(l1, "pb_01");
    }

    #[test]
    fn test_label_for_returns_correct() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xaaa", "USDC", 10.0);
        s.record_miss("0xbbb", "USDC", 20.0);
        assert_eq!(s.label_for("0xaaa"), Some("pb_01"));
        assert_eq!(s.label_for("0xbbb"), Some("pb_02"));
        assert_eq!(s.label_for("0xzzz"), None);
    }

    #[test]
    fn test_sorted_by_profit_descending() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xaaa", "USDC", 10.0);
        s.record_miss("0xbbb", "USDC", 50.0);
        s.record_miss("0xccc", "USDC", 30.0);
        let sorted = s.sorted_by_profit();
        assert_eq!(sorted[0].address, "0xbbb");
        assert_eq!(sorted[1].address, "0xccc");
        assert_eq!(sorted[2].address, "0xaaa");
    }

    #[test]
    fn test_tokens_sorted_desc_by_count() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xaaa", "WETH", 10.0);
        s.record_miss("0xaaa", "USDC", 10.0);
        s.record_miss("0xaaa", "USDC", 10.0);
        let tokens = &s.competitors[0].tokens;
        assert_eq!(tokens[0].0, "USDC");
        assert_eq!(tokens[0].1, 2);
        assert_eq!(tokens[1].0, "WETH");
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("competitors.json").to_str().unwrap().to_string();

        let mut s = CompetitorStore::default();
        s.record_miss("0xabc", "USDC", 42.0);
        s.save_to(&path);

        let loaded = CompetitorStore::load_from(&path);
        assert_eq!(loaded.competitors.len(), 1);
        assert_eq!(loaded.competitors[0].label, "pb_01");
        assert!((loaded.competitors[0].total_profit - 42.0).abs() < 1e-9);
    }

    #[test]
    fn test_load_nonexistent_returns_empty() {
        let s = CompetitorStore::load_from("/tmp/does_not_exist_competitors_xyz.json");
        assert!(s.competitors.is_empty());
    }

    #[test]
    fn test_save_atomic_no_tmp_leftover() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("competitors.json").to_str().unwrap().to_string();
        let tmp  = format!("{path}.tmp");
        let mut s = CompetitorStore::default();
        s.record_miss("0xabc", "USDC", 10.0);
        s.save_to(&path);
        assert!(std::path::Path::new(&path).exists());
        assert!(!std::path::Path::new(&tmp).exists());
    }

    #[test]
    fn test_format_empty() {
        let s = CompetitorStore::default();
        let out = s.format_for_telegram("⚡ LiqBot");
        assert!(out.contains("Aucun concurrent"));
    }

    #[test]
    fn test_format_contains_label_and_address() {
        let mut s = CompetitorStore::default();
        s.record_miss("0xdeadbeef", "USDC", 100.0);
        let out = s.format_for_telegram("⚡ LiqBot");
        assert!(out.contains("pb_01"));
        assert!(out.contains("0xdeadbeef"));
        assert!(out.contains("100"));
    }

    // ── MissedLog ──

    fn make_missed(liquidator: &str) -> crate::MissedEvent {
        crate::MissedEvent {
            user:         "0xuser".to_string(),
            liquidator:   liquidator.to_string(),
            debt_sym:     "USDC".to_string(),
            debt_amt:     1000.0,
            col_sym:      "WETH".to_string(),
            protocol:     "AV3".to_string(),
            tx_hash:      "0xtx".to_string(),
            last_hf:      0.99,
            est_profit:   50.0,
            untracked:    false,
            block_number: 0,
            debt_asset:   "0xUSDC".to_string(),
            col_asset:    "0xWETH".to_string(),
        }
    }

    #[test]
    fn test_missed_log_append_in_memory() {
        // Tests in-memory state only — does NOT write to disk.
        // append_batch writes to MISSED_FILE (relative path) which would pollute
        // the working directory during `cargo test`. Use append_in_memory instead.
        let events = vec![make_missed("0xaaa"), make_missed("0xbbb")];
        let labels: std::collections::HashMap<String, String> = [
            ("0xaaa".to_string(), "pb_01".to_string()),
        ].into();

        let mut log = MissedLog::default();
        log.append_in_memory(&events, &labels);

        assert_eq!(log.records.len(), 2);
        assert_eq!(log.records[0].label, "pb_01");
        assert_eq!(log.records[1].label, ""); // 0xbbb not in labels
        assert_eq!(log.records[0].debt_sym, "USDC");
    }

    #[test]
    fn test_missed_log_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missed.json").to_str().unwrap().to_string();

        let events = vec![make_missed("0xaaa")];
        let labels: std::collections::HashMap<String, String> = [
            ("0xaaa".to_string(), "pb_01".to_string()),
        ].into();

        let mut log = MissedLog::default();
        log.append_in_memory(&events, &labels);
        log.save_to(&path);

        let loaded = MissedLog::load_from(&path);
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].label, "pb_01");
    }

    #[test]
    fn test_missed_log_total_profit() {
        let events = vec![make_missed("0xaaa"), make_missed("0xbbb")];
        let labels = std::collections::HashMap::new();
        let mut log = MissedLog::default();
        log.append_batch(&events, &labels);
        assert!((log.total_est_profit() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_missed_log_load_nonexistent_returns_empty() {
        let log = MissedLog::load_from("/tmp/does_not_exist_missed_xyz.json");
        assert!(log.records.is_empty());
    }
}
