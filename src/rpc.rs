use anyhow::{anyhow, Result};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SolanaRpc {
    url: String,
    http: reqwest::Client,
}

impl SolanaRpc {
    pub fn new(url: String) -> Self {
        Self {
            url,
            // Forced to HTTP/1.1 -- see telegram.rs for why (ALPN/h2
            // negotiation on some RPC providers was surfacing as
            // "invalid HTTP version parsed").
            http: reqwest::Client::builder()
                .http1_only()
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });
        let resp: Value = self.http.post(&self.url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("RPC error on {method}: {err}"));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("RPC response for {method} had no result field"))
    }

    pub async fn get_balance_lamports(&self, pubkey: &str) -> Result<u64> {
        let result = self.call("getBalance", json!([pubkey])).await?;
        result["value"]
            .as_u64()
            .ok_or_else(|| anyhow!("unexpected getBalance response shape"))
    }

    /// Fetches the SPL token balance (in raw units) and decimals for a given
    /// owner + mint. Returns (raw_amount, decimals). Returns (0, decimals)
    /// if the owner has no token account for that mint, where decimals
    /// falls back to the mint's own decimals.
    pub async fn get_token_balance(&self, owner_pubkey: &str, mint: &str) -> Result<(u64, u8)> {
        let result = self
            .call(
                "getTokenAccountsByOwner",
                json!([
                    owner_pubkey,
                    { "mint": mint },
                    { "encoding": "jsonParsed" }
                ]),
            )
            .await?;

        let accounts = result["value"].as_array().cloned().unwrap_or_default();
        if accounts.is_empty() {
            let decimals = self.get_mint_decimals(mint).await.unwrap_or(9);
            return Ok((0, decimals));
        }

        let info = &accounts[0]["account"]["data"]["parsed"]["info"]["tokenAmount"];
        let raw: u64 = info["amount"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("could not parse token amount"))?;
        let decimals = info["decimals"].as_u64().unwrap_or(9) as u8;
        Ok((raw, decimals))
    }

    /// Whether an account exists on-chain at all (regardless of type).
    /// Used to check, at startup, whether the platform fee wallet's
    /// wrapped-SOL token account has actually been created -- if it
    /// hasn't, Jupiter has nowhere to deliver the platform fee and every
    /// trade will silently pay $0 in fees despite `platformFeeBps` being
    /// set on every quote.
    pub async fn get_account_exists(&self, pubkey: &str) -> Result<bool> {
        let result = self.call("getAccountInfo", json!([pubkey, { "encoding": "base64" }])).await?;
        Ok(!result["value"].is_null())
    }

    pub async fn get_mint_decimals(&self, mint: &str) -> Result<u8> {
        let result = self
            .call("getAccountInfo", json!([mint, { "encoding": "jsonParsed" }]))
            .await?;
        result["value"]["data"]["parsed"]["info"]["decimals"]
            .as_u64()
            .map(|d| d as u8)
            .ok_or_else(|| anyhow!("could not read mint decimals"))
    }

    /// Sends a base64-encoded, fully signed transaction. Returns the
    /// transaction signature.
    pub async fn send_raw_transaction_b64(&self, tx_b64: &str) -> Result<String> {
        let result = self
            .call(
                "sendTransaction",
                json!([
                    tx_b64,
                    { "encoding": "base64", "skipPreflight": false, "maxRetries": 3 }
                ]),
            )
            .await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("sendTransaction did not return a signature: {result}"))
    }

    pub async fn get_latest_blockhash(&self) -> Result<String> {
        let result = self.call("getLatestBlockhash", json!([])).await?;
        result["value"]["blockhash"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("could not fetch latest blockhash"))
    }

    /// Returns (mint_authority_renounced, freeze_authority_renounced).
    /// A live mint authority means the creator can print unlimited new
    /// supply whenever they want; a live freeze authority means they can
    /// freeze any holder's tokens. A renounced (null) authority on both is
    /// one of the clearest "clean contract" checks experienced memecoin
    /// traders do before entering - it doesn't guarantee safety, but a
    /// live mint/freeze authority is close to a guaranteed rug vector.
    pub async fn get_mint_authority_status(&self, mint: &str) -> Result<(bool, bool)> {
        let result = self
            .call("getAccountInfo", json!([mint, { "encoding": "jsonParsed" }]))
            .await?;
        let info = &result["value"]["data"]["parsed"]["info"];
        let mint_renounced = info["mintAuthority"].is_null();
        let freeze_renounced = info["freezeAuthority"].is_null();
        Ok((mint_renounced, freeze_renounced))
    }

    /// Rough top-10-holder concentration as a % of total supply.
    /// Caveat: this counts every largest account, including the liquidity
    /// pool's own vault, so it reads artificially high right after launch
    /// when most supply is still sitting in the pool. Treat it as a
    /// directional "is this heavily wallet-concentrated" signal, not a
    /// precise insider-holdings number.
    pub async fn get_top10_concentration_pct(&self, mint: &str) -> Result<Option<f64>> {
        let largest = self.call("getTokenLargestAccounts", json!([mint])).await?;
        let accounts = largest["value"].as_array().cloned().unwrap_or_default();
        if accounts.is_empty() {
            return Ok(None);
        }

        let supply_result = self.call("getTokenSupply", json!([mint])).await?;
        let total_supply = supply_result["value"]["uiAmount"].as_f64().unwrap_or(0.0);
        if total_supply <= 0.0 {
            return Ok(None);
        }

        let top10_sum: f64 = accounts
            .iter()
            .take(10)
            .filter_map(|a| a["uiAmount"].as_f64())
            .sum();

        Ok(Some((top10_sum / total_supply) * 100.0))
    }
}
