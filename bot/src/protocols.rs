//! Protocol registry — available lending protocols for liquidation.
//!
//! Tracks which protocols are implemented, which are enabled at runtime,
//! and provides metadata for /protocols Telegram command.
//!
//! Architecture:
//!   ALL_PROTOCOLS — static compile-time metadata for all known protocols
//!   ProtocolRegistry — runtime enable/disable state, persisted to protocols.json

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ─────────────────────────────────────────────────────────
// Static metadata
// ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImplStatus {
    /// Fully implemented — liquidations can run on this protocol
    Active,
    /// Metadata only — Solidity integration not yet deployed
    Planned,
}

#[derive(Debug, Clone)]
pub struct ProtocolMeta {
    pub id:          &'static str,  // machine ID: "aave_v3"
    pub name:        &'static str,  // display name: "Aave V3" (stored in LiqRecord.protocol)
    pub tvl_hint:    &'static str,
    pub competition: &'static str,
    pub bonus_pct:   &'static str,
    pub impl_status: ImplStatus,
}

/// All known protocols, in priority order (best opportunity first after Aave V3).
pub const ALL_PROTOCOLS: &[ProtocolMeta] = &[
    ProtocolMeta {
        id:          "aave_v3",
        name:        "Aave V3",
        tvl_hint:    "~$2B",
        competition: "Haute",
        bonus_pct:   "5-15%",
        impl_status: ImplStatus::Active,
    },
    ProtocolMeta {
        id:          "radiant_v2",
        name:        "Radiant V2",
        tvl_hint:    "~$80M",
        competition: "Faible",   // peu de bots, fork Aave V2 quasi-identique
        bonus_pct:   "5-15%",
        impl_status: ImplStatus::Active,
    },
    ProtocolMeta {
        id:          "compound_v3",
        name:        "Compound V3",
        tvl_hint:    "~$50M",
        competition: "Moyenne",
        bonus_pct:   "~5%",
        impl_status: ImplStatus::Planned,
    },
    ProtocolMeta {
        id:          "silo",
        name:        "Silo Finance",
        tvl_hint:    "~$20M",
        competition: "Très faible",
        bonus_pct:   "5-10%",
        impl_status: ImplStatus::Planned,
    },
];

// ─────────────────────────────────────────────────────────
// Persistence
// ─────────────────────────────────────────────────────────

const REGISTRY_FILE: &str = "protocols.json";

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    enabled: Vec<String>,
}

// ─────────────────────────────────────────────────────────
// Registry
// ─────────────────────────────────────────────────────────

pub struct ProtocolRegistry {
    enabled: HashSet<String>,
}

impl ProtocolRegistry {
    /// Load from default path. Falls back to `defaults` if no file exists.
    pub fn load(defaults: &[&str]) -> Self {
        Self::load_from(REGISTRY_FILE, defaults)
    }

    /// Load from a specific path (used in tests / migrations).
    pub fn load_from(path: &str, defaults: &[&str]) -> Self {
        let state: Option<PersistedState> = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        let enabled = state
            .map(|s| s.enabled.into_iter().collect())
            .unwrap_or_else(|| defaults.iter().map(|s| s.to_string()).collect());
        Self { enabled }
    }

    /// Atomically persist state (write tmp then rename).
    pub fn save(&self) {
        let state = PersistedState {
            enabled: self.enabled.iter().cloned().collect(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let tmp = format!("{REGISTRY_FILE}.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, REGISTRY_FILE);
            }
        }
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    /// Enable a protocol by machine ID.
    /// Returns the display name on success, user-facing error on failure.
    pub fn enable(&mut self, id: &str) -> Result<&'static str, &'static str> {
        let meta = ALL_PROTOCOLS.iter().find(|p| p.id == id)
            .ok_or("Protocole inconnu. Voir /protocols pour la liste.")?;
        if meta.impl_status == ImplStatus::Planned {
            return Err("Pas encore implémenté — intégration contrat à venir.");
        }
        self.enabled.insert(id.to_string());
        self.save();
        Ok(meta.name)
    }

    /// Disable a protocol by machine ID.
    /// Returns the display name on success, user-facing error on failure.
    pub fn disable(&mut self, id: &str) -> Result<&'static str, &'static str> {
        if !ALL_PROTOCOLS.iter().any(|p| p.id == id) {
            return Err("Protocole inconnu. Voir /protocols pour la liste.");
        }
        // Refuse to disable the last active protocol
        let active_enabled_count = self.enabled.iter()
            .filter(|eid| {
                ALL_PROTOCOLS.iter()
                    .any(|p| p.id == eid.as_str() && p.impl_status == ImplStatus::Active)
            })
            .count();
        let target_is_active = ALL_PROTOCOLS.iter()
            .any(|p| p.id == id && p.impl_status == ImplStatus::Active);
        if active_enabled_count <= 1 && target_is_active && self.enabled.contains(id) {
            return Err("Impossible — c'est le seul protocole actif. Active d'abord un autre.");
        }
        // id is known (validated by the `any()` check above), so unwrap is safe
        let name = ALL_PROTOCOLS.iter().find(|p| p.id == id).unwrap().name;
        self.enabled.remove(id);
        self.save();
        Ok(name)
    }

    /// Format the /protocols Telegram response.
    ///
    /// `live`: per-protocol live stats keyed by protocol ID.
    ///   value = (users_count, at_risk_count, net_profit_usd)
    pub fn format_for_telegram(
        &self,
        live: &std::collections::HashMap<&'static str, (usize, u32, f64)>,
    ) -> String {
        let mut lines = vec!["📋 <b>Protocoles de liquidation</b>\n".to_string()];

        for meta in ALL_PROTOCOLS {
            let is_enabled = self.is_enabled(meta.id);
            let (icon, status) = match (&meta.impl_status, is_enabled) {
                (ImplStatus::Active, true)  => ("✅", "ACTIF"),
                (ImplStatus::Active, false) => ("⏸", "INACTIF"),
                (ImplStatus::Planned, _)    => ("🔧", "PLANIFIÉ"),
            };

            lines.push(format!(
                "{icon} <b>{}</b>  <code>{}</code>  [{}]",
                meta.name, meta.id, status
            ));
            lines.push(format!(
                "   TVL: {}  |  Concurrence: {}  |  Bonus: {}",
                meta.tvl_hint, meta.competition, meta.bonus_pct
            ));

            if let Some(&(users, risk, profit)) = live.get(meta.id) {
                lines.push(format!(
                    "   👥 {users} users  ⚠️ {risk} at-risk  💰 ${profit:.2} net"
                ));
            }

            if meta.impl_status == ImplStatus::Planned {
                lines.push("   <i>⏳ Intégration contrat à venir</i>".to_string());
            }

            lines.push(String::new()); // blank line between protocols
        }

        lines.push("Pour activer/désactiver :".to_string());
        lines.push("<code>/enable &lt;id&gt;</code>  ou  <code>/disable &lt;id&gt;</code>".to_string());

        lines.join("\n")
    }
}

// ─────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a fresh registry without touching the filesystem.
    fn reg() -> ProtocolRegistry {
        ProtocolRegistry::load_from("/nonexistent/test_protocols_12345.json", &["aave_v3"])
    }

    #[test]
    fn test_all_protocols_unique_ids() {
        let ids: Vec<_> = ALL_PROTOCOLS.iter().map(|p| p.id).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "Duplicate protocol IDs detected");
    }

    #[test]
    fn test_aave_v3_is_active() {
        let meta = ALL_PROTOCOLS.iter().find(|p| p.id == "aave_v3").unwrap();
        assert_eq!(meta.impl_status, ImplStatus::Active);
    }

    #[test]
    fn test_planned_protocols_not_enableable() {
        let mut r = reg();
        // radiant_v2 is now Active — only compound_v3 and silo remain Planned
        assert!(r.enable("compound_v3").is_err());
        assert!(r.enable("silo").is_err());
    }

    #[test]
    fn test_unknown_id_returns_err() {
        let mut r = reg();
        assert!(r.enable("unknown_xyz").is_err());
        assert!(r.disable("unknown_xyz").is_err());
    }

    #[test]
    fn test_disable_last_active_returns_err() {
        // With radiant_v2 also Active, we need both enabled to test the guard.
        // Start with only aave_v3 enabled (default) — aave_v3 is the sole active+enabled.
        let mut r = reg();
        assert!(r.disable("aave_v3").is_err(),
            "Disabling the only active+enabled protocol must fail");

        // Now enable radiant_v2 as well — aave_v3 is no longer the sole active protocol.
        // Disabling aave_v3 should now SUCCEED since radiant_v2 is still active+enabled.
        let mut r2 = reg();
        r2.enable("radiant_v2").expect("radiant_v2 is Active — enable must succeed");
        assert!(r2.disable("aave_v3").is_ok(),
            "Disabling aave_v3 when radiant_v2 is also active+enabled must succeed");
    }

    #[test]
    fn test_aave_v3_enabled_by_default() {
        let r = reg();
        assert!(r.is_enabled("aave_v3"));
        assert!(!r.is_enabled("radiant_v2"));
        assert!(!r.is_enabled("compound_v3"));
        assert!(!r.is_enabled("silo"));
    }

    #[test]
    fn test_enable_returns_display_name() {
        // radiant_v2 is now Active — enabling it must return its display name.
        let mut r = reg();
        let name = r.enable("radiant_v2").expect("radiant_v2 is Active — enable must succeed");
        assert_eq!(name, "Radiant V2", "enable() must return the display name");

        // Planned protocols still return an error.
        let err = r.enable("compound_v3").unwrap_err();
        assert!(!err.is_empty(), "Error message must not be empty");
    }

    #[test]
    fn test_disable_planned_is_ok_even_if_not_enabled() {
        // Disabling a Planned protocol that was never enabled should not error
        // (it's a no-op but valid — no "last active" guard applies because Planned != Active)
        let mut r = reg();
        let result = r.disable("compound_v3");
        assert!(result.is_ok(), "Disabling a known but never-enabled planned protocol must succeed");
    }

    #[test]
    fn test_radiant_v2_can_be_enabled() {
        let mut r = reg();
        let name = r.enable("radiant_v2").expect("radiant_v2 is Active — it must be enableable");
        assert_eq!(name, "Radiant V2", "enable() must return display name");
        assert!(r.is_enabled("radiant_v2"), "radiant_v2 must be in enabled set after enable()");
    }

    #[test]
    fn test_format_contains_all_protocols() {
        let r = reg();
        let live = std::collections::HashMap::new();
        let out = r.format_for_telegram(&live);
        for meta in ALL_PROTOCOLS {
            assert!(out.contains(meta.name), "Missing protocol name: {}", meta.name);
            assert!(out.contains(meta.id),   "Missing protocol id: {}", meta.id);
        }
    }

    #[test]
    fn test_format_shows_actif_for_enabled() {
        let r = reg();
        let live = std::collections::HashMap::new();
        let out = r.format_for_telegram(&live);
        assert!(out.contains("ACTIF"),    "Enabled protocol must show ACTIF");
        assert!(out.contains("PLANIFIÉ"), "Planned protocol must show PLANIFIÉ");
    }

    #[test]
    fn test_format_shows_live_stats_when_provided() {
        let r = reg();
        let mut live = std::collections::HashMap::new();
        live.insert("aave_v3", (1247usize, 3u32, 1842.5f64));
        let out = r.format_for_telegram(&live);
        assert!(out.contains("1247"), "User count must appear in output");
        assert!(out.contains("1842.50"), "Net profit must appear in output");
    }
}
