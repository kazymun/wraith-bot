use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const PUMPPORTAL_WS: &str = "wss://pumpportal.fun/api/data";

/// Roughly how much SOL sits in the bonding curve at migration (varies
/// slightly in practice, ~85 SOL is the commonly cited figure). We use this
/// only to compute a rough "% of the way to migration" -- it's a heuristic,
/// not an exact on-chain constant, so don't treat it as gospel.
const MIGRATION_SOL_APPROX: f64 = 85.0;
const CURVE_WATCH_THRESHOLD_PCT: f64 = 0.30;

#[derive(Debug, Clone)]
pub enum PumpEvent {
    /// Token was just created -- seconds old. We don't alert on these (way
    /// too high volume/noise) but track them for later dedup + "how early
    /// did we actually catch this" bookkeeping.
    NewToken(TokenData),
    /// Crossed our watch threshold toward migration. Still pre-migration --
    /// NOT buyable through this bot's Jupiter-based swap yet.
    CurveProgress(TokenData),
    /// Just migrated to a real AMM pool (PumpSwap/Raydium). Tradeable
    /// through Jupiter now, and DexScreener will pick it up shortly too.
    Migrated(TokenData),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenData {
    pub mint: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(rename = "marketCapSol", default)]
    pub market_cap_sol: f64,
    #[serde(rename = "vSolInBondingCurve", default)]
    pub v_sol_in_bonding_curve: f64,
}

/// Connects to PumpPortal's free public WebSocket and streams new-token and
/// migration events forever, reconnecting with backoff on any disconnect
/// (public WS endpoints do drop). This is the earliest possible signal for
/// a Solana memecoin -- tokens show up here the instant they're created,
/// well before DexScreener has any listing for them at all.
pub async fn run(tx: mpsc::Sender<PumpEvent>) {
    let mut backoff = 1u64;
    loop {
        match connect_and_stream(&tx).await {
            Ok(()) => backoff = 1,
            Err(e) => eprintln!("PumpPortal WS error (reconnecting): {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn connect_and_stream(tx: &mpsc::Sender<PumpEvent>) -> anyhow::Result<()> {
    let (mut ws, _) = connect_async(PUMPPORTAL_WS).await?;

    ws.send(Message::Text(json!({ "method": "subscribeNewToken" }).to_string().into())).await?;
    ws.send(Message::Text(json!({ "method": "subscribeMigration" }).to_string().into())).await?;

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };

        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // We only subscribed to NewToken + Migration, so anything with a
        // "mint" field is one of the two. txType "create" is the documented
        // shape for new-token events; anything else with a mint we treat as
        // a migration event. This is a best-effort split -- PumpPortal's
        // migration payload shape isn't as clearly published as the create
        // event, so this may need a tweak once you see live traffic (an
        // unrecognized shape gets logged below instead of silently dropped).
        if v.get("mint").is_none() {
            continue;
        }

        let tx_type = v.get("txType").and_then(|t| t.as_str()).unwrap_or("");
        let data: TokenData = match serde_json::from_value(v.clone()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("PumpPortal: unrecognized message shape, skipping ({e}): {text}");
                continue;
            }
        };

        if tx_type == "create" {
            let _ = tx.send(PumpEvent::NewToken(data.clone())).await;
            continue;
        }

        if tx_type.is_empty() || tx_type == "migrate" {
            let _ = tx.send(PumpEvent::Migrated(data)).await;
            continue;
        }

        // Any other txType (buy/sell) shouldn't appear since we didn't
        // subscribe to trade streams, but ignore defensively if one slips through.
        if data.v_sol_in_bonding_curve >= MIGRATION_SOL_APPROX * CURVE_WATCH_THRESHOLD_PCT {
            let _ = tx.send(PumpEvent::CurveProgress(data)).await;
        }
    }

    Ok(())
}
