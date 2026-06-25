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
            http: reqwest::Client::new(),
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
}
