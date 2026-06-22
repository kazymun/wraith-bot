use anyhow::{anyhow, Result};
use serde_json::Value;

pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

pub struct Jupiter {
    http: reqwest::Client,
}

impl Jupiter {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
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
        let url = "https://quote-api.jup.ag/v6/quote";
        let resp = self
            .http
            .get(url)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", &amount.to_string()),
                ("slippageBps", &slippage_bps.to_string()),
                ("platformFeeBps", "50"),
            ])
            .send()
            .await?;

        let value: Value = resp.json().await?;
        if value.get("error").is_some() {
            return Err(anyhow!("Jupiter quote error: {value}"));
        }
        Ok(value)
    }

    /// Takes a quote response and returns the base64-encoded unsigned
    /// (versioned) transaction ready to be signed by the user's keypair.
    /// fee_wallet: if non-empty, Jupiter will route 0.5% to that address.
    pub async fn get_swap_transaction(&self, quote: &Value, user_pubkey: &str, fee_wallet: &str) -> Result<String> {
        let url = "https://quote-api.jup.ag/v6/swap";
        let mut body = serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": user_pubkey,
            "wrapAndUnwrapSol": true,
        });
        if !fee_wallet.is_empty() {
            body["feeAccount"] = serde_json::json!(fee_wallet);
        }

        let resp: Value = self.http.post(url).json(&body).send().await?.json().await?;
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
