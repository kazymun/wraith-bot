use crate::state::UserRecord;
use anyhow::Result;
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
/// Persisted so alert dedup survives restarts, same reasoning as GemSnapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PumpWatch {
    pub name: String,
    pub symbol: String,
    pub first_seen: i64,
    /// Already sent the "crossing 30% of the bonding curve" watch alert.
    pub alerted_curve: bool,
    /// Already sent the "just migrated, now tradeable" alert.
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

    pub fn get_user(&self, telegram_id: i64) -> Result<Option<UserRecord>> {
        match self.inner.get(Self::key(telegram_id))? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
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
