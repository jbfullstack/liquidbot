use eyre::{Result, eyre};

#[derive(Debug, Clone)]
pub struct Config {
    pub rpc_ws_url: String,
    pub rpc_http_url: String,
    pub private_key: String,
    pub cold_wallet: String,
    pub contract_address: String,
    pub min_profit_usd: f64,
    pub max_gas_gwei: f64,
    pub eth_keep: f64,
    pub health_factor_threshold: f64,  // e.g. 1.05 — start watching
    pub telegram_token: String,
    pub telegram_chat_id: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cfg = Self {
            rpc_ws_url: std::env::var("ARBITRUM_WS_URL")
                .map_err(|_| eyre!("ARBITRUM_WS_URL missing"))?,
            rpc_http_url: std::env::var("ARBITRUM_RPC_URL")
                .map_err(|_| eyre!("ARBITRUM_RPC_URL missing"))?,
            private_key: std::env::var("PRIVATE_KEY")
                .map_err(|_| eyre!("PRIVATE_KEY missing"))?,
            cold_wallet: std::env::var("COLD_WALLET")
                .map_err(|_| eyre!("COLD_WALLET missing"))?,
            contract_address: std::env::var("CONTRACT_ADDRESS")
                .map_err(|_| eyre!("CONTRACT_ADDRESS missing"))?,
            min_profit_usd: std::env::var("MIN_PROFIT_USD")
                .unwrap_or("2".into()).parse()
                .map_err(|_| eyre!("MIN_PROFIT_USD must be a number"))?,
            max_gas_gwei: std::env::var("MAX_GAS_GWEI")
                .unwrap_or("1".into()).parse()
                .map_err(|_| eyre!("MAX_GAS_GWEI must be a number"))?,
            eth_keep: std::env::var("ETH_KEEP")
                .unwrap_or("0.01".into()).parse()
                .map_err(|_| eyre!("ETH_KEEP must be a number"))?,
            health_factor_threshold: std::env::var("HF_THRESHOLD")
                .unwrap_or("1.05".into()).parse()
                .map_err(|_| eyre!("HF_THRESHOLD must be a number"))?,
            telegram_token: std::env::var("TELEGRAM_BOT_TOKEN")
                .unwrap_or_default(),
            telegram_chat_id: std::env::var("TELEGRAM_CHAT_ID")
                .unwrap_or_default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.min_profit_usd < 0.0 {
            return Err(eyre!("MIN_PROFIT_USD must be >= 0, got {}", self.min_profit_usd));
        }
        if self.max_gas_gwei <= 0.0 {
            return Err(eyre!("MAX_GAS_GWEI must be > 0, got {}", self.max_gas_gwei));
        }
        if self.eth_keep < 0.0 {
            return Err(eyre!("ETH_KEEP must be >= 0, got {}", self.eth_keep));
        }
        if self.health_factor_threshold < 1.0 {
            return Err(eyre!(
                "HF_THRESHOLD must be >= 1.0 (e.g. 1.05), got {}. \
                 Setting it below 1.0 would miss liquidatable positions.",
                self.health_factor_threshold
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn set_required() {
        std::env::set_var("ARBITRUM_WS_URL",    "wss://example.com");
        std::env::set_var("ARBITRUM_RPC_URL",   "https://example.com");
        // Known valid dev private key (from foundry test suite — never use in production)
        std::env::set_var("PRIVATE_KEY",        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
        std::env::set_var("COLD_WALLET",        "0x0000000000000000000000000000000000000001");
        std::env::set_var("CONTRACT_ADDRESS",   "0x0000000000000000000000000000000000000002");
    }

    fn clear_optional() {
        for key in &["MIN_PROFIT_USD", "MAX_GAS_GWEI", "ETH_KEEP", "HF_THRESHOLD",
                     "TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID"] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn test_defaults() {
        set_required();
        clear_optional();
        let cfg = Config::from_env().expect("valid config");
        assert_eq!(cfg.min_profit_usd, 2.0);
        assert_eq!(cfg.max_gas_gwei, 1.0);
        assert_eq!(cfg.eth_keep, 0.01);
        assert_eq!(cfg.health_factor_threshold, 1.05);
        assert!(cfg.telegram_token.is_empty());
    }

    #[test]
    #[serial]
    fn test_custom_values() {
        set_required();
        std::env::set_var("MIN_PROFIT_USD", "5");
        std::env::set_var("MAX_GAS_GWEI", "2.5");
        std::env::set_var("ETH_KEEP", "0.05");
        std::env::set_var("HF_THRESHOLD", "1.10");
        let cfg = Config::from_env().expect("valid config");
        assert_eq!(cfg.min_profit_usd, 5.0);
        assert_eq!(cfg.max_gas_gwei, 2.5);
        assert_eq!(cfg.eth_keep, 0.05);
        assert_eq!(cfg.health_factor_threshold, 1.10);
        clear_optional();
    }

    #[test]
    #[serial]
    fn test_missing_required_ws() {
        set_required();
        clear_optional();
        std::env::remove_var("ARBITRUM_WS_URL");
        assert!(Config::from_env().is_err());
    }

    #[test]
    #[serial]
    fn test_missing_required_private_key() {
        set_required();
        clear_optional();
        std::env::remove_var("PRIVATE_KEY");
        assert!(Config::from_env().is_err());
    }

    #[test]
    #[serial]
    fn test_missing_required_contract() {
        set_required();
        clear_optional();
        std::env::remove_var("CONTRACT_ADDRESS");
        assert!(Config::from_env().is_err());
    }

    #[test]
    #[serial]
    fn test_validation_negative_profit() {
        set_required();
        std::env::set_var("MIN_PROFIT_USD", "-1");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("MIN_PROFIT_USD"));
        clear_optional();
    }

    #[test]
    #[serial]
    fn test_validation_zero_gas() {
        set_required();
        std::env::set_var("MAX_GAS_GWEI", "0");
        assert!(Config::from_env().is_err());
        clear_optional();
    }

    #[test]
    #[serial]
    fn test_validation_hf_below_1() {
        set_required();
        std::env::set_var("HF_THRESHOLD", "0.95");
        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HF_THRESHOLD"));
        clear_optional();
    }

    #[test]
    #[serial]
    fn test_non_numeric_value() {
        set_required();
        std::env::set_var("MIN_PROFIT_USD", "abc");
        assert!(Config::from_env().is_err());
        clear_optional();
    }
}
