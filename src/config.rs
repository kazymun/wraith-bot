use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Config {
    pub telegram_token: String,
    pub rpc_url: String,
    pub master_key_b64: String,
    pub db_path: String,
    pub default_slippage_bps: u32,
    pub fee_wallet: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let telegram_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN is not set")?;
        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .context("SOLANA_RPC_URL is not set")?;
        let master_key_b64 = std::env::var("WRAITH_MASTER_KEY")
            .context("WRAITH_MASTER_KEY is not set. Generate one with `openssl rand -base64 32`")?;
        let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "./wraith_db".to_string());
        let default_slippage_bps: u32 = std::env::var("DEFAULT_SLIPPAGE_BPS")
            .unwrap_or_else(|_| "500".to_string())
            .parse()
            .unwrap_or(500);
        let fee_wallet = std::env::var("FEE_WALLET").unwrap_or_else(|_| {
            eprintln!("⚠️  FEE_WALLET not set in .env — platform fee will be skipped on swaps.");
            String::new()
        });

        Ok(Self {
            telegram_token,
            rpc_url,
            master_key_b64,
            db_path,
            default_slippage_bps,
            fee_wallet,
        })
    }
}
