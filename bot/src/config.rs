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
    pub health_factor_threshold: f64,  // e.g. 1.02 — start watching
    pub telegram_token: String,
    pub telegram_chat_id: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
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
                .unwrap_or("2".into()).parse()?,
            max_gas_gwei: std::env::var("MAX_GAS_GWEI")
                .unwrap_or("1".into()).parse()?,
            eth_keep: std::env::var("ETH_KEEP")
                .unwrap_or("0.01".into()).parse()?,
            health_factor_threshold: std::env::var("HF_THRESHOLD")
                .unwrap_or("1.05".into()).parse()?,
            telegram_token: std::env::var("TELEGRAM_BOT_TOKEN")
                .unwrap_or_default(),
            telegram_chat_id: std::env::var("TELEGRAM_CHAT_ID")
                .unwrap_or_default(),
        })
    }

    pub fn hot_wallet_address(&self) -> String {
        // Derive from private key — placeholder
        format!("0x...derived_from_pk")
    }
}
