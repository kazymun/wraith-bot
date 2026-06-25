use crate::state::UserRecord;
use anyhow::Result;

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
}
