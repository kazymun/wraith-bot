use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Config {
    pub telegram_token: String,
    pub rpc_url: String,
    /// Optional backup RPC endpoint, used automatically if `rpc_url`
    /// fails on a given call (network error, malformed response, or an
    /// RPC-level error -- e.g. a provider hitting its monthly credit
    /// cap and starting to return non-JSON "Unauthorized" bodies). Set
    /// via RPC_URL_FALLBACK in .env; leave unset to run with no
    /// fallback (single point of failure -- fine for local dev, not
    /// recommended for anything unattended).
    pub rpc_url_fallback: Option<String>,
    pub pepper_b64: String,
    pub db_path: String,
    pub default_slippage_bps: u32,
    pub fee_wallet: String,
    /// Optional Jupiter API key (get one free at https://portal.jup.ag).
    /// If set, we use https://api.jup.ag with the key attached (recommended
    /// -- your own rate limit, more future-proof). If unset, we fall back
    /// to the no-key https://lite-api.jup.ag endpoint so the bot still
    /// works out of the box.
    pub jupiter_api_key: Option<String>,
    /// Minimum PIN length. 4 digits is 10,000 combinations -- crackable
    /// against a stolen DB dump even with Argon2id given enough time on
    /// attacker hardware. 6+ raises that to 1,000,000+ combinations.
    /// Enforce this in handlers.rs wherever a PIN is set or changed.
    pub min_pin_length: usize,
    /// Telegram user IDs that bypass the subscription paywall entirely --
    /// for friends/testers/yourself. Comma-separated in FREE_ACCESS_IDS,
    /// e.g. "123456789,987654321". Get a user's ID by having them message
    /// @userinfobot on Telegram.
    pub free_access_ids: Vec<i64>,
    /// Monthly subscription price, in lamports. Set via SUBSCRIPTION_SOL
    /// in .env (a decimal SOL amount, e.g. "0.3"). Set to "0" (or leave
    /// unset -- 0 is also the default) to disable the paywall entirely --
    /// see App::has_access in handlers.rs, which treats 0 as "everyone
    /// has access, no subscribe prompt ever shown". This is the only
    /// place the price is defined; every prompt/button/payment in
    /// handlers.rs reads it from here, so changing it later is a
    /// one-line .env edit + restart, nothing else.
    pub subscription_lamports: u64,
    /// Max lamports Jupiter's dynamic priority-fee mode may spend per
    /// swap bidding for faster block inclusion (see jupiter.rs for how
    /// this works). Set via MAX_PRIORITY_FEE_SOL in .env (decimal SOL,
    /// e.g. "0.005"). Defaults to 0.003 SOL (~$0.50-$1 depending on SOL
    /// price) if unset -- enough to meaningfully help during a busy
    /// launch without silently eating a large chunk of a small trade.
    /// Set to "0" to disable priority fees entirely (cheapest, slowest).
    pub max_priority_fee_lamports: u64,
    /// Cut Wraith takes of STAKING GAINS ONLY (never principal) when a
    /// user unstakes from the built-in JitoSOL yield feature, in basis
    /// points. Set via YIELD_FEE_BPS in .env. Defaults to 1000 (10%).
    pub yield_fee_bps: u32,
    /// Liquid SOL every auto-yield user always keeps un-staked in their
    /// active wallet, so there's headroom for network fees / a quick
    /// small trade without needing to unstake first. Set via
    /// YIELD_RESERVE_SOL in .env; defaults to 0.05 SOL.
    pub yield_reserve_lamports: u64,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let telegram_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN is not set")?;
        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .context("SOLANA_RPC_URL is not set")?;
        let rpc_url_fallback = std::env::var("RPC_URL_FALLBACK").ok().filter(|s| !s.trim().is_empty());
        let pepper_b64 = std::env::var("WRAITH_PEPPER")
            .context(
                "WRAITH_PEPPER is not set. Generate one with `openssl rand -base64 32` \
                 and store it somewhere DIFFERENT from your database backups \
                 (secrets manager / KMS, not the same disk or repo).",
            )?;
        let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "./wraith_db".to_string());
        let default_slippage_bps: u32 = std::env::var("DEFAULT_SLIPPAGE_BPS")
            .unwrap_or_else(|_| "500".to_string())
            .parse()
            .unwrap_or(500);
        let fee_wallet = std::env::var("FEE_WALLET").unwrap_or_else(|_| {
            eprintln!("⚠️  FEE_WALLET not set in .env — platform fee will be skipped on swaps.");
            String::new()
        });
        let min_pin_length: usize = std::env::var("MIN_PIN_LENGTH")
            .unwrap_or_else(|_| "6".to_string())
            .parse()
            .unwrap_or(6);
        let jupiter_api_key = std::env::var("JUPITER_API_KEY").ok().filter(|s| !s.is_empty());
        let free_access_ids: Vec<i64> = std::env::var("FREE_ACCESS_IDS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        let subscription_lamports: u64 = std::env::var("SUBSCRIPTION_SOL")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|sol| *sol >= 0.0)
            .map(|sol| (sol * 1_000_000_000.0).round() as u64)
            .unwrap_or(0); // default: free (no subscription required)
        let max_priority_fee_lamports: u64 = std::env::var("MAX_PRIORITY_FEE_SOL")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|sol| *sol >= 0.0)
            .map(|sol| (sol * 1_000_000_000.0).round() as u64)
            .unwrap_or(3_000_000); // default: 0.003 SOL
        let yield_fee_bps: u32 = std::env::var("YIELD_FEE_BPS")
            .unwrap_or_else(|_| "1000".to_string())
            .parse()
            .unwrap_or(1000); // default: 10%
        let yield_reserve_lamports: u64 = std::env::var("YIELD_RESERVE_SOL")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|sol| *sol >= 0.0)
            .map(|sol| (sol * 1_000_000_000.0).round() as u64)
            .unwrap_or(50_000_000); // default: 0.05 SOL

        // NOTE: WRAITH_MASTER_KEY no longer exists. There is no single
        // key anywhere that decrypts every user's wallet. Each user's
        // secret requires their own PIN combined with this pepper --
        // see crypto.rs for the envelope-encryption scheme.

        Ok(Self {
            telegram_token,
            rpc_url,
            rpc_url_fallback,
            pepper_b64,
            db_path,
            default_slippage_bps,
            fee_wallet,
            min_pin_length,
            jupiter_api_key,
            free_access_ids,
            subscription_lamports,
            max_priority_fee_lamports,
            yield_fee_bps,
            yield_reserve_lamports,
        })
    }
}
