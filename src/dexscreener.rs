use anyhow::Result;
use serde_json::Value;

pub async fn get_token_pair(ca: &str) -> Result<Option<Value>> {
    let url = format!("https://api.dexscreener.com/latest/dex/tokens/{ca}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp: Value = client.get(url).send().await?.json().await?;
    Ok(resp["pairs"].as_array().and_then(|p| p.first()).cloned())
}

pub async fn get_trending_solana_pairs() -> Result<Vec<Value>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;

    // Get boosted tokens list
    let boosts: serde_json::Value = client
        .get("https://api.dexscreener.com/token-boosts/latest/v1")
        .send().await?.json().await?;

    let addrs: Vec<String> = boosts.as_array()
        .cloned().unwrap_or_default()
        .into_iter()
        .filter(|t| t["chainId"].as_str() == Some("solana"))
        .take(6)
        .filter_map(|t| t["tokenAddress"].as_str().map(|s| s.to_string()))
        .collect();

    if addrs.is_empty() { return Ok(vec![]); }

    // Batch fetch all pairs in one call
    let joined = addrs.join(",");
    let url = format!("https://api.dexscreener.com/latest/dex/tokens/{joined}");
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;

    Ok(resp["pairs"].as_array()
        .cloned().unwrap_or_default()
        .into_iter()
        .filter(|p| p["chainId"].as_str() == Some("solana"))
        .take(6)
        .collect())
}

pub struct Analysis {
    pub score: i32,
    pub risk_level: &'static str,
    pub risk_emoji: &'static str,
    pub flags: Vec<String>,
    pub good: Vec<String>,
}

fn f(v: &Value, path: &[&str]) -> f64 {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_f64()
        .or_else(|| cur.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

/// Heuristic risk scoring based purely on public market data. This is not
/// financial advice and does not detect every kind of rug - it flags
/// statistically risky patterns (thin liquidity, sell pressure, extreme
/// age/volatility) the same way the original bot's analyzer did.
pub fn analyze(pair: &Value) -> Analysis {
    let mut score: i32 = 100;
    let mut flags = vec![];
    let mut good = vec![];

    let liq_usd = f(pair, &["liquidity", "usd"]);
    if liq_usd < 10_000.0 {
        score -= 30;
        flags.push("🚨 Liquidity under $10K — extremely dangerous".to_string());
    } else if liq_usd < 50_000.0 {
        score -= 15;
        flags.push("⚠️ Low liquidity under $50K — high slippage risk".to_string());
    } else if liq_usd > 500_000.0 {
        good.push("✅ Strong liquidity over $500K".to_string());
    } else {
        good.push("✅ Decent liquidity".to_string());
    }

    let mc = f(pair, &["fdv"]);
    if mc > 0.0 && liq_usd > 0.0 {
        let ratio = (liq_usd / mc) * 100.0;
        if ratio < 1.0 {
            score -= 20;
            flags.push(format!("⚠️ Liquidity/MC ratio only {ratio:.2}% — very low"));
        } else if ratio > 5.0 {
            good.push(format!("✅ Healthy liq/MC ratio: {ratio:.2}%"));
        }
    }

    let change1h = f(pair, &["priceChange", "h1"]);
    if change1h < -30.0 {
        score -= 25;
        flags.push(format!("🚨 Down {:.0}% in 1 hour — possible dump", change1h.abs()));
    } else if change1h > 100.0 {
        score -= 10;
        flags.push(format!("⚠️ Up {change1h:.0}% in 1 hour — possible pump & dump"));
    }

    let vol24h = f(pair, &["volume", "h24"]);
    if mc > 0.0 {
        let vol_to_mc = (vol24h / mc) * 100.0;
        if vol_to_mc > 200.0 {
            score -= 15;
            flags.push(format!("⚠️ Volume {vol_to_mc:.0}% of MC — wash trading suspected"));
        } else if vol_to_mc > 10.0 {
            good.push(format!("✅ Healthy volume: {vol_to_mc:.1}% of MC"));
        } else if vol_to_mc < 1.0 {
            score -= 10;
            flags.push("⚠️ Very low volume — low interest or dead token".to_string());
        }
    }

    if let Some(created_at) = pair.get("pairCreatedAt").and_then(|v| v.as_i64()) {
        let now_ms = chrono_now_ms();
        let age_hours = (now_ms - created_at) as f64 / (1000.0 * 60.0 * 60.0);
        if age_hours < 1.0 {
            score -= 15;
            flags.push("⚠️ Token is less than 1 hour old — extremely new".to_string());
        } else if age_hours < 24.0 {
            score -= 5;
            flags.push(format!("⚠️ Token only {age_hours:.0} hours old — still very new"));
        } else if age_hours > 168.0 {
            good.push(format!("✅ Token is {} days old — established", (age_hours / 24.0) as i64));
        }
    }

    let buys = pair["txns"]["h24"]["buys"].as_i64().unwrap_or(0);
    let sells = pair["txns"]["h24"]["sells"].as_i64().unwrap_or(0);
    let total = buys + sells;
    if total < 50 {
        score -= 10;
        flags.push(format!("⚠️ Only {total} transactions in 24h — low activity"));
    }
    if sells > buys * 3 {
        score -= 20;
        flags.push(format!("🚨 Sell pressure: {sells} sells vs {buys} buys — dumping"));
    } else if buys > sells * 2 {
        good.push(format!("✅ Buy pressure: {buys} buys vs {sells} sells"));
    }

    if let Some(dex) = pair["dexId"].as_str() {
        if matches!(dex, "raydium" | "orca" | "jupiter") {
            good.push(format!("✅ Listed on {dex} — legitimate DEX"));
        }
    }

    score = score.clamp(0, 100);
    let (risk_level, risk_emoji) = if score >= 75 {
        ("SAFE", "✅")
    } else if score >= 50 {
        ("MODERATE RISK", "⚠️")
    } else if score >= 25 {
        ("HIGH RISK", "🔴")
    } else {
        ("LIKELY RUG", "🚨")
    };

    Analysis {
        score,
        risk_level,
        risk_emoji,
        flags,
        good,
    }
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
