use crate::crypto::EnvelopeSecret;
use crate::state::{Awaiting, PinLockout, Position, UserRecord};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Per-token record for the gem scanner: what we saw last scan, plus
/// whether we've already alerted on it. Persisting this (instead of an
/// in-memory HashSet) means restarts don't cause duplicate alerts or lose
/// the "when did we first see this" timestamp, and lets us compute
/// liquidity/volume momentum between scans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GemSnapshot {
    pub liq_usd: f64,
    pub vol_h24: f64,
    pub first_seen: i64,
    pub alerted: bool,
}

/// Tracks a token we first heard about from the PumpPortal WebSocket feed
/// (i.e. at or near creation, before it ever has a DexScreener listing).
/// Persisted so this survives restarts and so the AI Gem Scanner can score
/// pump.fun tokens (pre- and post-migration) on demand instead of us
/// broadcasting an alert for every single one as it happens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PumpWatch {
    #[serde(default)]
    pub mint: String,
    pub name: String,
    pub symbol: String,
    pub first_seen: i64,
    /// Rough % of the way to migration (v_sol_in_bonding_curve / ~85 SOL),
    /// refreshed on every CurveProgress event we see for this mint.
    #[serde(default)]
    pub last_curve_pct: f64,
    /// Cached "mint authority AND freeze authority both renounced" check,
    /// refreshed alongside `last_curve_pct` -- avoids an RPC round-trip
    /// per token every time someone runs the AI Gem Scanner.
    #[serde(default)]
    pub authorities_ok: Option<bool>,
    #[serde(default)]
    pub migrated: bool,
    #[serde(default)]
    pub migrated_at: i64,
    /// Legacy fields from when we used to broadcast a message per event --
    /// no longer used to gate anything, kept only so old records on disk
    /// still deserialize cleanly.
    #[serde(default)]
    pub alerted_curve: bool,
    #[serde(default)]
    pub alerted_migration: bool,
}

#[derive(Clone)]
pub struct Db {
    inner: sled::Db,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            inner: sled::open(path)?,
        })
    }

    fn key(telegram_id: i64) -> String {
        format!("user:{telegram_id}")
    }

    /// IMPORTANT: this must NEVER return `Ok(None)` for a record that
    /// exists on disk but failed to parse. Callers (see
    /// `App::get_or_create_user`) treat `Ok(None)` as "brand new user"
    /// and will generate a FRESH WALLET -- if a parse failure were
    /// mistaken for "no user", the old wallet (and any funds in it)
    /// would be silently orphaned. So: key not found -> Ok(None). Key
    /// found but unparseable -> attempt recovery; if recovery itself
    /// fails, return Err (bot stays broken for that user rather than
    /// ever inventing a new wallet for them).
    pub fn get_user(&self, telegram_id: i64) -> Result<Option<UserRecord>> {
        match self.inner.get(Self::key(telegram_id))? {
            Some(bytes) => match serde_json::from_slice::<UserRecord>(&bytes) {
                Ok(user) => Ok(Some(user)),
                Err(e) => {
                    eprintln!(
                        "⚠️ User {telegram_id}: record failed to parse ({e}). \
                         Attempting recovery (wallet/secret preserved, in-flight UI state reset)..."
                    );
                    match Self::recover_user_record(telegram_id, &bytes) {
                        Some(recovered) => {
                            eprintln!("✅ User {telegram_id}: recovered successfully.");
                            self.save_user(&recovered)?;
                            Ok(Some(recovered))
                        }
                        None => {
                            eprintln!(
                                "🚨 User {telegram_id}: recovery FAILED -- pubkey/secret unreadable. \
                                 Record left untouched on disk. Needs manual inspection."
                            );
                            Err(anyhow!("corrupted user record for {telegram_id}: {e}"))
                        }
                    }
                }
            },
            None => Ok(None),
        }
    }

    /// Rebuilds a UserRecord field-by-field from raw JSON, tolerating any
    /// field that has changed shape or gone missing -- EXCEPT `pubkey` and
    /// `secret`, which are the only two fields that actually matter for
    /// "does this user still own their wallet". If either of those can't
    /// be read, we refuse to recover (better to stay broken than guess).
    /// Everything else (awaiting/settings/positions/etc) falls back to a
    /// safe default if it can't be parsed -- losing "what were we waiting
    /// for the user to type next" is annoying, never fund-affecting.
    fn recover_user_record(telegram_id: i64, bytes: &[u8]) -> Option<UserRecord> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;

        let pubkey = v.get("pubkey")?.as_str()?.to_string();
        let secret: EnvelopeSecret = serde_json::from_value(v.get("secret")?.clone()).ok()?;

        let pin_lockout: PinLockout = v
            .get("pin_lockout")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        let slippage_bps = v.get("slippage_bps").and_then(|x| x.as_u64()).unwrap_or(500) as u32;
        let positions: Vec<Position> = v
            .get("positions")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        let ref_code = v
            .get("ref_code")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("WRAITH_{}", telegram_id % 1_000_000));
        let refs = v.get("refs").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_i64())
            .unwrap_or_else(crate::state::chrono_now);
        let gem_alerts = v.get("gem_alerts").and_then(|x| x.as_bool()).unwrap_or(true);
        let known_withdraw_addresses: Vec<String> = v
            .get("known_withdraw_addresses")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_default();
        let subscription_expires_at = v.get("subscription_expires_at").and_then(|x| x.as_i64()).unwrap_or(0);
        let yield_principal_lamports = v.get("yield_principal_lamports").and_then(|x| x.as_u64()).unwrap_or(0);

        Some(UserRecord {
            telegram_id,
            pubkey,
            secret,
            pin_lockout,
            awaiting: Awaiting::None, // transient UI state only -- always safe to reset
            slippage_bps,
            positions,
            ref_code,
            refs,
            created_at,
            gem_alerts,
            known_withdraw_addresses,
            subscription_expires_at,
            yield_principal_lamports,
        })
    }

    pub fn save_user(&self, user: &UserRecord) -> Result<()> {
        let bytes = serde_json::to_vec(user)?;
        self.inner.insert(Self::key(user.telegram_id), bytes)?;
        self.inner.flush()?;
        Ok(())
    }

    pub fn inner_iter(&self) -> sled::Iter {
        self.inner.iter()
    }

    /// Look up a user record by their referral code (linear scan - fine for
    /// small/medium user counts, replace with an index if you scale up).
    pub fn find_by_ref_code(&self, ref_code: &str) -> Result<Option<UserRecord>> {
        for item in self.inner.iter() {
            let (_, bytes) = item?;
            if let Ok(user) = serde_json::from_slice::<UserRecord>(&bytes) {
                if user.ref_code == ref_code {
                    return Ok(Some(user));
                }
            }
        }
        Ok(None)
    }

    fn gem_key(ca: &str) -> String {
        format!("gem:{ca}")
    }

    pub fn get_gem_snapshot(&self, ca: &str) -> Result<Option<GemSnapshot>> {
        match self.inner.get(Self::gem_key(ca))? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Not flushed on every write (called once per token per scan, every
    /// 60s) — sled persists in the background, and this data isn't
    /// financially sensitive like user records are.
    pub fn save_gem_snapshot(&self, ca: &str, snap: &GemSnapshot) -> Result<()> {
        let bytes = serde_json::to_vec(snap)?;
        self.inner.insert(Self::gem_key(ca), bytes)?;
        Ok(())
    }

    fn pump_key(mint: &str) -> String {
        format!("pump:{mint}")
    }

    pub fn get_pump_watch(&self, mint: &str) -> Result<Option<PumpWatch>> {
        match self.inner.get(Self::pump_key(mint))? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Not flushed on every write -- these fire on every PumpPortal message,
    /// which can be very high volume at creation time.
    pub fn save_pump_watch(&self, mint: &str, watch: &PumpWatch) -> Result<()> {
        let bytes = serde_json::to_vec(watch)?;
        self.inner.insert(Self::pump_key(mint), bytes)?;
        Ok(())
    }
}
