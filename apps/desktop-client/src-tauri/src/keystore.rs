//! Secret-at-rest seam: the refresh token lives in the OS keyring (Windows
//! Credential Manager) in production and in a plain in-memory cell in tests,
//! so the suite never touches real credentials.

use std::sync::Mutex;

pub trait KeyStore: Send + Sync {
    fn get(&self) -> anyhow::Result<Option<String>>;
    fn set(&self, secret: &str) -> anyhow::Result<()>;
    fn delete(&self) -> anyhow::Result<()>;
}

/// Windows Credential Manager entry: service `"Dice"`, account `"default"`
/// (a fixed account name so the entry is findable before any user id is
/// known; the cache `meta` table carries the user id).
pub struct OsKeyring {
    service: String,
    account: String,
}

impl OsKeyring {
    pub fn new() -> Self {
        Self {
            service: "Dice".to_owned(),
            account: "default".to_owned(),
        }
    }

    /// A keyring entry scoped to a named dev profile, so two instances on one
    /// machine hold independent sessions (see `lib.rs` `--profile`). The
    /// default (no profile) keeps account `"default"` so existing stored
    /// sessions still resolve.
    pub fn for_profile(name: &str) -> Self {
        Self {
            service: "Dice".to_owned(),
            account: format!("profile:{name}"),
        }
    }

    fn entry(&self) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, &self.account)?)
    }
}

impl Default for OsKeyring {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyStore for OsKeyring {
    fn get(&self) -> anyhow::Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, secret: &str) -> anyhow::Result<()> {
        Ok(self.entry()?.set_password(secret)?)
    }

    fn delete(&self) -> anyhow::Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Test double (also handy for portable/dev profiles).
#[derive(Default)]
pub struct MemoryKeyStore(Mutex<Option<String>>);

impl KeyStore for MemoryKeyStore {
    fn get(&self) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().expect("keystore lock").clone())
    }

    fn set(&self, secret: &str) -> anyhow::Result<()> {
        *self.0.lock().expect("keystore lock") = Some(secret.to_owned());
        Ok(())
    }

    fn delete(&self) -> anyhow::Result<()> {
        *self.0.lock().expect("keystore lock") = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_keystore_round_trips() {
        let ks = MemoryKeyStore::default();
        assert!(ks.get().expect("get").is_none());
        ks.set("drt_secret").expect("set");
        assert_eq!(ks.get().expect("get").as_deref(), Some("drt_secret"));
        ks.delete().expect("delete");
        assert!(ks.get().expect("get").is_none());
        ks.delete().expect("idempotent delete");
    }
}
