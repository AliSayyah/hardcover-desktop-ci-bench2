//! Where tokens live between runs.

use std::sync::Mutex;

use super::Tokens;
use crate::{Error, Result};

/// Persistent storage for [`Tokens`].
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    /// Succeeds if there was nothing to clear.
    fn clear(&self) -> Result<()>;
}

/// The desktop's secret service (GNOME Keyring, KWallet, ...) via `keyring`,
/// service `"hardcover-desktop"`. Tokens are stored as one JSON secret.
pub struct KeyringTokenStore {
    entry: keyring::Entry,
}

impl KeyringTokenStore {
    pub const SERVICE: &'static str = "hardcover-desktop";
    pub const DEFAULT_ACCOUNT: &'static str = "oauth-tokens";

    pub fn new() -> Result<Self> {
        Self::with_account(Self::DEFAULT_ACCOUNT)
    }

    pub fn with_account(account: &str) -> Result<Self> {
        Ok(Self {
            entry: keyring::Entry::new(Self::SERVICE, account).map_err(keyring_err)?,
        })
    }
}

impl std::fmt::Debug for KeyringTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringTokenStore").finish_non_exhaustive()
    }
}

fn keyring_err(e: keyring::Error) -> Error {
    // keyring errors describe the platform failure, never the secret.
    Error::Storage(format!("keyring: {e}"))
}

impl TokenStore for KeyringTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match self.entry.get_password() {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|_| Error::Storage("keyring: stored tokens are unreadable".into())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_err(e)),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens).map_err(|e| Error::Storage(e.to_string()))?;
        self.entry.set_password(&json).map_err(keyring_err)
    }

    fn clear(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_err(e)),
        }
    }
}

/// In-memory store, for tests and for "don't remember me".
#[derive(Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<Option<Tokens>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for MemoryTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTokenStore").finish_non_exhaustive()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        Ok(self
            .tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone())
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = Some(tokens.clone());
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }
}
