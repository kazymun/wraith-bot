use anyhow::Result;
use serde_json::Value;

pub async fn get_token_pair(ca: &str) -> Result<Option<Value>> {
    let url = format!("https://api.dexscreener.com/latest/dex/tokens/{ca}");
    let client = reqwest::Client::builder()
        // Forced to HTTP/1.1 -- see telegram.rs for why (ALPN/h2
        // negotiation was surfacing as "invalid HTTP version parsed").
        .http1_only()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp: Value = client.get(url).send().await?.json().await?;
    Ok(resp["pairs"].as_array().and_then(|p| p.first()).cloned())
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // Forced to HTTP/1.1 -- see telegram.rs for why (ALPN/h2
        // negotiation was surfacing as "invalid HTTP version parsed").
        .http1_only()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default()
}

fn push_solana_addrs(v: &Value, out: &mut Vec<String>) {
    if let Some(arr) = v.as_array() {
        for t in arr {
            if t["chainId"].as_str() == Some("solana") {
                if let Some(a) = t["tokenAddress"].as_str() {
                    out.push(a.to_string());
                }
            }
        }
    }
}

/// Pulls candidate Solana token addresses from several DexScreener feeds:
/// - token-boosts (latest + top): paid promotion, wide reach but often already pumped
/// - token-profiles/latest: freely submitted the moment a project fills in its
///   socials/website, usually right at launch — this is our earliest-signal source
///
/// Merging these (instead of relying on boosts alone) is what lets us catch
/// tokens before they're hyped, not just after.
pub async fn get_candidate_addresses() -> Result<Vec<String>> {
    let client = http_client();
    let mut addrs: Vec<String> = vec![];

    for url in [
        "https://api.dexscreener.com/token-boosts/latest/v1",
        "https://api.dexscreener.com/token-boosts/top/v1",
        "https://api.dexscreener.com/token-profiles/latest/v1",
    ] {
        if let Ok(resp) = client.get(url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                push_solana_addrs(&v, &mut addrs);
            }
        }
    }

    addrs.sort();
    addrs.dedup();
    addrs.truncate(90); // 3 batched calls of 30 addrs max
    Ok(addrs)
}

/// Batches address lookups (DexScreener's tokens endpoint accepts up to ~30
/// comma-separated addresses per call).
pub async fn get_pairs_for_addresses(addrs: &[String]) -> Result<Vec<Value>> {
    if addrs.is_empty() {
        return Ok(vec![]);
    }
    let client = http_client();
    let mut out = vec![];

    for chunk in addrs.chunks(30) {
        let joined = chunk.join(",");
        let url = format!("https://api.dexscreener.com/latest/dex/tokens/{joined}");
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(pairs) = v["pairs"].as_array() {
                    out.extend(pairs.iter().filter(|p| p["chainId"].as_str() == Some("solana")).cloned());
                }
            }
        }
    }
    Ok(out)
}

/// Percent-encodes a query string for the search endpoint. We avoid pulling
/// in a dedicated URL-encoding crate for just this one call site -- token
/// names/symbols are overwhelmingly plain ASCII words/numbers, so encoding
/// spaces and a small set of reserved characters covers the real world.
fn encode_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Free-text search (by name or symbol, e.g. "bonk" or "peanut the squirrel")
/// against DexScreener's search endpoint, restricted to Solana pairs and
/// sorted by liquidity (highest first) so the "real" token surfaces above
/// any low-liquidity copycats/scam clones sharing the same name.
pub async fn search_tokens(query: &str) -> Result<Vec<Value>> {
    let client = http_client();
    let url = format!("https://api.dexscreener.com/latest/dex/search?q={}", encode_query(query));
    let resp: Value = client.get(&url).send().await?.json().await?;

    let mut pairs: Vec<Value> = resp["pairs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p["chainId"].as_str() == Some("solana"))
        .collect();

    pairs.sort_by(|a, b| {
        let la = a["liquidity"]["usd"].as_f64().unwrap_or(0.0);
        let lb = b["liquidity"]["usd"].as_f64().unwrap_or(0.0);
        lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(pairs)
}

/// Kept for the manual "AI Gem Scanner" button — same broadened candidate
/// pool, single call.
pub async fn get_trending_solana_pairs() -> Result<Vec<Value>> {
    let addrs = get_candidate_addresses().await?;
    get_pairs_for_addresses(&addrs).await
}

/// Minimal snapshot of a token from a previous scan, used to detect momentum
/// (rising liquidity/volume) between polling intervals rather than judging
/// off a single static reading.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub liq_usd: f64,
    pub vol_h24: f64,
}

pub struct GemSignal {
    pub score: i32,
    pub tier: &'static str,
    pub notes: Vec<String>,
    pub is_fresh: bool,
}

pub fn tier_for_score(score: i32) -> &'static str {
    if score >= 75 {
        "🚀 HIGH POTENTIAL"
    } else if score >= 55 {
        "⚡ MODERATE POTENTIAL"
    } else if score >= 40 {
        "👀 WATCH"
    } else {
        "SKIP"
    }
}

/// Static keyword categories that tend to draw outsized attention on
/// Solana regardless of any single day's trending topic (AI agents,
/// real-world-assets, DePIN infra, political/meme-figure coins, animal
/// memes, gaming). This is a heuristic on the token's name/symbol only --
/// it can't know what's *actually* trending on a given day (that would
/// need a live social/trend data source), so treat it as a mild bonus,
/// not a guarantee the narrative is currently hot. Update this list over
/// time as narratives shift.
const NARRATIVE_KEYWORDS: &[(&str, &[&str])] = &[
    ("AI / Agent", &["ai", "agent", "gpt", "llm", "neural"]),
    ("RWA", &["rwa", "realworld", "tokeniz"]),
    ("DePIN", &["depin", "dewi", "compute", "gpu"]),
    ("Political/Figure", &["trump", "elon", "musk", "biden", "maga"]),
    ("Animal meme", &["dog", "cat", "shib", "inu", "pepe", "frog", "wif"]),
    ("Gaming", &["game", "gaming", "play2earn", "p2e"]),
];

/// Checks a pair's base-token name/symbol against `NARRATIVE_KEYWORDS`.
/// Returns the first matching category label and a flat bonus, or `None`.
fn match_narrative(pair: &Value) -> Option<(&'static str, i32)> {
    let name = pair["baseToken"]["name"].as_str().unwrap_or("").to_lowercase();
    let symbol = pair["baseToken"]["symbol"].as_str().unwrap_or("").to_lowercase();
    let haystack = format!("{name} {symbol}");

    for (label, keywords) in NARRATIVE_KEYWORDS {
        if keywords.iter().any(|kw| haystack.contains(kw)) {
            return Some((*label, 8));
        }
    }
    None
}

/// Early-gem scoring. Unlike `analyze()` (which is tuned for rug-risk on a
/// token the user already picked), this is tuned to surface promising tokens
/// *before* they're widely noticed:
/// - freshness is rewarded, not penalized, as long as basic safety floors hold
/// - short-horizon (5m/1h) buy/sell pressure is weighted alongside 24h data
/// - momentum vs. the last scan (if we have one) adds/subtracts points
///
/// This only looks at DexScreener market data. On-chain checks (mint/freeze
/// authority, holder concentration) are layered on separately in the caller
/// since they require RPC calls - see `App::apply_onchain_checks`.
pub fn score_gem(pair: &Value, prev: Option<&Snapshot>) -> GemSignal {
    let mc = f(pair, &["fdv"]);
    let liq = f(pair, &["liquidity", "usd"]);
    let vol24h = f(pair, &["volume", "h24"]);
    let buys5m = pair["txns"]["m5"]["buys"].as_i64().unwrap_or(0);
    let sells5m = pair["txns"]["m5"]["sells"].as_i64().unwrap_or(0);
    let buys1h = pair["txns"]["h1"]["buys"].as_i64().unwrap_or(0);
    let sells1h = pair["txns"]["h1"]["sells"].as_i64().unwrap_or(0);
    let change1h = f(pair, &["priceChange", "h1"]);

    // Hard safety floor — skip obvious no-liquidity junk outright.
    if liq < 3_000.0 || mc <= 0.0 {
        return GemSignal { score: 0, tier: "SKIP", notes: vec!["Below safety floor".to_string()], is_fresh: false };
    }

    let mut score = 0i32;
    let mut notes = vec![];

    // Several trader writeups converge on a rough "sweet spot" market-cap
    // band for early entries (not too microscopic, not already mature) -
    // treat this as a mild tiebreaker, not a hard rule.
    if mc >= 70_000.0 && mc <= 11_000_000.0 {
        score += 5;
    }

    let age_hours = pair
        .get("pairCreatedAt")
        .and_then(|v| v.as_i64())
        .map(|created| (chrono_now_ms() - created) as f64 / 3_600_000.0)
        .unwrap_or(999.0);
    let is_fresh = age_hours < 6.0;

    // Freshness is now a minor tiebreaker, not the dominant signal --
    // quality metrics below (liquidity, buy pressure, momentum, narrative
    // fit) carry most of the weight so the scanner doesn't just surface
    // "brand new" tokens by default.
    if age_hours < 1.0 && liq > 8_000.0 {
        score += 8;
        notes.push("🆕 Brand new (under 1h) with real liquidity".to_string());
    } else if age_hours < 6.0 {
        score += 5;
        notes.push("🆕 Very early (under 6h)".to_string());
    } else if age_hours < 24.0 {
        score += 3;
    }

    if liq >= 15_000.0 {
        score += 20;
    } else if liq >= 8_000.0 {
        score += 12;
    } else {
        score -= 5;
        notes.push("⚠️ Thin liquidity".to_string());
    }

    let liq_mc = if mc > 0.0 { liq / mc } else { 0.0 };
    if liq_mc > 0.08 {
        score += 14;
        notes.push("Healthy liquidity/MC ratio".to_string());
    } else if liq_mc < 0.015 {
        score -= 10;
        notes.push("⚠️ Low liquidity/MC ratio".to_string());
    }

    if buys5m + sells5m >= 5 {
        if buys5m > sells5m * 2 {
            score += 18;
            notes.push("🔥 Strong 5m buy pressure".to_string());
        } else if sells5m > buys5m * 2 {
            score -= 15;
            notes.push("🚨 5m sell-off".to_string());
        }
    }
    if buys1h + sells1h >= 10 {
        if buys1h > sells1h * 2 {
            score += 15;
            notes.push("Buy pressure building over the last hour".to_string());
        } else if sells1h > buys1h * 3 {
            score -= 20;
            notes.push("🚨 Heavy 1h sell pressure".to_string());
        }
    }

    if let Some(p) = prev {
        if p.liq_usd > 0.0 {
            let liq_growth = (liq - p.liq_usd) / p.liq_usd;
            if liq_growth > 0.25 {
                score += 16;
                notes.push(format!("💧 Liquidity up {:.0}% since last scan", liq_growth * 100.0));
            }
        }
        if p.vol_h24 > 0.0 {
            let vol_growth = (vol24h - p.vol_h24) / p.vol_h24;
            if vol_growth > 0.5 {
                score += 14;
                notes.push("📈 Volume accelerating".to_string());
            }
        }
    }

    if let Some((label, bonus)) = match_narrative(pair) {
        score += bonus;
        notes.push(format!("🎯 Trending narrative: {label}"));
    }

    if change1h > 200.0 {
        score -= 15;
        notes.push("⚠️ Already up huge — chasing risk".to_string());
    } else if change1h > 15.0 && change1h < 150.0 {
        score += 8;
    }

    if let Some(dex) = pair["dexId"].as_str() {
        if matches!(dex, "raydium" | "orca" | "meteora" | "pumpswap") {
            score += 5;
        }
    }

    score = score.clamp(0, 100);
    let tier = tier_for_score(score);

    GemSignal { score, tier, notes, is_fresh }
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
