use anyhow::{anyhow, Result};
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
/// JitoSOL -- Jito's liquid staking token. Verified independently against
/// Solana Explorer, Solscan, and Jito's own docs before hardcoding (a
/// wrong mint here would misroute real user funds). 9 decimals, same as
/// SOL. Its SOL value rises over time as staking rewards accrue -- there's
/// no separate "claim rewards" step, the exchange rate itself grows.
pub const JITO_SOL_MINT: &str = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";

const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Derives the associated token account (ATA) address for a given owner +
/// mint, using the standard SPL deterministic derivation. This is a pure
/// PDA computation -- it doesn't touch the network and doesn't require the
/// account to exist yet, so it works even before the ATA has been created.
///
/// Jupiter's `feeAccount` parameter must be a token account whose mint is
/// one of the two mints in the swap pair (input or output) -- passing a
/// raw wallet address (a SystemAccount, not a token account) makes
/// Jupiter reject the swap, or builds a transaction that fails on-chain.
/// Since SOL is always one side of every swap this bot does (input on
/// buys, output on sells), the wallet's wrapped-SOL ATA is always valid
/// as the fee account regardless of which token is being traded.
///
/// NOTE: this only *computes the address*. The account must actually be
/// initialized on-chain (create/wrap once via `spl-token create-account`
/// or any "wrap SOL" tool pointed at this owner) before fee collection
/// will work -- Jupiter does not create the fee account for you as a
/// side effect of the swap.
pub fn derive_wsol_fee_account(owner_pubkey: &str) -> Result<String> {
    let owner = Pubkey::from_str(owner_pubkey).map_err(|_| anyhow!("FEE_WALLET is not a valid pubkey"))?;
    let mint = Pubkey::from_str(SOL_MINT).expect("hardcoded SOL mint is always valid");
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).expect("hardcoded program id is always valid");
    let associated_program =
        Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM_ID).expect("hardcoded program id is always valid");

    let (ata, _bump) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    );
    Ok(ata.to_string())
}

/// Jupiter deprecated the old `quote-api.jup.ag/v6` endpoint (shut off
/// October last year). Current endpoints per dev.jup.ag:
/// - `https://api.jup.ag/swap/v1` -- requires a free API key (x-api-key
///   header), get one at https://portal.jup.ag
/// - `https://lite-api.jup.ag/swap/v1` -- works with no key at all, meant
///   for light/free usage. Jupiter has flagged this for eventual
///   deprecation too, but as of writing there's no hard cutoff date.
///
/// We use the API-key endpoint when a key is configured (recommended,
/// more future-proof and comes with its own rate limit), and silently
/// fall back to the no-key lite endpoint otherwise so the bot still works
/// out of the box.
const JUPITER_API_BASE: &str = "https://api.jup.ag/swap/v1";
const JUPITER_LITE_API_BASE: &str = "https://lite-api.jup.ag/swap/v1";

#[derive(Clone)]
pub struct Jupiter {
    http: reqwest::Client,
    api_key: Option<String>,
}

impl Jupiter {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            // Forced to HTTP/1.1 -- see telegram.rs for why (ALPN/h2
            // negotiation was surfacing as "invalid HTTP version parsed").
            http: reqwest::Client::builder()
                .http1_only()
                .build()
                .expect("failed to build reqwest client"),
            api_key,
        }
    }

    fn base_url(&self) -> &'static str {
        if self.api_key.is_some() {
            JUPITER_API_BASE
        } else {
            JUPITER_LITE_API_BASE
        }
    }

    fn add_api_key(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header("x-api-key", key),
            None => req,
        }
    }

    /// amount is in the input token's raw smallest unit (lamports for SOL).
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u32,
    ) -> Result<Value> {
        let url = format!("{}/quote", self.base_url());
        let req = self
            .http
            .get(&url)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", &amount.to_string()),
                ("slippageBps", &slippage_bps.to_string()),
                ("platformFeeBps", "75"), // 0.75% -- see fee_wallet in config.rs for where this gets collected
            ]);
        let resp = self.add_api_key(req).send().await?;

        let value: Value = resp.json().await?;
        if value.get("error").is_some() {
            return Err(anyhow!("Jupiter quote error: {value}"));
        }
        Ok(value)
    }

    /// Takes a quote response and returns the base64-encoded unsigned
    /// (versioned) transaction ready to be signed by the user's keypair.
    /// fee_wallet: if non-empty, Jupiter will route 0.75% to the wrapped-SOL
    /// token account derived from this address (NOT to the raw address
    /// itself -- see `derive_wsol_fee_account` for why).
    ///
    /// Jupiter's rule (ExactIn mode, which this bot uses): the feeAccount's
    /// mint must be either the input OR output mint of the swap. Since SOL
    /// is always one side of every swap this bot does (input on buys,
    /// output on sells), the WSOL fee account is always valid -- no
    /// direction check needed. (A prior "fix" here restricted this to
    /// buys only, based on a misreading of Jupiter's docs -- reverted.)
    ///
    /// `max_priority_fee_lamports`: caps what Jupiter's "auto" priority-fee
    /// mode is allowed to spend bidding for faster inclusion in a block.
    /// This is the real lever for "landing before other traders" during a
    /// busy launch -- validators prioritize transactions offering a higher
    /// fee when a block is contested. It is NOT front-running or private
    /// mempool access (Solana doesn't have a public mempool the way
    /// Ethereum does) -- it's simply outbidding other *ordinary* traders
    /// for block space, which is the standard, legitimate mechanism every
    /// wallet/bot on Solana uses to compete for speed. Set to 0 to disable
    /// (transaction lands at whatever base fee, cheapest but slowest under
    /// congestion).
    pub async fn get_swap_transaction(
        &self,
        quote: &Value,
        user_pubkey: &str,
        fee_wallet: &str,
        max_priority_fee_lamports: u64,
    ) -> Result<String> {
        let url = format!("{}/swap", self.base_url());
        let mut body = serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": user_pubkey,
            "wrapAndUnwrapSol": true,
            // Jupiter estimates compute units actually needed instead of
            // reserving the max -- slightly cheaper and, more importantly
            // here, lets the "auto" priority fee below concentrate its
            // budget on priority (speed) rather than padding for unused
            // compute headroom.
            "dynamicComputeUnitLimit": true,
        });
        if max_priority_fee_lamports > 0 {
            body["prioritizationFeeLamports"] = serde_json::json!({
                "priorityLevelWithMaxLamports": {
                    "priorityLevel": "veryHigh",
                    "maxLamports": max_priority_fee_lamports
                }
            });
        }
        if !fee_wallet.is_empty() {
            let fee_account = derive_wsol_fee_account(fee_wallet)?;
            body["feeAccount"] = serde_json::json!(fee_account);
        }

        let req = self.http.post(&url).json(&body);
        let resp: Value = self.add_api_key(req).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("Jupiter swap error: {err}"));
        }
        resp["swapTransaction"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("Jupiter swap response missing swapTransaction"))
    }
}

pub fn out_amount(quote: &Value) -> Option<u64> {
    quote["outAmount"].as_str()?.parse().ok()
}

pub fn price_impact_pct(quote: &Value) -> Option<f64> {
    quote["priceImpactPct"].as_str()?.parse().ok()
}
